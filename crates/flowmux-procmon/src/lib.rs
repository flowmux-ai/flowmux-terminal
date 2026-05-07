// SPDX-License-Identifier: GPL-3.0-or-later
//! Watch a process tree under a workspace's root PID and report which
//! TCP ports it has bound in LISTEN state. Mirrors cmux's sidebar
//! "listening ports" pill on Linux by parsing `/proc/<pid>/net/tcp[6]`
//! and `/proc/<pid>/fd/*` socket inodes.
//!
//! No external deps — pure procfs reads.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ProcError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("not on linux: /proc unavailable")]
    NotLinux,
}

/// Return all PIDs descended from `root` (inclusive). Walks
/// `/proc/<pid>/status` PPid edges. O(n_procs); cheap enough to call
/// per-second on the GTK main loop.
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

/// Read the comm (executable basename) of a process, trimmed.
pub fn comm_of(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

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
    fn listening_ports_reports_current_process_tcp_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let pids = HashSet::from([std::process::id()]);

        let ports = listening_ports(&pids).unwrap();
        assert!(ports.contains(&port), "expected {port} in {ports:?}");
    }
}
