// SPDX-License-Identifier: GPL-3.0-or-later
//! Watch a process tree under a workspace's root PID and report which
//! TCP ports it has bound in LISTEN state. On Linux, mirrors cmux's
//! sidebar "listening ports" pill by parsing `/proc/<pid>/net/tcp[6]`
//! and `/proc/<pid>/fd/*` socket inodes. Other Unix platforms still
//! support PID liveness, but report no descendants or listening ports.
//!
//! No platform command dependencies — procfs reads on Linux, `kill(0)`
//! liveness elsewhere.

use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::fs;
use std::io;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ProcError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("not on linux: /proc unavailable")]
    NotLinux,
}

/// Report whether process `pid` is still alive, by testing for the
/// existence of `/proc/<pid>` on Linux and `kill(pid, 0)` elsewhere.
/// Used by the
/// daemon's agent-liveness sweep to clear a presence whose agent process
/// vanished (hard kill / closed terminal) without firing `SessionEnd`.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// Return all PIDs descended from `root` (inclusive). Walks
/// `/proc/<pid>/status` PPid edges. O(n_procs); cheap enough to call
/// per-second on the GTK main loop.
#[cfg(target_os = "linux")]
pub fn descendants(root: u32) -> Result<HashSet<u32>, ProcError> {
    let mut by_parent: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let name = entry.file_name();
        let s = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        let pid: u32 = match s.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Some(ppid) = read_ppid(pid) {
            by_parent.entry(ppid).or_default().push(pid);
        }
    }

    let mut out = HashSet::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if !out.insert(p) {
            continue;
        }
        if let Some(children) = by_parent.get(&p) {
            stack.extend_from_slice(children);
        }
    }
    Ok(out)
}

#[cfg(not(target_os = "linux"))]
pub fn descendants(root: u32) -> Result<HashSet<u32>, ProcError> {
    Ok(HashSet::from([root]))
}

/// Read the comm (executable basename) of a process, trimmed.
#[cfg(target_os = "linux")]
pub fn comm_of(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(not(target_os = "linux"))]
pub fn comm_of(pid: u32) -> Option<String> {
    if pid != std::process::id() {
        return None;
    }
    std::env::current_exe().ok().and_then(|p| {
        p.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    })
}

/// Canonical CLI names of AI coding agents flowmux recognizes by process
/// `comm`. Kept ≤15 chars so a match survives the kernel's `comm`
/// truncation (`TASK_COMM_LEN` = 16 including NUL). These are the identity
/// strings the Agent Bar renders and that hook/screen sources also use.
pub const KNOWN_AGENT_COMMS: &[&str] = &[
    "codex", "claude", "opencode", "cline", "gemini", "aider", "goose",
];

/// Map a raw process `comm` to a canonical agent name, or `None`.
fn match_agent_comm(comm: &str) -> Option<&'static str> {
    let c = comm.trim().to_ascii_lowercase();
    KNOWN_AGENT_COMMS.iter().copied().find(|name| *name == c)
}

/// Script interpreters that host an agent as a file argument, so the kernel
/// `comm` is the interpreter (`node`, `python`, …) rather than the agent. For
/// these, the agent identity lives in the script path inside
/// `/proc/<pid>/cmdline`. Cline ships as a Node CLI (`node …/cline --tui`) that
/// never sets `process.title`, so its `comm` stays `node` and
/// [`match_agent_comm`] can't see it — unlike `claude` (sets `process.title`)
/// or `codex` (native binary). Lowercase for case-insensitive comparison.
#[cfg(target_os = "linux")]
const AGENT_SCRIPT_INTERPRETERS: &[&str] = &["node", "bun", "deno", "python", "python3"];

