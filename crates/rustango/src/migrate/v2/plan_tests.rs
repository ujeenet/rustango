//! Reverse-from-tip == forward replay on random applied sets, and the
//! 4.4 budget: 1,000 tenants at 1,000 distinct applied sets plan in < 5 s
//! (release builds; a debug build checks a sample).

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use super::super::load::{load_project, Project};
use super::super::perf_tests::{write_fixture_sized, Rng};
use super::super::render::render_file;
use super::super::state::{Backend, HashMemo, Target};
use super::*;

const FLEET_BUDGET: Duration = Duration::from_secs(5);

/// Each app's chain cut at `keep[a]` files: a set the DAG allows when app
/// 0 keeps at least its first file (the others depend on it).
fn set_of(project: &Project, keep: &[usize]) -> BTreeSet<usize> {
    project
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            let a: usize = f.file.app[1..].parse().unwrap();
            let n: usize = f.file.id[..4].parse().unwrap();
            n <= keep[a]
        })
        .map(|(i, _)| i)
        .collect()
}

#[test]
fn reverse_from_tip_matches_forward_replay() {
    let tmp = tempfile::tempdir().unwrap();
    write_fixture_sized(tmp.path(), 3, 60, 8);
    let project = load_project(&tmp.path().join("migrations")).unwrap();
    let planner = TipPlanner::new(&project, Target::Tenant).unwrap();
    let mut rng = Rng(11);
    let mut memo = HashMemo::default();
    for _ in 0..60 {
        let mut keep: Vec<usize> = (0..3).map(|_| rng.next(61)).collect();
        if keep[1..].iter().any(|&k| k > 0) {
            keep[0] = keep[0].max(1);
        }
        let set = set_of(&project, &keep);
        let rev = planner.reverse(&project, &set).expect("every op inverts");
        let fwd = planner.forward(&project, &set).unwrap();
        assert_eq!(rev.hash(&mut memo), fwd.hash(&mut memo), "{keep:?}");
    }
}

#[test]
fn a_thousand_tenants_at_distinct_sets_plan_within_budget() {
    let tmp = tempfile::tempdir().unwrap();
    write_fixture_sized(tmp.path(), 4, 500, 40);
    let project = load_project(&tmp.path().join("migrations")).unwrap();
    // 1,000 distinct sets: each app 0..=5 files short of its tip.
    let mut sets: Vec<Vec<usize>> = Vec::new();
    'all: for a in 0..6 {
        for b in 0..6 {
            for c in 0..6 {
                for d in 0..6 {
                    if sets.len() == 1000 {
                        break 'all;
                    }
                    sets.push(vec![500 - a, 500 - b, 500 - c, 500 - d]);
                }
            }
        }
    }
    let n = if cfg!(debug_assertions) {
        40
    } else {
        sets.len()
    };
    let started = Instant::now();
    let planner = TipPlanner::new(&project, Target::Tenant).unwrap();
    let order = project.order[&Target::Tenant].clone();
    let mut statements = 0;
    for keep in &sets[..n] {
        let set = set_of(&project, keep);
        let mut state = (*planner.state_at(&project, &set).unwrap()).clone();
        for &i in order.iter().filter(|i| !set.contains(i)) {
            let f = &project.files[i];
            let ctx = Ctx {
                target: Target::Tenant,
                registry: None,
            };
            let (units, after) =
                render_file(&state, &f.file, &f.at, &ctx, Backend::Postgres).unwrap();
            statements += units.iter().map(|u| u.sql.len()).sum::<usize>();
            state = after;
        }
    }
    let took = started.elapsed();
    eprintln!("planned {n} distinct applied sets ({statements} statements) in {took:?}");
    if !cfg!(debug_assertions) {
        assert!(took < FLEET_BUDGET, "{n} sets took {took:?}");
    }
}
