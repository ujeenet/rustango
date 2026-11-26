//! The fleet planner (4.2, 8.3): the state at a tenant's applied set,
//! computed once per distinct set by undoing from the tip, newest first.
//! Forward replay stays the reference, and the fallback below a checkpoint
//! or for an op with no state inverse.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use super::format::{Kind, Op};
use super::invert::{invert, Allow};
use super::load::Project;
use super::replay::{apply, apply_file, Ctx};
use super::state::{State, Target};
use super::LoadError;

/// One target's tip state, and the states planned from it.
pub(crate) struct TipPlanner {
    target: Target,
    /// The project's file ids, so a run on another load is not served.
    ids: Vec<(String, String)>,
    order: Vec<usize>,
    tip: State,
    planned: Mutex<HashMap<BTreeSet<usize>, Arc<State>>>,
}

impl std::fmt::Debug for TipPlanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TipPlanner")
            .field("target", &self.target)
            .field("files", &self.order.len())
            .finish_non_exhaustive()
    }
}

fn ctx(target: Target) -> Ctx<'static> {
    Ctx {
        target,
        registry: None,
    }
}

impl TipPlanner {
    /// Replay `target`'s chain forward once: the tip.
    ///
    /// # Errors
    /// A file that does not replay.
    pub(crate) fn new(project: &Project, target: Target) -> Result<Self, LoadError> {
        let order = project.order.get(&target).cloned().unwrap_or_default();
        let mut tip = State::default();
        for &i in &order {
            let f = &project.files[i];
            apply_file(&mut tip, &f.file, &f.at, &ctx(target))?;
        }
        Ok(Self {
            target,
            ids: ids(project),
            order,
            tip,
            planned: Mutex::new(HashMap::new()),
        })
    }

    /// Whether this planner was built from `project`'s files.
    pub(crate) fn serves(&self, project: &Project) -> bool {
        self.ids == ids(project)
    }

    /// The state after exactly the files in `applied` (indices into the
    /// project), memoised per set.
    ///
    /// # Errors
    /// A file that does not replay forward.
    pub(crate) fn state_at(
        &self,
        project: &Project,
        applied: &BTreeSet<usize>,
    ) -> Result<Arc<State>, LoadError> {
        if let Some(s) = self.lock().get(applied) {
            return Ok(s.clone());
        }
        let s = match self.reverse(project, applied) {
            Some(s) => s,
            None => self.forward(project, applied)?,
        };
        let s = Arc::new(s);
        self.lock().insert(applied.clone(), s.clone());
        Ok(s)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<BTreeSet<usize>, Arc<State>>> {
        self.planned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Undo every file outside `applied` from the tip, newest first; `None`
    /// when one has no state inverse (a checkpoint, a refused inverse).
    fn reverse(&self, project: &Project, applied: &BTreeSet<usize>) -> Option<State> {
        let mut s = self.tip.clone();
        for &i in self.order.iter().rev() {
            if applied.contains(&i) {
                continue;
            }
            let f = &project.files[i];
            if f.file.kind == Kind::Checkpoint {
                return None;
            }
            for (k, env) in f.file.operations.iter().enumerate().rev() {
                let at = format!("{}#/operations/{k}", f.at);
                undo(&mut s, &env.op, &at, self.target).ok()?;
            }
        }
        Some(s)
    }

    /// The reference: replay the files in `applied`, in order.
    fn forward(&self, project: &Project, applied: &BTreeSet<usize>) -> Result<State, LoadError> {
        let mut s = State::default();
        for &i in self.order.iter().filter(|i| applied.contains(i)) {
            let f = &project.files[i];
            apply_file(&mut s, &f.file, &f.at, &ctx(self.target))?;
        }
        Ok(s)
    }
}

fn ids(project: &Project) -> Vec<(String, String)> {
    project
        .files
        .iter()
        .map(|f| (f.file.app.clone(), f.file.id.clone()))
        .collect()
}

/// `op`'s state inverse on `s`, the state after it: data steps change no
/// schema state; `raw_sql` and `state_only` undo their declared state.
fn undo(s: &mut State, op: &Op, at: &str, target: Target) -> Result<(), LoadError> {
    match op {
        Op::DataSql { .. } | Op::DataRust { .. } => Ok(()),
        Op::RawSql { .. } | Op::StateOnly { .. } => {
            for (i, nested) in op.nested(at)?.iter().enumerate().rev() {
                undo(s, nested, &format!("{at}/state/{i}"), target)?;
            }
            Ok(())
        }
        _ => {
            for inv in invert(op, s, Allow { data_loss: true }, at)? {
                apply(s, &inv, at, &ctx(target), &[])?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