/// Resolve an agent name from a process's argv when `argv[0]` is a known script
/// interpreter: the first non-flag argument is the script path, whose basename
/// (minus a `.js`/`.mjs`/`.cjs` suffix) is matched against [`KNOWN_AGENT_COMMS`]
/// — e.g. `["node", "/home/u/.local/bin/cline", "--tui"]` → `Some("cline")`.
/// A non-interpreter `argv[0]` returns `None`: native binaries are matched by
/// `comm` instead, so a shell merely touching a file named `cline` can't
/// false-match.
#[cfg(target_os = "linux")]
fn agent_from_argv(argv: &[String]) -> Option<&'static str> {
    let interpreter = std::path::Path::new(argv.first()?)
        .file_name()?
        .to_str()?
        .to_ascii_lowercase();
    if !AGENT_SCRIPT_INTERPRETERS.contains(&interpreter.as_str()) {
        return None;
    }
    argv.iter().skip(1).find_map(|arg| {
        if arg.starts_with('-') {
            return None;
        }
        let stem = std::path::Path::new(arg).file_name()?.to_str()?;
        let stem = stem
            .strip_suffix(".js")
            .or_else(|| stem.strip_suffix(".mjs"))
            .or_else(|| stem.strip_suffix(".cjs"))
            .unwrap_or(stem)
            .to_ascii_lowercase();
        KNOWN_AGENT_COMMS.iter().copied().find(|name| *name == stem)
    })
}

/// The argv of a process from `/proc/<pid>/cmdline` (NUL-separated fields), or
/// an empty vec when it can't be read (the process exited, or permissions).
#[cfg(target_os = "linux")]
fn cmdline_of(pid: u32) -> Vec<String> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|raw| {
            raw.split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Canonical agent name for a single PID: the kernel `comm` first (covers
/// native binaries and agents that set `process.title`), falling back to the
/// argv script name for interpreter-hosted agents like Cline. See
/// [`agent_from_argv`].
#[cfg(target_os = "linux")]
fn agent_of_pid(pid: u32) -> Option<&'static str> {
    if let Some(name) = comm_of(pid).as_deref().and_then(match_agent_comm) {
        return Some(name);
    }
    agent_from_argv(&cmdline_of(pid))
}

/// Direct child PIDs of `pid`, unioned across **every** thread of the
/// process via `/proc/<pid>/task/<tid>/children`, or `None` when the feature
/// is unavailable (kernel built without `CONFIG_PROC_CHILDREN`, or the process
/// exited). An existing process with no children yields `Some(empty)`, which is
/// how the caller distinguishes "no kids" from "feature unavailable".
///
/// The per-thread aggregation matters: the `children` file lists only the
/// children forked by *that specific thread*. A multithreaded parent (e.g. the
/// tokio-based `flowmuxctl pty-tee` that spawns the pane's shell) forks its
/// child from a worker thread, so `/proc/<pid>/task/<pid>/children` (the main
/// thread) is empty and reading only that file would miss the entire subtree.
#[cfg(target_os = "linux")]
fn read_children(pid: u32) -> Option<Vec<u32>> {
    let entries = fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    let mut kids = Vec::new();
    let mut any = false;
    for entry in entries.flatten() {
        let path = entry.path().join("children");
        if let Ok(s) = fs::read_to_string(&path) {
            any = true;
            kids.extend(
                s.split_ascii_whitespace()
                    .filter_map(|t| t.parse::<u32>().ok()),
            );
        }
    }
    // Only report "feature unavailable" when not a single children file was
    // readable; an empty-but-present file means the thread simply has no kids.
    any.then_some(kids)
}

/// Cap on process-tree nodes visited per detection, a defensive bound so a
/// pathological tree can never turn one poll into an unbounded `/proc` walk.
#[cfg(target_os = "linux")]
const AGENT_TREE_NODE_CAP: usize = 512;

/// Detect which AI coding agent, if any, is running anywhere in the process
/// tree rooted at `root` (inclusive). The pane's root PID is the pty-tee /
/// shell wrapper; the real agent (e.g. `codex`, `claude`) is a descendant, so
/// the whole subtree's `comm` values are checked. Returns the canonical agent
/// name from [`KNOWN_AGENT_COMMS`]. This is the process-truth source for Agent
/// Bar presence: independent of the agent's TUI text, OSC title, or hooks, so
/// it detects idle agents that emit no recognizable screen signal (notably
/// Codex, whose title is `<spinner> <cwd>`).
///
/// Cost: walks only `root`'s own subtree (typically 2–6 processes) via the
/// kernel `children` file, so a poll's cost is proportional to the pane's
/// descendants — not to the total number of system processes, and not
/// multiplied when several panes are polled. Falls back to a full `/proc`
/// parent-map scan only on kernels without `CONFIG_PROC_CHILDREN`.
#[cfg(target_os = "linux")]
pub fn agent_name_in_tree(root: u32) -> Option<&'static str> {
    if read_children(root).is_none() {
        // Feature unavailable: one full scan, matching the old behaviour.
        let mut pids: Vec<u32> = descendants(root).ok()?.into_iter().collect();
        pids.sort_unstable();
        return pids.iter().copied().find_map(agent_of_pid);
    }
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) || seen.len() > AGENT_TREE_NODE_CAP {
            continue;
        }
        if let Some(name) = agent_of_pid(pid) {
            return Some(name);
        }
        stack.extend(read_children(pid).unwrap_or_default());
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub fn agent_name_in_tree(root: u32) -> Option<&'static str> {
    descendants(root)
        .ok()?
        .into_iter()
        .filter_map(comm_of)
        .find_map(|comm| match_agent_comm(&comm))
}

