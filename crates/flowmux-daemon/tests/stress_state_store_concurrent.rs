// SPDX-License-Identifier: GPL-3.0-or-later
//! Stress: concurrent mutators driving StateStore at scale.
//!
//! Marked `#[ignore]` so default `cargo test` skips it. Run with:
//!     cargo test -p flowmux-daemon --release --test stress_state_store_concurrent -- --ignored --nocapture
//!
//! Constructs a populated state via `new_lazy` so no persist-loop fires (no
//! disk writes), then spawns multiple tokio tasks that hammer the public
//! mutator API. Validates: no panic, no deadlock (timeout-bounded), workspace
//! count stays sane, every snapshot at the end is structurally valid.

use flowmux_core::{Pane, PaneContent, PaneId, SplitDirection, SurfaceId, Workspace, WorkspaceId};
use flowmux_daemon::state_store::StateStore;
use flowmux_state::State;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;

struct Xs(u64);
impl Xs {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn first_leaf_in_workspace(ws: &Workspace) -> Option<(PaneId, SurfaceId)> {
    let surface = ws.surfaces.first()?;
    fn walk(p: &Pane) -> Option<(PaneId, SurfaceId)> {
        match p {
            Pane::Leaf {
                id,
                content: PaneContent::Tabs { active, .. },
            } => Some((*id, *active)),
            Pane::Leaf { .. } => None,
            Pane::Split { first, second, .. } => walk(first).or_else(|| walk(second)),
        }
    }
    walk(&surface.root_pane)
}

fn collect_leaf_ids(p: &Pane, out: &mut Vec<PaneId>) {
    match p {
        Pane::Leaf { id, .. } => out.push(*id),
        Pane::Split { first, second, .. } => {
            collect_leaf_ids(first, out);
            collect_leaf_ids(second, out);
        }
    }
}

fn assert_state_invariants(state: &State) {
    let mut all_pane_ids: HashSet<PaneId> = HashSet::new();
    let mut all_ws_ids: HashSet<WorkspaceId> = HashSet::new();
    let mut all_surface_ids: HashSet<SurfaceId> = HashSet::new();
    for ws in &state.workspaces {
        assert!(
            all_ws_ids.insert(ws.id),
            "duplicate workspace id {:?}",
            ws.id
        );
        for surface in &ws.surfaces {
            assert!(
                all_surface_ids.insert(surface.id),
                "duplicate workspace-level surface id {:?}",
                surface.id
            );
            let mut leaves = Vec::new();
            collect_leaf_ids(&surface.root_pane, &mut leaves);
            assert!(
                !leaves.is_empty(),
                "workspace {:?} has empty pane tree",
                ws.id
            );
            for p in &leaves {
                assert!(all_pane_ids.insert(*p), "duplicate pane id {:?}", p);
            }
        }
    }
    // workspace_order, when present, must reference real workspaces (no
    // dangling ids).
    for id in &state.workspace_order {
        assert!(
            all_ws_ids.contains(id),
            "workspace_order references unknown id {:?}",
            id
        );
    }
    if let Some(active) = state.active_workspace {
        assert!(
            all_ws_ids.contains(&active),
            "active_workspace references unknown id {:?}",
            active
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "stress: concurrent mutators on StateStore"]
async fn state_store_concurrent_churn_holds_invariants() {
    const INITIAL_WORKSPACES: usize = 200;
    const TASKS: usize = 8;
    const OPS_PER_TASK: usize = 1_500;
    const OVERALL_BUDGET: Duration = Duration::from_secs(60);

    // Build with new_lazy + don't spawn persist loop -> no disk writes.
    let store = Arc::new(StateStore::new_lazy(State::default()));
    let mut ids = Vec::new();
    for i in 0..INITIAL_WORKSPACES {
        let id = store
            .create_workspace(Some(format!("ws-{i}")), PathBuf::from("/tmp"))
            .await;
        ids.push(id);
    }
    assert_eq!(store.list_workspaces().await.len(), INITIAL_WORKSPACES);

    // Snapshot once to seed each task with concrete pane/surface ids.
    let snap = store.snapshot().await;
    let mut targets: Vec<(WorkspaceId, PaneId, SurfaceId)> = Vec::new();
    for ws in &snap.workspaces {
        if let Some((pane, surface)) = first_leaf_in_workspace(ws) {
            targets.push((ws.id, pane, surface));
        }
    }
    assert_eq!(targets.len(), INITIAL_WORKSPACES);
    let targets = Arc::new(targets);

    let start = Instant::now();
    let mut handles = Vec::new();
    for task_idx in 0..TASKS {
        let store = Arc::clone(&store);
        let targets = Arc::clone(&targets);
        handles.push(tokio::spawn(async move {
            let mut rng = Xs::new(0x000A_11CE_0000 ^ (task_idx as u64));
            let mut splits_ok = 0u32;
            for op in 0..OPS_PER_TASK {
                let pick_idx = (rng.next_u64() as usize) % targets.len();
                let (ws_id, pane_id, surface_id) = targets[pick_idx];
                match rng.next_u64() % 7 {
                    0 => {
                        let _ = store
                            .rename_workspace(ws_id, format!("renamed-{task_idx}-{op}"))
                            .await;
                    }
                    1 => {
                        let _ = store
                            .update_surface_auto_title(
                                pane_id,
                                surface_id,
                                format!("title-{task_idx}-{op}"),
                            )
                            .await;
                    }
                    2 => {
                        let _ = store
                            .update_surface_cwd(
                                pane_id,
                                surface_id,
                                PathBuf::from(format!("/tmp/cwd-{task_idx}-{op}")),
                            )
                            .await;
                    }
                    3 => {
                        let _ = store.add_terminal_surface_to_pane(pane_id, None).await;
                    }
                    4 => {
                        let dir = if rng.next_u64() & 1 == 0 {
                            SplitDirection::Horizontal
                        } else {
                            SplitDirection::Vertical
                        };
                        if store.split_pane(pane_id, dir).await.is_some() {
                            splits_ok += 1;
                        }
                    }
                    5 => {
                        let _ = store.set_active_surface(pane_id, surface_id).await;
                    }
                    _ => {
                        let _ = store.surface_title(pane_id, surface_id).await;
                    }
                }
            }
            splits_ok
        }));
    }

    // Bound the wait so a deadlock would manifest as a timeout, not a hang.
    let total_splits: u32 = match timeout(OVERALL_BUDGET, async {
        let mut total = 0u32;
        for h in handles {
            total += h.await.expect("task panicked");
        }
        total
    })
    .await
    {
        Ok(v) => v,
        Err(_) => panic!(
            "concurrent stress did not complete within {OVERALL_BUDGET:?}: probable deadlock"
        ),
    };
    let elapsed = start.elapsed();

    // Final structural sanity.
    let snap = store.snapshot().await;
    assert!(
        snap.workspaces.len() >= INITIAL_WORKSPACES,
        "workspaces shrank unexpectedly: {} < {}",
        snap.workspaces.len(),
        INITIAL_WORKSPACES
    );
    assert_state_invariants(&snap);

    let total_ops = (TASKS * OPS_PER_TASK) as f64;
    eprintln!(
        "concurrent churn: {} tasks x {} ops = {} total in {:?} ({:.0} ops/s); successful splits={}",
        TASKS,
        OPS_PER_TASK,
        total_ops as u64,
        elapsed,
        total_ops / elapsed.as_secs_f64(),
        total_splits
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "stress: rapid create/remove churn"]
async fn state_store_create_remove_churn_does_not_leak_ids() {
    const ROUNDS: usize = 2_000;
    let store = Arc::new(StateStore::new_lazy(State::default()));
    let start = Instant::now();
    for round in 0..ROUNDS {
        let id = store
            .create_workspace(Some(format!("ws-{round}")), PathBuf::from("/tmp"))
            .await;
        assert!(store.remove_workspace(id).await, "remove {id:?} failed");
        // Sample every 100 rounds: count must stay at 0.
        if round % 100 == 0 {
            let n = store.list_workspaces().await.len();
            assert_eq!(n, 0, "workspaces leaked after round {round}: {n} remain");
        }
    }
    let elapsed = start.elapsed();
    let snap = store.snapshot().await;
    assert_eq!(
        snap.workspaces.len(),
        0,
        "expected empty workspaces, got {}",
        snap.workspaces.len()
    );
    assert_state_invariants(&snap);
    eprintln!(
        "create/remove churn: {ROUNDS} rounds in {elapsed:?} ({:.0} ops/s)",
        (ROUNDS * 2) as f64 / elapsed.as_secs_f64()
    );
}
