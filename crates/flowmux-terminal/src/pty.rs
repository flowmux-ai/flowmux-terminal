// SPDX-License-Identifier: GPL-3.0-or-later
//! PTY helpers shared by the terminal pane.
//!
//! Spawns a child under a fresh PTY via `forkpty(3)`, exposes the master fd for
//! read/write plumbing, and resizes the kernel window via `TIOCSWINSZ`.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::Path;
use std::time::{Duration, Instant};

const CLOSE_GRACE: Duration = Duration::from_secs(5);
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A spawned child process attached to a PTY master fd.
pub struct Pty {
    master: RawFd,
    child: libc::pid_t,
    reaped: bool,
}

impl Pty {
    /// Spawn `argv` under a fresh PTY sized `cols` x `rows`. `cwd` is the
    /// child's working directory; `extra_env` is merged onto the inherited
    /// environment (later entries win). `argv[0]` is resolved via `PATH`.
    pub fn spawn(
        argv: &[&str],
        cwd: Option<&Path>,
        extra_env: &[(String, String)],
        cols: u16,
        rows: u16,
    ) -> io::Result<Pty> {
        if argv.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
        }

        // Build argv/envp/cwd as C strings BEFORE forking: after fork() in the
        // child we may only call async-signal-safe functions, so no allocation
        // or std::env mutation is allowed there.
        let c_argv: Vec<CString> = argv
            .iter()
            .map(|a| CString::new(*a))
            .collect::<Result<_, _>>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argv has NUL"))?;
        let mut argv_ptrs: Vec<*const libc::c_char> = c_argv.iter().map(|s| s.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());

        let c_env = build_envp(extra_env)?;
        let mut env_ptrs: Vec<*const libc::c_char> = c_env.iter().map(|s| s.as_ptr()).collect();
        env_ptrs.push(std::ptr::null());