#[cfg(target_os = "linux")]
fn read_ppid(pid: u32) -> Option<u32> {
    let text = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Local TCP ports in LISTEN state owned by any pid in `pids`.
#[cfg(target_os = "linux")]
pub fn listening_ports(pids: &HashSet<u32>) -> Result<Vec<u16>, ProcError> {
    let inodes = collect_socket_inodes(pids)?;
    let mut ports = HashSet::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for line in text.lines().skip(1) {
            // Format: sl  local_address rem_address st ... uid timeout inode ...
            let mut it = line.split_whitespace();
            let _sl = it.next();
            let local = it.next();
            let _remote = it.next();
            let st = it.next();
            // st == "0A" means TCP_LISTEN.
            if st != Some("0A") {
                continue;
            }
            let local = match local {
                Some(s) => s,
                None => continue,
            };
            // local = ADDR:HEXPORT
            let port = match local.rsplit_once(':') {
                Some((_, p)) => p,
                None => continue,
            };
            let port = match u16::from_str_radix(port, 16) {
                Ok(p) => p,
                Err(_) => continue,
            };
            // Walk fields to inode.
            // sl local rem st tx_queue:rx_queue tr:tm_when retrnsmt uid timeout inode
            let inode = it.nth(5);
            let inode: u64 = match inode.and_then(|s| s.parse().ok()) {
                Some(i) => i,
                None => continue,
            };
            if inodes.contains(&inode) {
                ports.insert(port);
            }
        }
    }
    let mut v: Vec<u16> = ports.into_iter().collect();
    v.sort_unstable();
    Ok(v)
}

#[cfg(not(target_os = "linux"))]
pub fn listening_ports(_pids: &HashSet<u32>) -> Result<Vec<u16>, ProcError> {
    Ok(Vec::new())
}

