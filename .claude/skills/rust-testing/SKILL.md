---
name: rust-testing
description: Write and run Rust tests for this workspace correctly. Use when asked to add tests, run the suite, fix failing tests, set up coverage, or verify a change. Knows the GUI-vs-headless split, the xvfb + dbus harness CI mirrors, single-test filtering, and famous test crates (proptest, insta, rstest, nextest).
---

# Rust testing (flowmux workspace)

Tests split into two worlds. Run the right harness or GUI/D-Bus tests
hang or fail spuriously.

## Headless crates — run bare

No GTK, no D-Bus. Fast. Default to these while iterating.

```bash
cargo test -p flowmux-core
cargo test -p flowmux-ipc -- --nocapture
cargo test -p flowmux-config
cargo test -p flowmux-state
```

## GTK / D-Bus crates — mirror CI

Crates that open GTK or D-Bus (the `flowmux` GUI crate, notifier paths)
need a virtual display and a session bus. Mirror CI exactly:

```bash
xvfb-run -a dbus-run-session -- cargo test --workspace --locked -- --nocapture
```

- `xvfb-run -a` — headless X server (auto-picks a free display).
- `dbus-run-session` — private session bus so `org.gtk.Notifications`
  tests don't touch the real desktop.
- `--locked` — fail if `Cargo.lock` would change (CI parity).

## Single test / filter

`cargo test` takes a substring filter:

```bash
cargo test -p flowmux-core title_is_shell_cwd_echo   # one test by name
cargo test -p flowmux-core pane_tree                 # all matching substring
cargo test -p flowmux-core -- --exact path::to::test # exact path
cargo test -p flowmux-core -- --ignored              # run #[ignore]d ones
```

`-- --nocapture` shows `println!`/`dbg!` output; `-- --test-threads=1`
serializes when a test touches shared global/env state.

## Writing tests — house style

- **Unit tests inline** under `#[cfg(test)] mod tests { … }` at the
  bottom of the file (see `flowmux-core/src/lib.rs`, 200+ tests). Keep
  new domain tests next to the logic.
- **Integration tests** in `crates/<crate>/tests/*.rs` for cross-module
  / cross-process behavior (e.g. `cross_process_lock.rs`,
  `pty_tee_integration.rs`).
- **Pure logic first** — push testable logic into `flowmux-core` /
  headless crates so it runs without xvfb. Widget code stays thin.
- **Env / path tests** set `XDG_*` via a scoped guard and restore it;
  never mutate process env without cleanup (parallel tests share it).
- **Round-trip tests** for anything serialized: build → to_json →
  from_json → assert_eq. Add one for every new persisted field, and a
  "missing field loads as default" test to lock backward compat.
- **Async tests** use `#[tokio::test]`; the daemon/IPC layer is tokio.

## Famous crates to reach for (install on demand)

None are wired in yet — add to the target crate's `[dev-dependencies]`
only when they earn their place.

- **nextest** (https://nexte.st) — faster runner, better output, flaky
  retries. `cargo install cargo-nextest`, then
  `cargo nextest run -p flowmux-core` (wrap GUI crate in the xvfb/dbus
  prefix the same way).
- **proptest** — property-based testing; great for the pane-tree
  mutations and the OSC parser FSM (`flowmux-notify`). Generate random
  trees/byte-streams, assert invariants (IDs stable, no panic, ratio in
  range) instead of hand-picked cases.
- **insta** (https://insta.rs) — snapshot tests; ideal for browser
  Markdown snapshots and rendered titles. `assert_snapshot!`,
  review with `cargo insta review`.
- **rstest** — parametric fixtures / table-driven cases; collapses the
  many `title_is_shell_cwd_echo`-style scenarios into one `#[case]` set.
- **mockall** — trait mocks for `TerminalBackend` / `BrowserController`
  when a test shouldn't spawn a real PTY or WebView.

## Coverage

```bash
cargo install cargo-llvm-cov                          # one-time
cargo llvm-cov -p flowmux-core --html                 # headless: open target/llvm-cov/html
xvfb-run -a dbus-run-session -- cargo llvm-cov --workspace --lcov --output-path lcov.info
```

## Before declaring done

1. `cargo fmt --all` clean.
2. `cargo clippy --workspace --all-targets -- -D warnings` silent.
3. Headless tests green per-crate.
4. Full mirrored run green:
   `xvfb-run -a dbus-run-session -- cargo test --workspace --locked`.

If any step fails, report the failure and the output — do not mark the
work complete. A flaky/hung GUI test usually means the xvfb/dbus prefix
was missing, not a real failure.