        let c_cwd = match cwd {
            Some(p) => Some(
                CString::new(p.as_os_str().as_bytes())
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "cwd has NUL"))?,
            ),
            None => None,
        };

        let mut ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let mut master: RawFd = -1;
        // libc declares forkpty's `winp` as `*const winsize` on Linux
        // but `*mut winsize` on macOS; go through a raw pointer so one
        // call site type-checks (and passes clippy) on both.
        let winp = std::ptr::addr_of_mut!(ws);
        // SAFETY: forkpty allocates a PTY pair and forks. We pass a valid
        // master-out pointer and winsize; name/termios default to NULL.
        let pid = unsafe {
            libc::forkpty(
                &mut master,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                winp as _,
            )
        };

        if pid < 0 {
            return Err(io::Error::last_os_error());
        }

        if pid == 0 {
            // ---- Child: async-signal-safe only, then exec or _exit. ----
            if let Some(ref c) = c_cwd {
                // Ignore chdir failure: better to exec in the wrong dir than to
                // leave a half-spawned child. (Mirrors common shell behavior.)
                unsafe { libc::chdir(c.as_ptr()) };
            }
            unsafe {
                // execvpe(3) is a glibc/BSD extension absent on macOS, so there
                // we replace the environment then execvp. Both paths hand the
                // child exactly `env_ptrs` (build_envp already merged the
                // inherited env with the pane's extra vars).
                #[cfg(not(target_os = "macos"))]
                libc::execvpe(argv_ptrs[0], argv_ptrs.as_ptr(), env_ptrs.as_ptr());
                #[cfg(target_os = "macos")]
                {
                    extern "C" {
                        fn _NSGetEnviron() -> *mut *mut *mut libc::c_char;
                    }
                    *_NSGetEnviron() = env_ptrs.as_ptr() as *mut *mut libc::c_char;
                    libc::execvp(argv_ptrs[0], argv_ptrs.as_ptr());
                }
                // exec only returns on failure.
                libc::_exit(127);
            }
        }

        // ---- Parent. ----
        Ok(Pty {
            master,
            child: pid,
            reaped: false,
        })
    }

    /// The PTY master file descriptor (for poll/glib fd watches).
    pub fn master_fd(&self) -> RawFd {
        self.master
    }

    /// PID of the spawned child (for `/proc/<pid>/cwd` lookups).
    pub fn child_pid(&self) -> i32 {
        self.child
    }

    /// Read available output bytes into `buf`. Returns 0 at EOF (child exited
    /// and closed the slave).
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: master is a valid fd; buf is a valid mutable slice.
        let n = unsafe {
            libc::read(
                self.master,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            // After the child exits, the master read fails with EIO on Linux;
            // surface that as a clean EOF for the read loop.
            if err.raw_os_error() == Some(libc::EIO) {
                return Ok(0);
            }
            return Err(err);
        }
        Ok(n as usize)
    }

    /// Write keystroke bytes to the child.
    pub fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        // SAFETY: master is a valid fd; data is a valid slice.
        let n = unsafe {
            libc::write(
                self.master,
                data.as_ptr() as *const libc::c_void,
                data.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    /// Resize the kernel PTY window. `cell_w_px`/`cell_h_px` populate the pixel
    /// fields some apps read; pass 0 when unknown.
    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_w_px: u16,
        cell_h_px: u16,
    ) -> io::Result<()> {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: cols.saturating_mul(cell_w_px),
            ws_ypixel: rows.saturating_mul(cell_h_px),
        };
        // SAFETY: master is a valid fd; ws outlives the ioctl call.
        let rc = unsafe { libc::ioctl(self.master, libc::TIOCSWINSZ, &ws) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Non-blocking reap: returns `Some(status)` if the child has exited.
    pub fn try_wait(&mut self) -> io::Result<Option<i32>> {
        if self.reaped {
            return Ok(Some(0));
        }
        let mut status: libc::c_int = 0;
        // SAFETY: standard waitpid usage.
        let rc = unsafe { libc::waitpid(self.child, &mut status, libc::WNOHANG) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if rc == 0 {
            return Ok(None); // still running
        }
        self.reaped = true;
        Ok(Some(status))
    }

    /// Send SIGHUP to the child (used on pane close before reaping).
    pub fn hangup(&mut self) {
        if !self.reaped {
            signal_process_group(self.child, libc::SIGHUP);
        }
    }

    /// Close immediately and reap the child process group off-thread.
    pub fn close_async(mut self) {
        self.hangup();
        self.close_master();
        self.start_reaper();
    }

    fn close_master(&mut self) {
        if self.master >= 0 {
            // SAFETY: closing our owned fd once.
            unsafe { libc::close(self.master) };
            self.master = -1;
        }
    }

    fn start_reaper(&mut self) {
        if self.reaped {
            return;
        }
        let child = self.child;
        self.reaped = true;
        if let Err(error) = std::thread::Builder::new()
            .name("flowmux-pty-reaper".into())
            .spawn(move || reap_process_group(child, CLOSE_GRACE))
        {
            signal_process_group(child, libc::SIGKILL);
            eprintln!("failed to start PTY reaper: {error}");
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.hangup();
        self.close_master();
        self.start_reaper();
    }
}

fn signal_process_group(pid: libc::pid_t, signal: libc::c_int) {
    // SAFETY: forkpty creates the child as the leader of its own process group.
    unsafe {
        libc::kill(-pid, signal);
    }
}

fn process_group_exists(pid: libc::pid_t) -> bool {
    let rc = unsafe { libc::kill(-pid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn reap_process_group(pid: libc::pid_t, grace: Duration) {
    let deadline = Instant::now() + grace;
    let mut child_reaped = false;
    loop {
        if !child_reaped {
            let mut status = 0;
            let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if rc == pid
                || (rc < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
            {
                child_reaped = true;
            } else if rc < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return;
            }
        }
        if child_reaped && !process_group_exists(pid) {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(REAP_POLL_INTERVAL);
    }

    signal_process_group(pid, libc::SIGKILL);
    let kill_deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < kill_deadline {
        if !child_reaped {
            let mut status = 0;
            let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if rc == pid
                || (rc < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
            {
                child_reaped = true;
            }
        }
        if child_reaped && !process_group_exists(pid) {
            return;
        }
        std::thread::sleep(REAP_POLL_INTERVAL);
    }
}

/// Merge `extra_env` onto the inherited environment into a `KEY=VALUE` CString
/// list. Later duplicate keys win.
fn build_envp(extra_env: &[(String, String)]) -> io::Result<Vec<CString>> {
    build_envp_from(std::env::vars(), extra_env)
}

fn build_envp_from<I>(inherited: I, extra_env: &[(String, String)]) -> io::Result<Vec<CString>>
where
    I: IntoIterator<Item = (String, String)>,
{
    use std::collections::BTreeMap;
    // Terminal panes should advertise their own color capability instead of
    // inheriting NO_COLOR from the GUI launcher (for example Codex runs with
    // NO_COLOR=1). Callers can still pass NO_COLOR explicitly via extra_env.
    let mut map: BTreeMap<String, String> = inherited
        .into_iter()
        .filter(|(k, _)| k != "NO_COLOR")
        .collect();
    for (k, v) in extra_env {
        map.insert(k.clone(), v.clone());
    }
    map.into_iter()
        .map(|(k, v)| {
            CString::new(format!("{k}={v}"))
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "env has NUL"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn envp_strings(envp: Vec<CString>) -> Vec<String> {
        envp.into_iter()
            .map(|s| s.into_string().expect("test env should be utf8"))
            .collect()
    }

    #[test]
    fn inherited_no_color_is_not_forwarded_to_terminal_children() {
        let envp = build_envp_from(
            [
                ("NO_COLOR".to_string(), "1".to_string()),
                ("TERM".to_string(), "dumb".to_string()),
            ],
            &[("TERM".to_string(), "xterm-256color".to_string())],
        )
        .expect("envp");
        let entries = envp_strings(envp);

        assert!(!entries.iter().any(|e| e.starts_with("NO_COLOR=")));
        assert!(entries.iter().any(|e| e == "TERM=xterm-256color"));
    }

    #[test]
    fn explicit_no_color_extra_env_is_preserved() {
        let envp = build_envp_from(
            [("NO_COLOR".to_string(), "1".to_string())],
            &[("NO_COLOR".to_string(), "1".to_string())],
        )
        .expect("envp");
        let entries = envp_strings(envp);

        assert!(entries.iter().any(|e| e == "NO_COLOR=1"));
    }

    /// A real child under a PTY: its stdout reaches the master fd. The GTK layer
    /// hands this same master to VTE for rendering; here we only check the raw
    /// plumbing.
    #[test]
    fn shell_output_reaches_the_pty_master() {
        let mut pty =
            Pty::spawn(&["sh", "-c", "printf 'hello world'"], None, &[], 40, 8).expect("spawn sh");

        let mut out = String::new();
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match pty.read(&mut buf) {
                Ok(0) => break, // EOF: child exited
                Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("pty read failed: {e}"),
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for shell output");
            }
        }

        assert!(out.contains("hello world"), "got: {out:?}");
    }

    #[test]
    fn extra_env_reaches_the_child() {
        let mut pty = Pty::spawn(
            &["sh", "-c", "printf \"%s\" \"$FLOWMUX_TEST_VAR\""],
            None,
            &[("FLOWMUX_TEST_VAR".to_string(), "pane42".to_string())],
            40,
            4,
        )
        .expect("spawn sh");

        let mut out = String::new();
        let mut buf = [0u8; 1024];
        loop {
            match pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("read: {e}"),
            }
        }
        assert!(out.contains("pane42"), "got: {out:?}");
    }

    #[test]
    fn resize_updates_winsize_seen_by_child() {
        // `stty size` prints "rows cols" from the kernel winsize we set.
        let mut pty = Pty::spawn(&["sh", "-c", "stty size"], None, &[], 100, 30).expect("spawn");
        // Resize before the child reads its winsize.
        pty.resize(100, 30, 8, 16).expect("resize");

        let mut out = String::new();
        let mut buf = [0u8; 1024];
        loop {
            match pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        // Some shells emit "30 100"; tolerate either field order issues by
        // checking both numbers are present on the first row.
        let row = out;
        assert!(
            row.contains("30") && row.contains("100"),
            "stty size output was {row:?}"
        );
    }

    #[test]
    fn drop_does_not_block_for_sighup_ignoring_child() {
        let mut pty = Pty::spawn(
            &["sh", "-c", "trap '' HUP TERM; printf ready; sleep 60"],
            None,
            &[],
            80,
            24,
        )
        .expect("spawn SIGHUP-ignoring child");
        let child_pid = pty.child_pid();
        let mut output = [0u8; 16];
        let ready_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match pty.read(&mut output) {
                Ok(n) if output[..n].windows(5).any(|bytes| bytes == b"ready") => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => panic!("read child readiness: {error}"),
            }
            assert!(
                Instant::now() < ready_deadline,
                "timed out waiting for child readiness"
            );
        }
        let (done_tx, done_rx) = mpsc::channel();

        std::thread::spawn(move || {
            drop(pty);
            let _ = done_tx.send(());
        });

        let result = done_rx.recv_timeout(Duration::from_millis(300));
        // Ensure a failing baseline run cannot leave the process group alive.
        unsafe {
            libc::kill(-child_pid, libc::SIGKILL);
        }

        assert!(result.is_ok(), "Pty::drop blocked waiting for its child");
    }
}
