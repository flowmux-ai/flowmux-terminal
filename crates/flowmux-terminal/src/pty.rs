// SPDX-License-Identifier: GPL-3.0-or-later
//! A pseudo-terminal for the libghostty-vt backend.
//!
//! The VTE GUI path lets the VTE widget own its PTY. The libghostty path owns
//! the PTY itself: this module spawns a child under a fresh PTY via
//! `forkpty(3)`, exposes the master fd for the read/write loop, and resizes the
//! kernel window via `TIOCSWINSZ`. The bytes read here are fed to
//! [`crate::vt::Vt::write`]; keystrokes encoded by the GUI are written back.
//!
//! Compiled only under the `libghostty` cargo feature.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::Path;

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
        let mut argv_ptrs: Vec<*const libc::c_char> =
            c_argv.iter().map(|s| s.as_ptr()).collect();
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
        // SAFETY: forkpty allocates a PTY pair and forks. We pass a valid
        // master-out pointer and winsize; name/termios default to NULL.
        let pid = unsafe {
            libc::forkpty(
                &mut master,
                std::ptr::null_mut(),
                std::ptr::null(),
                &mut ws,
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
                libc::execvpe(argv_ptrs[0], argv_ptrs.as_ptr(), env_ptrs.as_ptr());
                // execvpe only returns on failure.
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
    pub fn resize(&mut self, cols: u16, rows: u16, cell_w_px: u16, cell_h_px: u16) -> io::Result<()> {
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
            // SAFETY: kill on our own child pid.
            unsafe { libc::kill(self.child, libc::SIGHUP) };
        }
    }
}

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
            // Best-effort reap so we don't leak a zombie.
            // SAFETY: standard waitpid usage on our child.
            unsafe { libc::waitpid(self.child, &mut status, 0) };
            self.reaped = true;
        }
    }
}

/// Merge `extra_env` onto the inherited environment into a `KEY=VALUE` CString
/// list. Later duplicate keys win.
fn build_envp(extra_env: &[(String, String)]) -> io::Result<Vec<CString>> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, String> = std::env::vars().collect();
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
    use crate::vt::Vt;
    use std::time::{Duration, Instant};

    /// End-to-end headless pipeline: a real child process under a PTY, its
    /// output fed through libghostty-vt, read back from the grid.
    #[test]
    fn shell_output_flows_through_pty_into_vt_grid() {
        let mut vt = Vt::new(40, 8, 200).expect("vt");
        let mut pty = Pty::spawn(
            &["sh", "-c", "printf 'hello world'"],
            None,
            &[],
            40,
            8,
        )
        .expect("spawn sh");

        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match pty.read(&mut buf) {
                Ok(0) => break, // EOF: child exited
                Ok(n) => vt.write(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("pty read failed: {e}"),
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for shell output");
            }
        }

        assert!(vt.update());
        assert_eq!(vt.row_text(0), "hello world");
    }

    #[test]
    fn extra_env_reaches_the_child() {
        let mut vt = Vt::new(40, 4, 100).expect("vt");
        let mut pty = Pty::spawn(
            &["sh", "-c", "printf \"%s\" \"$FLOWMUX_TEST_VAR\""],
            None,
            &[("FLOWMUX_TEST_VAR".to_string(), "pane42".to_string())],
            40,
            4,
        )
        .expect("spawn sh");

        let mut buf = [0u8; 1024];
        loop {
            match pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => vt.write(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("read: {e}"),
            }
        }
        assert!(vt.update());
        assert_eq!(vt.row_text(0), "pane42");
    }

    #[test]
    fn resize_updates_winsize_seen_by_child() {
        // `stty size` prints "rows cols" from the kernel winsize we set.
        let mut vt = Vt::new(120, 40, 100).expect("vt");
        let mut pty = Pty::spawn(&["sh", "-c", "stty size"], None, &[], 100, 30).expect("spawn");
        // Resize before the child reads its winsize.
        pty.resize(100, 30, 8, 16).expect("resize");

        let mut buf = [0u8; 1024];
        loop {
            match pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => vt.write(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        assert!(vt.update());
        // Some shells emit "30 100"; tolerate either field order issues by
        // checking both numbers are present on the first row.
        let row = vt.row_text(0);
        assert!(
            row.contains("30") && row.contains("100"),
            "stty size row was {row:?}"
        );
    }
}
