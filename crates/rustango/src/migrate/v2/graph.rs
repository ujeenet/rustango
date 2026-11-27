//! One DAG per target. Edges: `parents` (same app) and `depends_on`
//! (`<app>:<id>`). The canonical order is topological, ties by `(app, id)`;
//! the file name is never the order.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap};

use super::state::Target;
use super::{Code, LoadError};

/// What the graph needs of a file.
pub(crate) struct Node<'a> {
    pub(crate) app: &'a str,
    pub(crate) id: &'a str,
    pub(crate) target: Target,
    pub(crate) parents: &'a [String],
    pub(crate) depends_on: &'a [(String, String)],
    /// The file, for errors.
    pub(crate) at: &'a str,
}

/// Validate one target's nodes and return their canonical order (indexes
/// into `nodes`).
///
/// # Errors
/// [`Code::Graph`]: a duplicate id (both files named), a missing parent, a
/// cycle (its path printed) or more than one leaf per app.
pub(crate) fn order(nodes: &[Node<'_>]) -> Result<Vec<usize>, LoadError> {
    let mut by_key: HashMap<(&str, &str), usize> = HashMap::with_capacity(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        if let Some(j) = by_key.insert((n.app, n.id), i) {
            return Err(LoadError::new(
                Code::Graph,
                n.at,
                format!("duplicate id `{}:{}`, also in {}", n.app, n.id, nodes[j].at),
            ));
        }
    }
    let mut indegree = vec![0usize; nodes.len()];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    // Children through `parents` only: they decide leaves per app.
    let mut has_child = vec![false; nodes.len()];
    for (i, n) in nodes.iter().enumerate() {
        let parents = n.parents.iter().map(|p| (n.app, p.as_str(), true));
        let deps = n
            .depends_on
            .iter()
            .map(|(a, id)| (a.as_str(), id.as_str(), false));
        for (app, id, same_app) in parents.chain(deps) {
            let Some(&p) = by_key.get(&(app, id)) else {
                return Err(LoadError::new(
                    Code::Graph,
                    n.at,
                    format!(
                        "`{app}:{id}` is not a {} migration of this project",
                        n.target.as_str()
                    ),
                ));
            };
            children[p].push(i);
            indegree[i] += 1;
            has_child[p] |= same_app;
        }
    }
    let mut leaves: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, n) in nodes.iter().enumerate() {
        if !has_child[i] {
            leaves.entry(n.app).or_default().push(i);
        }
    }
    if let Some((app, ids)) = leaves.iter().find(|(_, v)| v.len() > 1) {
        let names: Vec<&str> = ids.iter().map(|&i| nodes[i].id).collect();
        return Err(LoadError::new(
            Code::Graph,
            nodes[ids[0]].at,
            format!(
                "app `{app}` has {} leaves ({}); merge them with a migration whose parents \
                 are all of them, or rebase one onto the other",
                ids.len(),
                names.join(", ")
            ),
        ));
    }
    let mut ready: BinaryHeap<Reverse<(&str, &str, usize)>> = indegree
        .iter()
        .enumerate()
        .filter(|(_, d)| **d == 0)
        .map(|(i, _)| Reverse((nodes[i].app, nodes[i].id, i)))
        .collect();
    let mut out = Vec::with_capacity(nodes.len());
    while let Some(Reverse((_, _, i))) = ready.pop() {
        out.push(i);
        for &c in &children[i] {
            indegree[c] -= 1;
            if indegree[c] == 0 {
                ready.push(Reverse((nodes[c].app, nodes[c].id, c)));
            }
        }
    }
    if out.len() < nodes.len() {
        let path = cycle(nodes, &children, &indegree);
        return Err(LoadError::new(
            Code::Graph,
            nodes[path[0]].at,
            format!(
                "cycle: {}",
                path.iter()
                    .map(|&i| format!("{}:{}", nodes[i].app, nodes[i].id))
                    .collect::<Vec<_>>()
                    .join(" -> ")
            ),
        ));
    }
    Ok(out)
}

