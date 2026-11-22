//! SPIKE S2 (migrations engine v2): replay and plan performance on a
//! synthetic chain. Pure, no database. Run:
//! `cargo test --release --test migrate_v2_perf_spike -- --ignored --nocapture`

#![cfg(all(
    feature = "migrate-v2-spike",
    feature = "postgres",
    feature = "mysql",
    feature = "sqlite"
))]

use std::collections::{BTreeSet, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use rustango::migrate::v2_spike::perf::*;
use rustango::migrate::v2_spike::render::{apply, Op, State};
use rustango::sql::{Dialect, MySql, Postgres, Sqlite};

/// Resident set size in bytes, from `ps` (the workspace forbids `unsafe`, so
/// no counting allocator). Coarse, but the numbers here are tens of MB.
fn live() -> usize {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<usize>()
        .expect("rss")
        * 1024
}
fn mb(b: usize) -> f64 {
    b as f64 / 1_048_576.0
}
/// Median of 5 runs.
fn time<T>(f: impl FnMut() -> T) -> (Duration, T) {
    time_n(5, f)
}
fn time_n<T>(n: usize, mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut ds = Vec::new();
    let mut last = None;
    for _ in 0..n {
        let t = Instant::now();
        let r = f();
        ds.push(t.elapsed());
        last = Some(r);
    }
    ds.sort();
    (ds[ds.len() / 2], last.unwrap())
}
fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as usize
    }
}

#[test]
#[ignore = "timing run; use --release --ignored --nocapture"]
fn s2_budgets() {
    for (label, files_per_app) in [
        ("2,000 files x 5 ops", 400usize),
        ("10,000 files x 5 ops (5x)", 2000),
    ] {
        if files_per_app > 400 && std::env::var("S2_BIG").is_err() {
            continue;
        }
        run(label, files_per_app);
    }
}

