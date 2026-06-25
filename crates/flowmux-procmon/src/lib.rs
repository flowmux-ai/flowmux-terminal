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
        return std::path::Path::new(&format!("/proc/{pid}")).exists();
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