/// One cycle among the nodes the sort could not place, in edge order, the
/// first node repeated at the end.
fn cycle(nodes: &[Node<'_>], children: &[Vec<usize>], indegree: &[usize]) -> Vec<usize> {
    let stuck = |i: usize| indegree[i] > 0;
    let mut parents: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (p, cs) in children.iter().enumerate() {
        for &c in cs {
            parents[c].push(p);
        }
    }
    // A stuck node always has a stuck parent, so walking parents repeats.
    let start = (0..nodes.len())
        .find(|&i| stuck(i))
        .expect("a cycle exists");
    let mut seen: HashMap<usize, usize> = HashMap::new();
    let mut path = vec![start];
    let mut at = start;
    loop {
        seen.insert(at, path.len() - 1);
        let next = *parents[at]
            .iter()
            .find(|&&p| stuck(p))
            .expect("a stuck node has a stuck parent");
        if let Some(&pos) = seen.get(&next) {
            let mut cyc = path[pos..].to_vec();
            cyc.push(next);
            cyc.reverse();
            return cyc;
        }
        path.push(next);
        at = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Spec {
        app: &'static str,
        id: &'static str,
        parents: Vec<String>,
        deps: Vec<(String, String)>,
    }

    fn spec(app: &'static str, id: &'static str, parents: &[&str], deps: &[(&str, &str)]) -> Spec {
        Spec {
            app,
            id,
            parents: parents.iter().map(|p| (*p).to_owned()).collect(),
            deps: deps
                .iter()
                .map(|(a, i)| ((*a).to_owned(), (*i).to_owned()))
                .collect(),
        }
    }

    fn run(specs: &[Spec]) -> Result<Vec<String>, LoadError> {
        let nodes: Vec<Node<'_>> = specs
            .iter()
            .map(|s| Node {
                app: s.app,
                id: s.id,
                target: Target::Tenant,
                parents: &s.parents,
                depends_on: &s.deps,
                at: s.id,
            })
            .collect();
        order(&nodes).map(|o| {
            o.iter()
                .map(|&i| format!("{}:{}", specs[i].app, specs[i].id))
                .collect()
        })
    }

    /// Ties break by `(app, id)`; the name is not the order.
    #[test]
    fn canonical_order() {
        let got = run(&[
            spec("b", "0002_x", &["0009_root"], &[]),
            spec("b", "0009_root", &[], &[("a", "0001_a")]),
            spec("a", "0001_a", &[], &[]),
            spec("a", "0002_b", &["0001_a"], &[]),
        ])
        .unwrap();
        assert_eq!(got, ["a:0001_a", "a:0002_b", "b:0009_root", "b:0002_x"]);
    }

    #[test]
    fn two_leaves_are_refused() {
        let e = run(&[
            spec("a", "0001_a", &[], &[]),
            spec("a", "0002_b", &["0001_a"], &[]),
            spec("a", "0002_c", &["0001_a"], &[]),
        ])
        .unwrap_err();
        assert_eq!(e.code, Code::Graph);
        assert!(e.msg.contains("2 leaves (0002_b, 0002_c)"), "{e}");
        // A merge node joins them.
        run(&[
            spec("a", "0001_a", &[], &[]),
            spec("a", "0002_b", &["0001_a"], &[]),
            spec("a", "0002_c", &["0001_a"], &[]),
            spec("a", "0003_merge", &["0002_b", "0002_c"], &[]),
        ])
        .unwrap();
    }

    #[test]
    fn a_cycle_prints_its_path() {
        let e = run(&[
            spec("a", "0001_a", &["0003_c"], &[]),
            spec("a", "0002_b", &["0001_a"], &[]),
            spec("a", "0003_c", &["0002_b"], &[]),
            spec("a", "0004_d", &["0003_c"], &[]),
        ])
        .unwrap_err();
        assert_eq!(e.code, Code::Graph);
        assert!(e.msg.starts_with("cycle: "), "{e}");
        for id in ["0001_a", "0002_b", "0003_c"] {
            assert!(e.msg.contains(id), "{e}");
        }
        assert!(!e.msg.contains("0004_d"), "{e}");
    }

    #[test]
    fn cross_app_cycle() {
        let e = run(&[
            spec("a", "0001_a", &[], &[("b", "0001_b")]),
            spec("b", "0001_b", &[], &[("a", "0001_a")]),
        ])
        .unwrap_err();
        assert!(
            e.msg.contains("a:0001_a") && e.msg.contains("b:0001_b"),
            "{e}"
        );
    }

    #[test]
    fn missing_parent_and_duplicate_id() {
        let e = run(&[spec("a", "0002_b", &["0001_a"], &[])]).unwrap_err();
        assert_eq!(e.code, Code::Graph);
        let e = run(&[spec("a", "0001_a", &[], &[]), spec("a", "0001_a", &[], &[])]).unwrap_err();
        assert!(e.msg.contains("duplicate"), "{e}");
    }
}
