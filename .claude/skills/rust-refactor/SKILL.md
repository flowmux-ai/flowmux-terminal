---
name: rust-refactor
description: Refactor and clean up Rust code in this workspace using Clippy-driven, idiomatic patterns. Use when asked to refactor, simplify, clean up, de-duplicate, reduce clones/allocations, tighten error handling, or modernize Rust in any crate under crates/. Pairs with rust-testing — refactor behind green tests.
---

# Rust refactoring (flowmux workspace)

Refactor in small, behavior-preserving steps, each verified by the
compiler, Clippy, and the test suite. Never refactor on red — get a
green baseline first (see `rust-testing`), change, re-verify.

## Golden loop

Run after every meaningful edit. Order matters: fmt → check → clippy → test.

```bash
cargo fmt --all
cargo check --workspace            # headless crates (excludes GUI crate `flowmux`)
cargo check -p flowmux             # GTK crate (needs GTK4/libadwaita/WebKitGTK 6.0 dev pkgs)
cargo clippy --workspace --all-targets -- -D warnings
```

`-D warnings` is the project bar (CI enforces it). A refactor is not
done until Clippy is silent at that level.

Scope a single crate while iterating to cut cycle time:

```bash
cargo clippy -p flowmux-core --all-targets -- -D warnings
cargo test  -p flowmux-core
```

## Clippy as the refactor engine

Clippy (https://doc.rust-lang.org/clippy/) is the famous, canonical
Rust linter — 700+ lints. Use it as the to-do list, not just a gate.

```bash
# See every suggestion, including pedantic, without failing the build:
cargo clippy --workspace --all-targets -- -W clippy::pedantic -W clippy::nursery

# Auto-apply the mechanical fixes Clippy is confident about:
cargo clippy --fix --workspace --all-targets --allow-dirty --allow-staged
cargo fmt --all                    # re-format after --fix
```

After `--fix`, read the diff — Clippy's autofix is conservative but
not infallible. Re-run the golden loop.

When a lint is wrong for a specific line, suppress narrowly with a
reason, never blanket-allow a whole module:

```rust
#[allow(clippy::too_many_arguments)] // builder is generated; splitting hurts call sites
```

## High-value idiomatic refactors

Pick the smallest change that removes the smell. Common ones, with the
flowmux idioms already in the tree:

- **Clone reduction** — borrow instead of `.clone()`; take `&str`/`&[T]`
  in signatures, return owned only at boundaries. `Arc<Mutex<State>>` is
  shared by clone-of-Arc (cheap) — never `.lock().await.clone()` the
  inner `State` unless you truly need a snapshot.
- **Option/Result combinators** — replace `match`/`if let` ladders with
  `map`/`and_then`/`unwrap_or_else`/`ok_or`/`?`. The pane-tree
  `*_descend` helpers in `flowmux-core` are the house style.
- **Iterator chains over index loops** — `filter`/`map`/`flat_map`/
  `find`/`collect`. Prefer `for_each_leaf`-style traversal over manual
  recursion when a helper exists.
- **Error enums with `thiserror`** — every crate models errors as a
  `thiserror`-derived enum (`TerminalError`, `RpcError`, `VcsError`,
  `SshError`, …). New fallible code returns a crate enum, not
  `anyhow::Error`, in library crates; reserve `anyhow` for binaries/CLI.
- **Newtype IDs** — keep UUID newtypes (`PaneId`, `SurfaceId`, …) opaque;
  don't leak the inner `Uuid`. Add methods, not pub fields.
- **`#[serde(default)]` / `skip`** — runtime-only fields (e.g.
  `AgentPresence`) stay `#[serde(skip)]`; optional persisted fields stay
  `#[serde(default)]` so old `state.json` loads. Preserve these on edits.
- **Extract function / collapse duplication** — when two `match` arms or
  two crate sites diverge only in a value, hoist a helper. Watch for the
  same selector/JS builder duplicated in `flowmux-browser`.

## Bigger structural moves

- **Move a type to `flowmux-core`** when two crates need it — core has no
  GTK/tokio deps; keep it that way (don't pull `gtk`/`tokio` into core).
- **New IPC verb touching widgets** follows the fixed pattern: add a
  `Request`/`Response` variant in `flowmux-ipc`, a `GtkCommand` variant +
  `oneshot` ack in `crates/flowmux/src/bridge/`, handle it in the
  dispatch loop. Don't shortcut around the bridge — GTK is `!Send`.
- **Don't stub CDP-only browser verbs** as no-ops; WebKitGTK has no CDP,
  so they stay `not_supported` (see `AGENTS.md`).

## Guardrails

- Behavior-preserving only unless the task says otherwise. If a refactor
  changes observable behavior, call it out and gate it on a test.
- Preserve the `SPDX-License-Identifier: GPL-3.0-or-later` header on
  every source file; add it to any new `.rs` file.
- Match surrounding style (naming, comment density, error idiom). Read
  the neighbors before introducing a new pattern.
- One concern per commit. Mechanical Clippy `--fix` and hand
  refactors go in separate commits so review is readable.
- Optional sharper tools (install on demand, none required):
  `cargo machete` (unused deps), `cargo +nightly udeps`,
  `cargo bloat` (binary size hotspots), `cargo expand` (macro output).
