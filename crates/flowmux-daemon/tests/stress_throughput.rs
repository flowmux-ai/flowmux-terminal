// SPDX-License-Identifier: GPL-3.0-or-later
//! Stress: throughput / latency probes for hot StateStore paths.
//!
//! Marked `#[ignore]`. Run with:
//!     cargo test -p flowmux-daemon --release --test stress_throughput -- --ignored --nocapture
//!
//! These tests check three things:
//!
//! 1. **Hot-loop throughput** -- 20k `update_surface_auto_title` calls on a
//!    single-pane workspace. Asserts the loop finishes within a generous
//!    wall-clock budget so a future N^2 or lock-contention regression trips
//!    the test.
//! 2. **No-op fast path** -- repeating the same title is detected as
//!    unchanged and the second half of a same-title burst stays at least as
//!    fast as the first.
//! 3. **Linear, not quadratic, scaling in workspace count** -- driving 2k
//!    title updates with 1000 workspaces present should be slower than with
//!    10 workspaces present (the lookup walks workspaces), but not orders
//!    of magnitude worse than that linear factor would predict.

use flowmux_core::{Pane, PaneContent, SurfaceId, Workspace};
use flowmux_daemon::state_store::StateStore;
use flowmux_state::State;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn first_pane_surface(ws: &Workspace) -> Option<(flowmux_core::PaneId, SurfaceId)> {
    let s = ws.surfaces.first()?;
    fn walk(p: &Pane) -> Option<(flowmux_core::PaneId, SurfaceId)> {
        match p {
            Pane::Leaf {
                id,
                content: PaneContent::Tabs { active, .. },
            } => Some((*id, *active)),
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => walk(first).or_else(|| walk(second)),
        }
    }
    walk(&s.root_pane)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "stress: throughput burst"]
async fn auto_title_burst_meets_throughput_budget() {
    const OPS: usize = 20_000;
    const BUDGET: Duration = Duration::from_secs(30);

    let store = StateStore::new_lazy(State::default());
    let ws_id = store
        .create_workspace(Some("hot".into()), PathBuf::from("/tmp"))
        .await;
    let snap = store.snapshot().await;
    let ws = snap.workspaces.iter().find(|w| w.id == ws_id).unwrap();
    let (pane, surface) = first_pane_surface(ws).expect("fresh workspace must have a leaf");

    let start = Instant::now();
    for i in 0..OPS {
        let _ = store
            .update_surface_auto_title(pane, surface, format!("title-{i}"))
            .await;
    }
    let elapsed = start.elapsed();
    let per_op_ns = elapsed.as_nanos() as f64 / OPS as f64;
    let ops_per_sec = OPS as f64 / elapsed.as_secs_f64();

    eprintln!(
        "auto_title burst: {OPS} ops in {elapsed:?} ({ops_per_sec:.0} ops/s, {per_op_ns:.0} ns/op)"
    );
    assert!(
        elapsed < BUDGET,
        "auto_title burst {OPS} ops took {elapsed:?}, budget {BUDGET:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "stress: no-op burst path"]
async fn auto_title_repeat_is_a_fast_path() {
    // Repeating the same title should hit the "unchanged" branch and stay
    // cheap. We measure the second half against the first: it should be
    // strictly cheaper or roughly equal, never noticeably slower.
    const HALF: usize = 10_000;
    let store = StateStore::new_lazy(State::default());
    let ws_id = store
        .create_workspace(Some("hot".into()), PathBuf::from("/tmp"))
        .await;
    let snap = store.snapshot().await;
    let ws = snap.workspaces.iter().find(|w| w.id == ws_id).unwrap();
    let (pane, surface) = first_pane_surface(ws).unwrap();

    // Prime: set the title once.
    let _ = store
        .update_surface_auto_title(pane, surface, "Same".into())
        .await;

    let t0 = Instant::now();
    for _ in 0..HALF {
        let _ = store
            .update_surface_auto_title(pane, surface, "Same".into())
            .await;
    }
    let first = t0.elapsed();
    let t1 = Instant::now();
    for _ in 0..HALF {
        let _ = store
            .update_surface_auto_title(pane, surface, "Same".into())
            .await;
    }
    let second = t1.elapsed();
    eprintln!("auto_title same-value: first half {first:?}, second half {second:?} ({HALF} each)");
    // Loose envelope: second must be within 4x of first. Catches a regression
    // that turns the no-op path quadratic without false-firing on noisy CI.
    assert!(
        second <= first * 4 + Duration::from_millis(200),
        "second-half ({second:?}) ran much slower than first ({first:?})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "stress: scaling probe across workspace count"]
async fn auto_title_scales_with_workspace_count_linearly() {
    // Lookup walks workspaces, so we expect roughly linear slowdown when N
    // grows. Assert it's not dramatically worse (e.g. quadratic).
    const SMALL: usize = 10;
    const LARGE: usize = 1_000;
    const OPS: usize = 2_000;

    async fn run(n_ws: usize, ops: usize) -> Duration {
        let store = StateStore::new_lazy(State::default());
        let mut last = None;
        for i in 0..n_ws {
            let id = store
                .create_workspace(Some(format!("ws-{i}")), PathBuf::from("/tmp"))
                .await;
            last = Some(id);
        }
        // Use the *last* workspace for ops so the linear walk is worst-case.
        let id = last.unwrap();
        let snap = store.snapshot().await;
        let ws = snap.workspaces.iter().find(|w| w.id == id).unwrap();
        let (pane, surface) = first_pane_surface(ws).unwrap();
        let start = Instant::now();
        for i in 0..ops {
            let _ = store
                .update_surface_auto_title(pane, surface, format!("t-{i}"))
                .await;
        }
        start.elapsed()
    }

    let small = run(SMALL, OPS).await;
    let large = run(LARGE, OPS).await;
    let factor = LARGE as f64 / SMALL as f64;
    let observed = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
    eprintln!(
        "scaling: ws=10 took {small:?}, ws=1000 took {large:?}; \
         expected ~{factor:.0}x linear slowdown, observed {observed:.1}x"
    );
    // Allow a 4x cushion above the linear expectation. A quadratic regression
    // (1000^2 / 10^2 = 10000x) trips this; a normal linear fan-out (~100x)
    // does not.
    assert!(
        observed < factor * 4.0,
        "scaling looks worse than linear: {observed:.1}x vs ~{factor:.0}x expected"
    );
}
