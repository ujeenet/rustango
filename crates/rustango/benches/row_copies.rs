//! Row copies on read paths: prefetch stitching, `values_dict`, and the
//! admin / ListView / ViewSet list pages. In-memory SQLite.
//!
//! `cargo bench -p rustango --features sqlite,admin,template_views,serializer --bench row_copies`
//! Prints allocations per call before the timings.

use std::future::Future;

use criterion::{criterion_group, criterion_main, Criterion};
use rustango::sql::{fetch_with_prefetch_pool, fetch_with_prefetch_related};
use stats_alloc::{Region, StatsAlloc, INSTRUMENTED_SYSTEM};

#[path = "../tests/support/row_copies.rs"]
mod fixture;
use fixture::{RcChild, RcParent, RcTag};

#[global_allocator]
static GLOBAL: &StatsAlloc<std::alloc::System> = &INSTRUMENTED_SYSTEM;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Run `name` once to warm up, then time it; print allocations per call.
fn bench<F, Fut>(c: &mut Criterion, rt: &tokio::runtime::Runtime, name: &str, f: F)
where
    F: Fn() -> Fut,
    Fut: Future,
{
    rt.block_on(f());
    const N: usize = 20;
    let region = Region::new(GLOBAL);
    for _ in 0..N {
        rt.block_on(f());
    }
    let stats = region.change();
    println!(
        "allocs/call {name}: {} ({} bytes)",
        (stats.allocations + stats.reallocations) / N,
        stats.bytes_allocated / N,
    );
    c.bench_function(name, |b| b.iter(|| rt.block_on(f())));
}

fn row_copies(c: &mut Criterion) {
    let rt = runtime();
    let pool = rt.block_on(fixture::seeded_pool());

    bench(c, &rt, "prefetch_fk_1k_x10", || async {
        let out: Vec<(RcParent, Vec<RcChild>)> =
            fetch_with_prefetch_pool(RcParent::objects(), "parent", &pool)
                .await
                .unwrap();
        assert_eq!(out.len(), 1_000);
    });
    bench(c, &rt, "prefetch_m2m_1k_x10", || async {
        let out: Vec<(RcParent, Vec<RcTag>)> =
            fetch_with_prefetch_related(RcParent::objects(), "tags", &pool)
                .await
                .unwrap();
        assert_eq!(out.len(), 1_000);
    });
    bench(c, &rt, "values_dict_10k", || async {
        let rows = RcChild::objects()
            .values_dict(&["id", "parent", "title", "rank"])
            .fetch(&pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 10_000);
    });

    let admin = fixture::admin_app(pool.clone());
    bench(c, &rt, "admin_list_100_2fk", || async {
        fixture::get(&admin, "/rc_post").await;
    });
    let list = fixture::list_view_app(pool.clone());
    bench(c, &rt, "listview_100_fk_display", || async {
        fixture::get(&list, "/posts").await;
    });
    let api = fixture::viewset_app(pool.clone());
    bench(c, &rt, "viewset_list_100", || async {
        fixture::get(&api, "/api/posts").await;
    });
    let api_ser = fixture::viewset_serializer_app(pool.clone());
    bench(c, &rt, "viewset_serializer_list_100", || async {
        fixture::get(&api_ser, "/api/posts").await;
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(30);
    targets = row_copies
}
criterion_main!(benches);
