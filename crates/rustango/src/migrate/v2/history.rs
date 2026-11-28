//! The ledger as rollback and drift read it (7.1, item 28): rows in `seq`
//! order, except a squash checkpoint recorded on a database that ran rows
//! it replaces. That one is a marker: it sits right after the last row it
//! covers and is never a step of its own.

use std::collections::BTreeSet;

use super::format::{File, Kind};
use super::ledger::Row;
use super::replay::{apply_file, Ctx};
use super::state::State;
use super::{Code, LoadError};

/// Ledger rows with their stored bodies, in planning order.
pub(crate) struct History<'r> {
    pub(crate) rows: Vec<&'r Row>,
    pub(crate) bodies: Vec<File>,
    /// Per position: the markers (positions) whose checkpoint covers it.
    covered_by: Vec<Vec<usize>>,
    marker: Vec<bool>,
}

/// The file a ledger row ran: its stored body, decoded.
///
/// # Errors
/// A body that does not decode.
pub(crate) fn body(row: &Row) -> Result<File, LoadError> {
    let at = format!("ledger:{}:{}", row.app, row.id);
    let json: serde_json::Value = serde_json::from_str(&row.applied_body)
        .map_err(|e| LoadError::new(Code::File, &at, e.to_string()))?;
    super::format::decode(&json, &at)
}

/// Whether checkpoint `c` stands for `f`: a file it replaces, or an older
/// squash checkpoint whose replaced files it replaces too.
fn covers(c: &File, f: &File) -> bool {
    if c.kind != Kind::Checkpoint || c.replaces.is_empty() || f.app != c.app || f.id == c.id {
        return false;
    }
    c.replaces.contains(&f.id)
        || (f.kind == Kind::Checkpoint
            && !f.replaces.is_empty()
            && f.replaces.iter().all(|r| c.replaces.contains(r)))
}

impl<'r> History<'r> {
    /// `rows` in `seq` order, their bodies decoded.
    ///
    /// # Errors
    /// A stored body that does not decode.
    pub(crate) fn read(rows: Vec<&'r Row>) -> Result<Self, LoadError> {
        let bodies = rows.iter().map(|r| body(r)).collect::<Result<_, _>>()?;
        Ok(Self::new(rows, bodies))
    }

    fn new(rows: Vec<&'r Row>, bodies: Vec<File>) -> Self {
        let n = rows.len();
        let cov: Vec<Vec<usize>> = (0..n)
            .map(|c| (0..n).filter(|&i| covers(&bodies[c], &bodies[i])).collect())
            .collect();
        // Each marker follows the last plain row it covers (inner ones first).
        let mut anchor: Vec<Option<usize>> = vec![None; n];
        let mut markers: Vec<usize> = (0..n).filter(|&c| !cov[c].is_empty()).collect();
        markers.sort_by_key(|&c| (bodies[c].replaces.len(), c));
        for &c in &markers {
            anchor[c] = cov[c].iter().map(|&i| anchor[i].unwrap_or(i)).max();
        }
        let mut order = Vec::with_capacity(n);
        for i in (0..n).filter(|&i| anchor[i].is_none()) {
            order.push(i);
            order.extend(markers.iter().copied().filter(|&c| anchor[c] == Some(i)));
        }
        let pos_of = |seq: usize| order.iter().position(|&o| o == seq).expect("placed");
        let mut covered_by = vec![Vec::new(); n];
        for &c in &markers {
            for &i in &cov[c] {
                covered_by[pos_of(i)].push(pos_of(c));
            }
        }
        let marker = order.iter().map(|&o| anchor[o].is_some()).collect();
        let mut slots: Vec<Option<File>> = bodies.into_iter().map(Some).collect();
        Self {
            rows: order.iter().map(|&o| rows[o]).collect(),
            bodies: order
                .iter()
                .map(|&o| slots[o].take().expect("once"))
                .collect(),
            covered_by,
            marker,
        }
    }