#[cfg(target_os = "linux")]
fn collect_socket_inodes(pids: &HashSet<u32>) -> Result<HashSet<u64>, ProcError> {
    let mut out = HashSet::new();
    for &pid in pids {
        let dir = PathBuf::from(format!("/proc/{pid}/fd"));
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue, // process may have exited or we lack perms
        };
        for entry in entries.flatten() {
            let target = match fs::read_link(entry.path()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            // Symlink target looks like "socket:[12345]"
            if let Some(s) = target.to_str() {
                if let Some(rest) = s.strip_prefix("socket:[") {
                    if let Some(num) = rest.strip_suffix(']') {
                        if let Ok(inode) = num.parse() {
                            out.insert(inode);
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_in_its_own_descendants() {
        let pid = std::process::id();
        let ds = descendants(pid).unwrap();
        assert!(ds.contains(&pid));
    }

    #[test]
    fn match_agent_comm_maps_known_names_case_insensitively() {
        assert_eq!(match_agent_comm("codex"), Some("codex"));
        assert_eq!(match_agent_comm("Claude"), Some("claude"));
        assert_eq!(match_agent_comm("  opencode  "), Some("opencode"));
        // Wrappers and unrelated processes must not match.
        assert_eq!(match_agent_comm("node"), None);
        assert_eq!(match_agent_comm("bash"), None);
        assert_eq!(match_agent_comm("python"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn agent_from_argv_resolves_interpreter_hosted_agents() {
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Cline: a Node CLI whose script basename is the agent name.
        assert_eq!(
            agent_from_argv(&argv(&["node", "/home/u/.local/bin/cline", "--tui"])),
            Some("cline")
        );
        // Interpreter flags before the script path are skipped.
        assert_eq!(
            agent_from_argv(&argv(&["node", "--enable-source-maps", "/x/codex"])),
            Some("codex")
        );
        // A `.js`/`.mjs`/`.cjs` script suffix is stripped before matching.
        assert_eq!(
            agent_from_argv(&argv(&["node", "/opt/aider.js"])),
            Some("aider")
        );
        // python-hosted agent, argv[0] as a full path.
        assert_eq!(
            agent_from_argv(&argv(&["/usr/bin/python3", "/usr/bin/aider"])),
            Some("aider")
        );
        // Non-interpreter argv[0]: native binaries go through `comm`, so a shell
        // merely touching a file named `cline` must not false-match.
        assert_eq!(agent_from_argv(&argv(&["bash", "cline"])), None);
        // An interpreter running an unrelated script matches nothing.
        assert_eq!(agent_from_argv(&argv(&["node", "/srv/server.js"])), None);
    }

    #[test]
    fn agent_name_in_tree_is_none_for_a_tree_without_an_agent() {
        // The test runner's own tree contains cargo/rustc/the test binary,
        // none of which are in KNOWN_AGENT_COMMS.
        assert_eq!(agent_name_in_tree(std::process::id()), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn read_children_sees_child_forked_from_worker_thread() {
        use std::process::Command;
        use std::sync::mpsc;
        use std::time::Duration;
        // Reproduce the pty-tee shape: a *worker* thread forks the child while
        // staying alive. The kernel files children under the forking thread's
        // tid, so `/proc/<pid>/task/<main-tid>/children` never lists it. Before
        // the per-thread aggregation fix, agent detection walked an empty
        // subtree from the pane root and reported "no agent".
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (pid_tx, pid_rx) = mpsc::channel::<u32>();
        let worker = std::thread::spawn(move || {
            let mut child = Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn sleep");
            pid_tx.send(child.id()).unwrap();
            let _ = release_rx.recv(); // keep this thread (and its children file) alive
            let _ = child.kill();
            let _ = child.wait();
        });
        let child_pid = pid_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let kids = read_children(std::process::id()).expect("children feature available");
        let found = kids.contains(&child_pid);
        let _ = release_tx.send(()); // let the worker reap its child and exit
        worker.join().unwrap();
        assert!(
            found,
            "worker-thread child {child_pid} missing from {kids:?}"
        );
    }

    #[test]
    fn pid_alive_tracks_self_and_rejects_unused_pid() {
        assert!(pid_alive(std::process::id()));
        // PID 0 is the scheduler swapper — never a /proc entry on Linux.
        assert!(!pid_alive(0));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn listening_ports_reports_current_process_tcp_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let pids = HashSet::from([std::process::id()]);

        let ports = listening_ports(&pids).unwrap();
        assert!(ports.contains(&port), "expected {port} in {ports:?}");
    }

    #[test]
    fn descendants_of_unknown_pid_yields_only_itself() {
        // A PID that is not currently a process can still appear as a
        // singleton "subtree" — the walker simply finds no children. This
        // documents the fact that callers must treat the result as "PIDs
        // we know about", not "live PIDs", and that we do not try to
        // validate liveness here (cheap procfs scan only).
        let unlikely_pid: u32 = u32::MAX - 1;
        let ds = descendants(unlikely_pid).unwrap();
        assert!(ds.contains(&unlikely_pid));
        // No live children for a synthetic PID.
        assert_eq!(ds.len(), 1);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn listening_ports_filters_to_pids_owning_the_socket() {
        // A second process's listener should not show up under our PID.
        let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let other_port = other.local_addr().unwrap().port();
        let our = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let our_port = our.local_addr().unwrap().port();

        // Pretend a different (synthetic) PID owns nothing — the result
        // must not include either port. Only our own pid set should
        // surface our_port.
        let other_pids = HashSet::from([u32::MAX - 1]);
        let ports = listening_ports(&other_pids).unwrap();
        assert!(!ports.contains(&our_port));
        assert!(!ports.contains(&other_port));

        let our_pids = HashSet::from([std::process::id()]);
        let ports = listening_ports(&our_pids).unwrap();
        assert!(ports.contains(&our_port));
        assert!(ports.contains(&other_port));
    }

    #[test]
    fn comm_of_returns_basename_of_self() {
        let comm = comm_of(std::process::id()).expect("comm should be readable");
        assert!(!comm.is_empty());
        // The current binary is one of the cargo test runners. The comm
        // must not include a path separator (kernel truncates to basename
        // and 16 chars).
        assert!(!comm.contains('/'));
        #[cfg(target_os = "linux")]
        assert!(comm.len() <= 16);
    }
}
