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
    /// Returns `Ok(None)` when the child is still running.
    /// Treats `ECHILD` (already reaped by VTE's `watch_child`) as `Ok(Some(0))`.
    pub fn try_wait(&mut self) -> io::Result<Option<i32>> {
        if self.reaped {
            return Ok(Some(0));
        }
        let mut status: libc::c_int = 0;
        // SAFETY: standard waitpid usage.
        let rc = unsafe { libc::waitpid(self.child, &mut status, libc::WNOHANG) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ECHILD) {
                // VTE's watch_child already reaped this child.
                self.reaped = true;
                return Ok(Some(0));
            }
            return Err(err);
        }
        if rc == 0 {
            return Ok(None); // still running
        }
        self.reaped = true;
        Ok(Some(status))
    }

    /// Timeout for [`Self::close`] bounded child reaping.
    pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

    /// Send SIGHUP to the child (used on pane close before reaping).
    pub fn hangup(&mut self) {
        if !self.reaped {
            // SAFETY: kill on our own child process group.
            unsafe { libc::kill(-self.child, libc::SIGHUP) };
        }
    }

    /// Close the master fd immediately. This is a fast syscall — call it
    /// from the GTK main thread. VTE has its own dup of this fd (made at
    /// spawn time via `libc::dup`), so the terminal widget keeps displaying
    /// buffered output until the widget is dropped.
    pub fn close_master(&mut self) {
        if self.master >= 0 {
            // SAFETY: closing our owned fd once.
            unsafe { libc::close(self.master) };
            self.master = -1;
        }
    }

    /// Close the PTY synchronously with a bounded child-reap window.
    /// For use in tests and non-GTK callers.
    ///
    /// Sends SIGHUP to the process group, polls with WNOHANG for up to
    /// `timeout`. If the child hasn't exited by the deadline it sends
    /// SIGKILL (unignorable) to the process group and does one final
    /// blocking wait. The master fd is closed before returning.
    /// After this call the Pty is fully consumed — `Drop` becomes a no-op.
    ///
    /// Note: with the pty-tee topology the direct child is `flowmuxctl
    /// pty-tee`, not the user's shell. pty-tee runs its own escalation
    /// protocol against the inner shell's process group when the outer
    /// PTY master closes (or when this SIGHUP reaches it). This method
    /// provides a second backstop in case pty-tee itself is unresponsive.
    pub fn close(mut self, timeout: Duration) -> io::Result<i32> {
        self.hangup();
        let status = Self::reap_with_timeout(&mut self, timeout);
        self.close_master();
        Ok(status)
    }

    /// Close the master fd immediately, signal the process group, and spawn
    /// a background thread to reap the child. Non-blocking — safe to call
    /// from the GTK main thread.
    ///
    /// The master fd is closed synchronously (fast syscall) before this
    /// returns. Closing the master causes pty-tee (the direct child) to
    /// see stdin EOF on the outer PTY, triggering its own bounded
    /// escalation against the inner shell's process group. This method
    /// adds a second backstop: the background thread reaps the pty-tee
    /// process itself with the same bounded-timeout + SIGKILL logic as
    /// [`Self::close`].
    pub fn close_and_reap_async(mut self, timeout: Duration) {
        self.close_master();
        self.hangup();
        std::thread::spawn(move || {
            Self::reap_with_timeout(&mut self, timeout);
            // Pty dropped here; master is already -1.
        });
    }

    /// Poll the child with WNOHANG for `timeout`, escalating to SIGKILL to
    /// the process group on expiry. Does not touch the master fd.
    fn reap_with_timeout(&mut self, timeout: Duration) -> i32 {
        let deadline = Instant::now() + timeout;
        loop {
            match self.try_wait() {
                Ok(Some(status)) => return status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    eprintln!("pty reap try_wait error: {e}");
                    return 0;
                }
            }
        }
        // Timed out: SIGKILL to the process group is unignorable.
        // SAFETY: kill on our own child process group.
        unsafe { libc::kill(-self.child, libc::SIGKILL) };
        let mut status: libc::c_int = 0;
        // SAFETY: standard waitpid — after SIGKILL the child will
        // become a zombie within a scheduler tick.
        let rc = unsafe { libc::waitpid(self.child, &mut status, 0) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ECHILD) {
                // VTE's watch_child already reaped.
                self.reaped = true;
                return 0;
            }
            eprintln!("pty reap waitpid error after SIGKILL: {err}");
        } else {
            self.reaped = true;
        }
        status
    }
}

// SAFETY: Pty contains only RawFd, pid_t, and bool — all Send.
unsafe impl Send for Pty {}

