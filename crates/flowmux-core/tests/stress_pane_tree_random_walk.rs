// SPDX-License-Identifier: GPL-3.0-or-later
//! Stress: deterministic random walk over the Pane tree.
//!
//! Marked `#[ignore]` so the default suite stays quick. Run with:
//!     cargo test -p flowmux-core --release --test stress_pane_tree_random_walk -- --ignored --nocapture

use flowmux_core::{
    Pane, PaneContent, PaneId, PaneSurface, RemoveOutcome, SplitDirection, SurfaceId,
};
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Tiny seeded xorshift64 PRNG. Inline to avoid pulling `rand` into the
/// workspace just for stress tests.
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
    fn pick<T: Copy>(&mut self, slice: &[T]) -> T {
        slice[(self.next_u64() as usize) % slice.len()]
    }
}

fn collect_leaves(p: &Pane) -> Vec<PaneId> {
    let mut v = Vec::new();
    p.for_each_leaf(|id| v.push(id));
    v
}

fn tree_depth(p: &Pane) -> u32 {
    match p {
        Pane::Leaf { .. } => 1,
        Pane::Split { first, second, .. } => tree_depth(first).max(tree_depth(second)) + 1,
    }
}

fn check_invariants(root: &Pane, op_index: usize, max_depth: u32) {
    let leaves = collect_leaves(root);
    assert!(
        !leaves.is_empty(),
        "tree became empty after op {op_index} (root collapse not allowed in stress harness)"
    );

    let unique: HashSet<&PaneId> = leaves.iter().collect();
    assert_eq!(
        leaves.len(),
        unique.len(),
        "duplicate leaf ids after op {op_index}"
    );

    assert!(
        root.first_leaf_id().is_some(),
        "first_leaf_id() = None despite {} leaves",
        leaves.len()
    );

    fn rec(p: &Pane, op_index: usize) {
        match p {
            Pane::Leaf {
                id,
                content: PaneContent::Tabs { active, surfaces },
            } => {
                assert!(
                    !surfaces.is_empty(),
                    "Tabs with empty surfaces at leaf {id:?} after op {op_index}"
                );
                assert!(
                    surfaces.iter().any(|s| s.id == *active),
                    "active surface {active:?} missing from surfaces at leaf {id:?} after op {op_index}"
                );
                let unique: HashSet<SurfaceId> = surfaces.iter().map(|s| s.id).collect();
                assert_eq!(
                    surfaces.len(),
                    unique.len(),
                    "duplicate surface ids at leaf {id:?} after op {op_index}"
                );
            }
            Pane::Leaf { .. } => {}
            Pane::Split {
                first,
                second,
                ratio,
                ..
            } => {
                assert!(
                    *ratio > 0.0 && *ratio < 1.0,
                    "split ratio {ratio} out of (0,1) after op {op_index}"
                );
                rec(first, op_index);
                rec(second, op_index);
            }
        }
    }
    rec(root, op_index);

    let depth = tree_depth(root);
    assert!(
        depth <= max_depth,
        "tree depth {depth} exceeded cap {max_depth} after op {op_index}"
    );
}