fn run(label: &str, files_per_app: usize) {
    let sh = Shape {
        apps: 5,
        files_per_app,
        tables_per_app: 40,
        adopt_tables: 15,
    };
    println!(
        "\n=== {label}: {} apps, {} files, 200 tables ===",
        sh.apps,
        sh.apps * files_per_app
    );
    let texts = generate(sh);
    let bytes: usize = texts.iter().map(String::len).sum();
    println!("chain text {:.1} MB", mb(bytes));

    // ---- stages
    let (t_parse, vals) = time(|| texts.iter().map(|t| parse(t).unwrap()).collect::<Vec<_>>());
    let (t_sum, _) = time(|| vals.iter().map(|v| checksum(v).unwrap()).count());
    let before = live();
    let files: Vec<_> = vals.iter().map(decode).collect();
    let ops: usize = files.iter().map(|f| f.ops.len()).sum();
    println!(
        "files {} ops {ops}; decoded-ops resident {:.1} MB",
        files.len(),
        mb(live().saturating_sub(before))
    );
    let (t_dec, _) = time(|| vals.iter().map(decode).count());
    let (t_dag, ord) = time(|| order(&files).unwrap());
    let all: Vec<usize> = ord.clone();

    // ---- structural sharing + memory: keep the state after every file
    let before = live();
    let mut snaps: Vec<State> = Vec::new();
    let mut st = State::default();
    for &i in &all {
        replay_onto(&mut st, &files, &[i]).unwrap();
        snaps.push(st.clone());
    }
    let shared = live().saturating_sub(before);
    let entries: usize = snaps.iter().map(|s| s.tables.len()).sum();
    let uniq: HashSet<usize> = snaps
        .iter()
        .flat_map(|s| {
            s.tables
                .values()
                .map(|t| std::sync::Arc::as_ptr(t) as usize)
        })
        .collect();
    let one = live();
    let deep: Vec<State> = snaps.iter().take(50).map(deep_clone).collect();
    let per_deep = live().saturating_sub(one) / deep.len();
    println!(
        "SHARING: {} snapshots hold {entries} table slots but {} distinct allocations; retained {:.1} MB (a deep copy is {:.2} MB each -> {:.0} MB)",
        snaps.len(), uniq.len(), mb(shared), mb(per_deep), mb(per_deep * snaps.len())
    );
    drop(deep);
    drop(snaps);
    println!("tip state {:.2} MB", mb(per_deep));

    // ---- replay (Arc state, apply in place)
    let (t_replay, tip) = time(|| {
        let mut st = State::default();
        replay_onto(&mut st, &files, &all).unwrap();
        st
    });
    // old behaviour: deep copy of the whole schema per op
    let (t_deep, _) = time_n(1, || {
        let mut st = State::default();
        for &i in &all {
            for op in &files[i].ops {
                let mut after = deep_clone(&st);
                rustango::migrate::v2_spike::render::apply(&mut after, op).unwrap();
                st = after;
            }
        }
        st
    });
    // Arc clone per op, as render_file does
    let (t_arcclone, _) = time(|| {
        let mut st = State::default();
        for &i in &all {
            for op in &files[i].ops {
                let mut after = st.clone();
                rustango::migrate::v2_spike::render::apply(&mut after, op).unwrap();
                st = after;
            }
        }
        st
    });
    println!("tip: {} tables", tip.tables.len());
    println!(
        "PARSE {:.1} ms | CHECKSUM {:.1} ms | DECODE {:.1} ms | DAG {:.1} ms",
        ms(t_parse),
        ms(t_sum),
        ms(t_dec),
        ms(t_dag)
    );
    println!(
        "REPLAY (Arc, in place) {:.1} ms | per-op Arc clone {:.1} ms | per-op DEEP clone {:.1} ms",
        ms(t_replay),
        ms(t_arcclone),
        ms(t_deep)
    );

    // ---- state hashes per file (design 2.4: before/after per migration)
    let (t_hash, _) = time(|| {
        let mut memo = HashMemo::default();
        let mut st = State::default();
        for &i in &all {
            replay_onto(&mut st, &files, &[i]).unwrap();
            memo.state_hash(&st);
        }
    });
    let (t_hash_naive, _) = time(|| table_hash_all(&tip));
    println!("STATE HASH after every file (memoised by Arc) {:.1} ms total (replay incl.); one un-memoised tip hash {:.2} ms", ms(t_hash), ms(t_hash_naive));

    // ---- fresh plan + render, per dialect
    let pg: &dyn Dialect = &Postgres;
    let my: &dyn Dialect = &MySql;
    let li: &dyn Dialect = &Sqlite;
    for (n, d) in [("postgres", pg), ("mysql", my), ("sqlite", li)] {
        let before = live();
        let (t_plan, (_, units)) = time(|| plan_onto(State::default(), &files, &all, d).unwrap());
        println!("PLAN+RENDER {n}: {:.1} ms ({units} units); render-only ~ {:.1} ms; RSS after {:+.1} MB", ms(t_plan), ms(t_plan) - ms(t_arcclone), mb(live().saturating_sub(before)));
    }
    let (t_cold, _) = time(|| {
        let vals: Vec<_> = texts.iter().map(|t| parse(t).unwrap()).collect();
        for v in &vals {
            checksum(v).unwrap();
        }
        let files: Vec<_> = vals.iter().map(decode).collect();
        let o = order(&files).unwrap();
        plan_onto(State::default(), &files, &o, pg).unwrap().1
    });
    println!(
        "COLD end to end (parse+checksum+decode+DAG+plan+render, PG) {:.1} ms",
        ms(t_cold)
    );

    if files_per_app > 400 {
        return;
    }
    // ---- 1000 tenants
    let apps = sh.apps;
    let mut rng = Rng(99);
    let heads = |max_lag: usize, rng: &mut Rng| -> Vec<usize> {
        (0..apps)
            .map(|_| files_per_app - rng.next() % max_lag)
            .collect()
    };
    // distinct sets, realistic: lag 0..3 per app, 25 distinct sets
    let mut sets: Vec<Vec<usize>> = Vec::new();
    while sets.len() < 25 {
        let h = heads(3, &mut rng);
        if !sets.contains(&h) {
            sets.push(h);
        }
    }
    let tenants_realistic: Vec<Vec<usize>> =
        (0..1000).map(|_| sets[rng.next() % 25].clone()).collect();
    let mut uniq_sets: BTreeSet<Vec<usize>> = BTreeSet::new();
    while uniq_sets.len() < 1000 {
        uniq_sets.insert(heads(8, &mut rng));
    }
    let tenants_worst: Vec<Vec<usize>> = uniq_sets.into_iter().collect();
    // position of each file inside its app chain (file ids are NNNN_step, n = idx+1)
    let pos: Vec<usize> = files
        .iter()
        .map(|f| f.id[..4].parse::<usize>().unwrap())
        .collect();
    let app_no: Vec<usize> = files
        .iter()
        .map(|f| f.app[1..].parse::<usize>().unwrap())
        .collect();
    let applied = |h: &[usize], i: usize| pos[i] <= h[app_no[i]];

    let plan_set = |h: &[usize], d: &dyn Dialect| {
        let done: Vec<usize> = all.iter().copied().filter(|&i| applied(h, i)).collect();
        let todo: Vec<usize> = all.iter().copied().filter(|&i| !applied(h, i)).collect();
        let mut st = State::default();
        replay_onto(&mut st, &files, &done).unwrap();
        plan_onto(st, &files, &todo, d).unwrap().1
    };
    // full states by canonical position, once: the prefix cache
    let mut by_pos: Vec<State> = Vec::with_capacity(all.len() + 1);
    {
        let mut st = State::default();
        by_pos.push(st.clone());
        for &i in &all {
            replay_onto(&mut st, &files, &[i]).unwrap();
            by_pos.push(st.clone());
        }
    }
    let plan_set_cached = |h: &[usize], d: &dyn Dialect| {
        let p = all
            .iter()
            .position(|&i| !applied(h, i))
            .unwrap_or(all.len());
        let mut st = by_pos[p].clone();
        let rest: Vec<usize> = all[p..]
            .iter()
            .copied()
            .filter(|&i| applied(h, i))
            .collect();
        replay_onto(&mut st, &files, &rest).unwrap();
        let todo: Vec<usize> = all.iter().copied().filter(|&i| !applied(h, i)).collect();
        plan_onto(st, &files, &todo, d).unwrap().1
    };
    // Per-app head trie: apps replay in canonical (app-major) order, so the
    // state for heads (h0..hk) extends the cached state for (h0..hk-1).
    let by_app: Vec<Vec<usize>> = (0..apps)
        .map(|a| all.iter().copied().filter(|&i| app_no[i] == a).collect())
        .collect();
    let plan_set_trie =
        |h: &[usize], d: &dyn Dialect, trie: &mut std::collections::HashMap<Vec<usize>, State>| {
            let mut st = State::default();
            for k in 0..apps {
                let key = h[..=k].to_vec();
                if let Some(c) = trie.get(&key) {
                    st = c.clone();
                } else {
                    let idx: Vec<usize> = by_app[k]
                        .iter()
                        .copied()
                        .filter(|&i| applied(h, i))
                        .collect();
                    replay_onto(&mut st, &files, &idx).unwrap();
                    trie.insert(key, st.clone());
                }
            }
            let todo: Vec<usize> = all.iter().copied().filter(|&i| !applied(h, i)).collect();
            plan_onto(st, &files, &todo, d).unwrap().1
        };
    // Reverse replay: start from the tip and undo the unapplied tail with
    // each op's inverse (ops carry `was`/`from`), so the cost is the tail.
    let tip_state = by_pos[all.len()].clone();
    let plan_set_reverse = |h: &[usize], d: &dyn Dialect| {
        let todo: Vec<usize> = all.iter().copied().filter(|&i| !applied(h, i)).collect();
        let mut st = tip_state.clone();
        for &i in todo.iter().rev() {
            for op in files[i].ops.iter().rev() {
                apply(&mut st, &inverse(op)).unwrap();
            }
        }
        plan_onto(st, &files, &todo, d).unwrap().1
    };
    let set_hash = |h: &[usize]| {
        let mut x = std::collections::hash_map::DefaultHasher::new();
        for i in 0..files.len() {
            if applied(h, i) {
                i.hash(&mut x);
            }
        }
        x.finish()
    };
    for (name, tenants) in [
        ("25 distinct sets", &tenants_realistic),
        ("1000 distinct sets (worst)", &tenants_worst),
    ] {
        let (t, (n_sets, units)) = time_n(1, || {
            let mut cache: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
            for h in tenants {
                let k = set_hash(h);
                cache.entry(k).or_insert_with(|| plan_set(h, pg));
            }
            (cache.len(), cache.values().sum::<usize>())
        });
        let (tc, (_, _)) = time_n(1, || {
            let mut cache: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
            for h in tenants {
                let k = set_hash(h);
                cache.entry(k).or_insert_with(|| plan_set_cached(h, pg));
            }
            (cache.len(), 0)
        });
        let (tt, nt) = time_n(1, || {
            let mut trie = std::collections::HashMap::new();
            let mut cache: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
            for h in tenants {
                let k = set_hash(h);
                cache
                    .entry(k)
                    .or_insert_with(|| plan_set_trie(h, pg, &mut trie));
            }
            trie.len()
        });
        let (tr, _) = time_n(1, || {
            let mut cache: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
            for h in tenants {
                let k = set_hash(h);
                cache.entry(k).or_insert_with(|| plan_set_reverse(h, pg));
            }
            cache.len()
        });
        println!("  reverse-from-tip: {:.0} ms", ms(tr));
        println!("TENANTS 1000, {name}: {n_sets} plans ({units} units): full replay {:.0} ms | canonical-prefix cache {:.0} ms | per-app head trie {:.0} ms ({nt} cached states)", ms(t), ms(tc), ms(tt));
    }
}

fn table_hash_all(st: &State) -> usize {
    st.tables.values().map(|t| table_hash(t)[0] as usize).sum()
}

fn inverse(op: &Op) -> Op {
    match op {
        Op::CreateTable { table, .. } => Op::DropTable { was: table.clone() },
        Op::AddColumn { table, column, .. } => Op::DropColumn {
            table: table.clone(),
            was: column.clone(),
        },
        Op::DropColumn { table, was } => Op::AddColumn {
            table: table.clone(),
            column: was.clone(),
            fill: None,
        },
        Op::AlterColumn { table, from, to } => Op::AlterColumn {
            table: table.clone(),
            from: to.clone(),
            to: from.clone(),
        },
        Op::AddIndex {
            index,
            concurrently,
        } => Op::DropIndex {
            was: index.clone(),
            concurrently: *concurrently,
        },
        Op::AddCheck { table, check } => Op::DropCheck {
            table: table.clone(),
            was: check.clone(),
        },
        other => panic!("no inverse for {other:?}"),
    }
}