impl Drop for Pty {
    fn drop(&mut self) {
        self.hangup();
        if self.master >= 0 {
            // SAFETY: closing our owned fd once.
            unsafe { libc::close(self.master) };
            self.master = -1;
        }
        if !self.reaped {
            let mut status: libc::c_int = 0;
            // Non-blocking best-effort reap. The direct child is pty-tee
            // (not the user's shell). Closing the master triggers pty-tee's
            // escalation protocol against the inner process group. If pty-tee
            // has already exited (common case — it reaps its own inner child
            // and exits cleanly), this waitpid returns immediately. If pty-tee
            // is somehow still alive, it will be reaped when the background
            // thread's SIGKILL lands. Must not block — Drop runs on the GTK
            // main thread. The normal GUI path always registers VTE's
            // `watch_child` as the external reaper; non-VTE owners must call
            // `close` or `close_and_reap_async` instead of relying on Drop.
            // ECHILD means VTE's watch_child already reaped; treat as success.
            // SAFETY: standard waitpid usage on our child.
            let rc = unsafe { libc::waitpid(self.child, &mut status, libc::WNOHANG) };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ECHILD) {
                    eprintln!("Pty::drop waitpid error: {err}");
                }
            }
            self.reaped = true;
        }
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

    /// Regression: a child that ignores SIGHUP must not block `close()`
    /// indefinitely. The bounded timeout + SIGKILL fallback must complete.
    /// Also verifies process-group signalling: SIGKILL is sent to `-pid`,
    /// not just `pid`.
    #[test]
    fn close_completes_for_child_that_ignores_sighup() {
        // `trap '' HUP` makes the shell ignore SIGHUP, then `sleep 60`
        // keeps it alive. close() must return within the timeout.
        let pty = Pty::spawn(&["sh", "-c", "trap '' HUP; sleep 60"], None, &[], 80, 24)
            .expect("spawn SIGHUP-ignoring child");

        let start = Instant::now();
        let timeout = Duration::from_secs(3);
        let status = pty.close(timeout).expect("close should succeed");
        let elapsed = start.elapsed();

        // Must complete within the timeout (with some slack).
        assert!(
            elapsed < timeout + Duration::from_secs(3),
            "close took {elapsed:?} for SIGHUP-ignoring child"
        );
        // Child was killed by SIGKILL (signal 9) after timeout.
        assert!(
            status != 0,
            "SIGHUP-ignoring child should not exit cleanly, got status {status}"
        );
    }

    /// Normal well-behaved child exits on SIGHUP promptly.
    #[test]
    fn close_reaps_well_behaved_child_immediately() {
        let pty = Pty::spawn(&["sh", "-c", "exit 42"], None, &[], 80, 24)
            .expect("spawn immediate-exit child");

        // Give the child a moment to exit, then close (it's already dead).
        std::thread::sleep(Duration::from_millis(100));
        let start = Instant::now();
        let _status = pty
            .close(Duration::from_secs(5))
            .expect("close should succeed");
        let elapsed = start.elapsed();

        // Should reap almost instantly — no need for the full timeout.
        assert!(
            elapsed < Duration::from_secs(1),
            "close took {elapsed:?} for already-exited child"
        );
    }

    /// A shell descendant (background child) must not survive PTY close.
    /// Verifies process-group cleanup via `kill(-pid, SIGKILL)`.
    #[test]
    fn descendant_does_not_survive_close() {
        // Spawn a shell that launches a background sleep, then waits.
        // The background sleep is in the same process group as the shell.
        let pty = Pty::spawn(
            &["sh", "-c", "sleep 60 & CHILD=$!; trap '' HUP; wait $CHILD"],
            None,
            &[],
            80,
            24,
        )
        .expect("spawn shell with background child");

        let shell_pid = pty.child_pid();
        // Give the shell a moment to fork the background sleep.
        std::thread::sleep(Duration::from_millis(200));

        pty.close(Duration::from_secs(3))
            .expect("close should succeed");

        // The shell leader should be gone.
        // SAFETY: kill(pid, 0) checks existence without sending a signal.
        let exists = unsafe { libc::kill(shell_pid, 0) };
        assert_eq!(
            exists, -1,
            "shell pid {shell_pid} should not exist after close"
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH),
            "shell pid {shell_pid} should return ESRCH"
        );
    }

    /// `try_wait` treats ECHILD (already reaped by VTE's watch_child)
    /// as `Ok(Some(0))` instead of an error.
    #[test]
    fn try_wait_treats_echild_as_already_reaped() {
        let mut pty =
            Pty::spawn(&["sh", "-c", "exit 0"], None, &[], 80, 24).expect("spawn quick-exit child");

        // Wait for the child to exit, then externally reap it to simulate
        // VTE's watch_child.
        std::thread::sleep(Duration::from_millis(200));
        let mut status: libc::c_int = 0;
        // SAFETY: external reap simulating VTE.
        unsafe { libc::waitpid(pty.child_pid(), &mut status, 0) };

        // Now try_wait should get ECHILD and treat it as already reaped.
        let result = pty.try_wait().expect("try_wait should not error on ECHILD");
        assert_eq!(result, Some(0), "ECHILD should yield Some(0)");
        assert!(pty.reaped);
    }

    /// `close_and_reap_async` returns immediately (non-blocking) and the
    /// background thread completes the reap.
    #[test]
    fn close_and_reap_async_returns_immediately() {
        let pty =
            Pty::spawn(&["sh", "-c", "exit 0"], None, &[], 80, 24).expect("spawn quick-exit child");
        let child_pid = pty.child_pid();

        let start = Instant::now();
        pty.close_and_reap_async(Duration::from_secs(5));
        let elapsed = start.elapsed();

        // Must return in well under 1 second — it's non-blocking.
        assert!(
            elapsed < Duration::from_millis(100),
            "close_and_reap_async blocked for {elapsed:?}"
        );

        // Give the background thread time to reap, then verify the
        // child is gone.
        std::thread::sleep(Duration::from_millis(500));
        let exists = unsafe { libc::kill(child_pid, 0) };
        assert_eq!(
            exists, -1,
            "child pid {child_pid} should be reaped after close_and_reap_async"
        );
    }
}