#[test]
#[ignore = "stress: long random walk; run with --ignored"]
fn pane_tree_random_walk_holds_invariants() {
    const ITERATIONS: usize = 5_000;
    const MAX_DEPTH: u32 = 60;
    const TIME_BUDGET: Duration = Duration::from_secs(30);

    // Two seeds, run sequentially. Different seeds explore different op
    // distributions; both should pass.
    for &seed in &[0x00C0_FFEE_DEAD_BEEFu64, 0x0123_4567_89AB_CDEF] {
        let mut rng = Xs::new(seed);
        let mut root = Pane::Leaf {
            id: PaneId::new(),
            content: PaneContent::tabbed_terminal("Terminal", None),
        };

        let start = Instant::now();
        let mut splits = 0u32;
        let mut closes = 0u32;
        let mut tab_adds = 0u32;
        let mut tab_closes = 0u32;
        let mut renames = 0u32;
        let mut activate = 0u32;
        let mut reorder = 0u32;

        for op_index in 0..ITERATIONS {
            let leaves = collect_leaves(&root);
            let target = rng.pick(&leaves);

            // Pick op weighted toward growth/mutation, slight bias against close so
            // the tree exercises non-trivial structures most of the time.
            let r = rng.next_u64() % 100;
            match r {
                0..=22 => {
                    // split
                    let dir = if rng.next_u64() & 1 == 0 {
                        SplitDirection::Horizontal
                    } else {
                        SplitDirection::Vertical
                    };
                    if tree_depth(&root) < MAX_DEPTH - 1 {
                        let _ = root.split_leaf(
                            target,
                            dir,
                            0.5,
                            PaneContent::tabbed_terminal("Terminal", None),
                        );
                        splits += 1;
                    }
                }
                23..=37 => {
                    // close pane (leaf), keep root non-empty.
                    if leaves.len() > 1 {
                        let owned = std::mem::replace(
                            &mut root,
                            Pane::Leaf {
                                id: PaneId::new(),
                                content: PaneContent::tabbed_terminal("placeholder", None),
                            },
                        );
                        match owned.remove_leaf(target) {
                            RemoveOutcome::Replaced(p) => root = p,
                            RemoveOutcome::EntirelyRemoved => {
                                root = Pane::Leaf {
                                    id: PaneId::new(),
                                    content: PaneContent::tabbed_terminal("Terminal", None),
                                };
                            }
                            RemoveOutcome::NotFound(p) => root = p,
                        }
                        closes += 1;
                    }
                }
                38..=60 => {
                    let _ = root.add_surface_to_leaf(target, PaneSurface::terminal("tab", None));
                    tab_adds += 1;
                }
                61..=72 => {
                    if let Some(active) = root.active_surface_id(target) {
                        // Avoid removing the only surface; surface_count will be >= 2 for a real close.
                        if root.surface_count(target).unwrap_or(0) > 1 {
                            let _ = root.close_surface_in_leaf(target, active);
                            tab_closes += 1;
                        }
                    }
                }
                73..=84 => {
                    if let Some(active) = root.active_surface_id(target) {
                        let _ =
                            root.rename_surface(target, active, format!("name-{}", rng.next_u64()));
                        renames += 1;
                    }
                }
                85..=92 => {
                    if let Some(active) = root.active_surface_id(target) {
                        let _ = root.set_active_surface(target, active);
                        activate += 1;
                    }
                }
                _ => {
                    if let Some(active) = root.active_surface_id(target) {
                        let n = root.surface_count(target).unwrap_or(1);
                        let new_idx = (rng.next_u64() as usize) % n.max(1);
                        let _ = root.reorder_surface_in_leaf(target, active, new_idx);
                        reorder += 1;
                    }
                }
            }

            check_invariants(&root, op_index, MAX_DEPTH);

            assert!(
                start.elapsed() < TIME_BUDGET,
                "stress walk exceeded {TIME_BUDGET:?} after {op_index} ops (seed {seed:#x})"
            );
        }

        let elapsed = start.elapsed();
        let final_leaves = collect_leaves(&root).len();
        let final_depth = tree_depth(&root);
        eprintln!(
            "seed {seed:#018x}: {ITERATIONS} ops in {elapsed:?} \
             (splits={splits} closes={closes} tab_adds={tab_adds} \
             tab_closes={tab_closes} renames={renames} activate={activate} reorder={reorder}) \
             final_leaves={final_leaves} final_depth={final_depth}"
        );
    }
}

#[test]
#[ignore = "stress: deep split chain memory probe"]
fn pane_tree_deep_chain_stays_traversable() {
    // Build a chain of 1000 splits and confirm leaf walk + depth match expectation.
    const N: u32 = 1_000;
    let root_id = PaneId::new();
    let mut root = Pane::Leaf {
        id: root_id,
        content: PaneContent::tabbed_terminal("Terminal", None),
    };
    let mut current = root_id;
    let start = Instant::now();
    for i in 0..N {
        let new_id = root
            .split_leaf(
                current,
                if i & 1 == 0 {
                    SplitDirection::Horizontal
                } else {
                    SplitDirection::Vertical
                },
                0.5,
                PaneContent::tabbed_terminal("Terminal", None),
            )
            .unwrap_or_else(|| panic!("split failed at depth {i}"));
        current = new_id;
    }
    let leaves = collect_leaves(&root);
    assert_eq!(leaves.len() as u32, N + 1, "expected {} leaves", N + 1);
    assert!(root.first_leaf_id().is_some());
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "deep-split chain too slow: {:?}",
        start.elapsed()
    );
    eprintln!(
        "deep-split chain n={N} built in {:?}, leaves={}, depth={}",
        start.elapsed(),
        leaves.len(),
        tree_depth(&root)
    );
}