    /// Only the rows at positions `keep` (ascending).
    pub(crate) fn keep(&mut self, keep: &[usize]) {
        let at = |p: usize| keep.iter().position(|&k| k == p);
        self.covered_by = keep
            .iter()
            .map(|&k| self.covered_by[k].iter().filter_map(|&m| at(m)).collect())
            .collect();
        self.marker = keep.iter().map(|&k| self.marker[k]).collect();
        self.rows = keep.iter().map(|&k| self.rows[k]).collect();
        let mut slots: Vec<Option<File>> = std::mem::take(&mut self.bodies)
            .into_iter()
            .map(Some)
            .collect();
        self.bodies = keep
            .iter()
            .map(|&k| slots[k].take().expect("once"))
            .collect();
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether position `p` is a squash checkpoint recorded over rows it covers.
    pub(crate) fn is_marker(&self, p: usize) -> bool {
        self.marker[p]
    }

    /// The position of `target` (`<app>:<id>` or `<id>`).
    pub(crate) fn position(&self, target: &str) -> Option<usize> {
        self.rows
            .iter()
            .position(|r| super::run::names(target, &r.app, &r.id))
    }

    /// State after the rows before position `p`; a row a marker earlier
    /// than `p` covers is in that checkpoint's state, so it is skipped.
    ///
    /// # Errors
    /// A stored body that does not replay.
    pub(crate) fn state_before(&self, p: usize, ctx: &Ctx<'_>) -> Result<State, LoadError> {
        let mut state = State::default();
        for j in 0..p {
            if self.covered_by[j].iter().any(|&m| m < p) {
                continue;
            }
            let at = format!("ledger:{}:{}", self.rows[j].app, self.rows[j].id);
            apply_file(&mut state, &self.bodies[j], &at, ctx)?;
        }
        Ok(state)
    }

    /// The last `n` steps, newest first: markers do not count, and come
    /// along with the rows they cover.
    pub(crate) fn last_steps(&self, n: usize) -> Vec<usize> {
        let mut chosen: Vec<usize> = (0..self.len())
            .rev()
            .filter(|&p| !self.marker[p])
            .take(n)
            .collect();
        let markers: BTreeSet<usize> = chosen
            .iter()
            .flat_map(|&p| self.covered_by[p].iter().copied())
            .collect();
        chosen.extend(markers);
        chosen.sort_unstable_by(|a, b| b.cmp(a));
        chosen
    }

    /// The rows after `pos` that descend from it over `parents` (same app)
    /// and `depends_on` (any app), oldest first (7.1 step 1). A checkpoint
    /// stands for the ids it replaces, and a marker comes along with them.
    pub(crate) fn descendants(&self, pos: usize) -> Vec<usize> {
        let mut set: Vec<(String, String)> = Vec::new();
        let add = |set: &mut Vec<(String, String)>, f: &File| {
            set.push((f.app.clone(), f.id.clone()));
            set.extend(f.replaces.iter().map(|r| (f.app.clone(), r.clone())));
        };
        add(&mut set, &self.bodies[pos]);
        let mut chosen: Vec<usize> = Vec::new();
        for i in pos + 1..self.len() {
            let f = &self.bodies[i];
            let hit = f
                .parents
                .iter()
                .any(|p| set.contains(&(f.app.clone(), p.clone())))
                || f.depends_on.iter().any(|d| set.contains(d))
                || (self.marker[i]
                    && (pos..i).any(|j| {
                        (j == pos || chosen.contains(&j)) && self.covered_by[j].contains(&i)
                    }));
            if hit {
                add(&mut set, f);
                chosen.push(i);
            }
        }
        chosen
    }

    /// The newest row left after rolling back `steps` steps.
    #[cfg(feature = "tenancy")]
    pub(crate) fn head_after(&self, steps: usize) -> Option<String> {
        let gone = self.last_steps(steps);
        let first = gone.last().copied().unwrap_or(self.len());
        (0..first)
            .next_back()
            .map(|p| format!("{}:{}", self.rows[p].app, self.rows[p].id))
    }

    /// The steps that leave `head` the newest row (`None`: no row); `0`
    /// once it is, or once `head` is gone.
    #[cfg(feature = "tenancy")]
    pub(crate) fn steps_to(&self, head: Option<&str>) -> usize {
        let after = match head {
            None => 0,
            Some(h) => match self.position(h) {
                Some(p) => p + 1,
                None => return 0,
            },
        };
        (after..self.len()).filter(|&p| !self.marker[p]).count()
    }

    /// The steps applied after `target`, newest last: `None` when it is
    /// not applied.
    pub(crate) fn after(&self, target: &str) -> Option<Vec<String>> {
        let pos = self.position(target)?;
        Some(
            (pos + 1..self.len())
                .filter(|&p| !self.marker[p])
                .map(|p| format!("{}:{}", self.rows[p].app, self.rows[p].id))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: &str, body: serde_json::Value) -> Row {
        Row {
            app: "project".into(),
            id: id.into(),
            checksum: String::new(),
            applied_body: body.to_string(),
            state: super::super::ledger::RowState::Applied,
            unit_plan: Vec::new(),
            pos: 0,
            started_unit: None,
            run_id: String::new(),
            error_class: None,
            session: None,
        }
    }

    fn mig(id: &str, parents: &[&str]) -> Row {
        row(
            id,
            json!({"engine": 2, "format": 1, "id": id, "app": "project", "target": "tenant",
                   "parents": parents, "operations": []}),
        )
    }

    fn squash(id: &str, replaces: &[&str]) -> Row {
        row(
            id,
            json!({"engine": 2, "format": 1, "kind": "checkpoint", "id": id, "app": "project",
                   "target": "tenant", "replaces": replaces, "state": {}, "operations": []}),
        )
    }

    /// A checkpoint recorded last over the rows it replaces plans as if it
    /// ran in their place: never a step, `--to` it rolls back what follows.
    #[test]
    fn a_squash_marker_sits_after_what_it_replaces() {
        let rows = [
            mig("0001_a", &[]),
            mig("0002_b", &["0001_a"]),
            mig("0003_c", &["0002_b"]),
            squash("0002_squash", &["0001_a", "0002_b"]),
        ];
        let h = History::read(rows.iter().collect()).unwrap();
        let ids: Vec<&str> = h.rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["0001_a", "0002_b", "0002_squash", "0003_c"]);
        assert_eq!(h.last_steps(1), [3]);
        assert_eq!(
            h.last_steps(2),
            [3, 2, 1],
            "the marker goes with what it covers"
        );
        assert_eq!(h.descendants(2), [3]);
        assert_eq!(h.after("0002_squash").unwrap(), ["project:0003_c"]);
        #[cfg(feature = "tenancy")]
        {
            assert_eq!(h.head_after(1).as_deref(), Some("project:0002_squash"));
            assert_eq!(h.steps_to(Some("project:0002_squash")), 1);
        }
        assert_eq!(h.descendants(0), [1, 2, 3]);
    }
}
