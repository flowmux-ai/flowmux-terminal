// SPDX-License-Identifier: GPL-3.0-or-later
//! `flowmuxctl pty-tee` — PTY proxy that snoops OSC notifications.
//!
//! ## Why this exists
//!
//! Some terminal-widget stacks do not expose OSC 9 / 99 / 777 notifications
//! as structured GUI events. flowmux therefore keeps notification parsing in
//! a PTY-side sniffer instead of tying it to a toolkit-specific signal.
//!
//! `pty-tee` is that sniffer. It is invoked transparently as the
//! "shell" command by `terminal_pane::spawn`, runs as the terminal pane's
//! child process, and sits between the pane's outer PTY and the user's real
//! shell:
//!
//! ```text
//!     terminal pane  <-->  outer PTY (stdin/stdout)  <-->  pty-tee  <-->  inner PTY  <-->  shell
//!                                                  |
//!                                                  +--> OscExtractor --> Request::Notify
//! ```
//!
//! Bytes flow verbatim except for cursor-key normalization while a
//! foreground TUI has enabled application cursor mode (`smkx` /
//! DECCKM). The inner-to-outer half is also fed to
//! `flowmux_notify::OscExtractor`, and any parsed OSC notification is
//! forwarded to the daemon over the existing IPC socket. The shell's
//! view (its `tty`, its termios, its environment) is unchanged from a
//! direct terminal spawn.
//!
//! ## Why a separate process
//!
//! * The terminal pane owns the outer PTY while pty-tee owns the inner
//!   master. This keeps OSC parsing independent of the renderer.
//! * Crashes / OOMs in the tee never take down the GUI.
//! * The same binary works on Ubuntu 22.04 and 24.04 because we never
//!   touch a renderer-version-specific API.

use anyhow::{anyhow, Context};
use flowmux_core::{NotificationLevel, PaneId, SurfaceId};
use flowmux_ipc::{client::Client, protocol::Request};
use flowmux_notify::{osc::parse_osc, OscExtractor};
use flowmux_terminal::TerminalInputModes;
use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::termios::{self, SetArg, Termios};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{execvp, fork, setsid, ForkResult, Pid};
use std::cell::RefCell;
use std::ffi::{CString, OsString};
use std::io::Write;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Self-pipe write end. Signal handlers wake the I/O loop by writing
/// one byte here; the loop polls the read end together with the PTY
/// fds. A self-pipe is the only async-signal-safe way to compose
/// `poll` with arbitrary signals on stable Rust.
static SIGNAL_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn signal_wakeup_handler(_sig: libc::c_int) {
    let fd = SIGNAL_PIPE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        let buf = [0u8; 1];
        // Best-effort write; EAGAIN is fine — the loop will wake on
        // the byte we already buffered. Async-signal-safe by design.
        unsafe {
            libc::write(fd, buf.as_ptr() as *const _, 1);
        }
    }
}

/// One snooped OSC notification, dispatched on the IPC worker thread.
struct NotifyEvent {
    title: String,
    body: String,
    level: NotificationLevel,
}

/// Public entry point used by `main.rs`. Returns the inner shell's exit
/// code so `flowmuxctl` can exit transparently — the terminal pane sees the
/// child exit at the user's expected status.
pub fn run(
    pane: Option<PaneId>,
    surface: Option<SurfaceId>,
    socket: Option<PathBuf>,
    child_argv: Vec<OsString>,
) -> anyhow::Result<i32> {
    if child_argv.is_empty() {
        return Err(anyhow!("pty-tee needs a child argv after `--`"));
    }

    // Resolve the daemon socket using the same precedence as the rest
    // of `flowmuxctl`: explicit --socket > FLOWMUX_SOCKET_PATH (env
    // injected by the GUI) > FLOWMUX_SOCKET (legacy) > XDG runtime.
    let socket = socket
        .or_else(|| std::env::var_os("FLOWMUX_SOCKET_PATH").map(PathBuf::from))
        .or_else(|| std::env::var_os("FLOWMUX_SOCKET").map(PathBuf::from))
        .unwrap_or_else(flowmux_config::paths::runtime_socket);

    // Spin up the IPC worker BEFORE the I/O loop starts so the first
    // OSC arriving in the very first millisecond doesn't get dropped.
    let (notify_tx, notify_rx) = mpsc::channel::<NotifyEvent>();
    let worker = std::thread::Builder::new()
        .name("flowmuxctl-pty-tee-ipc".into())
        .spawn(move || ipc_worker(socket, pane, surface, notify_rx))
        .context("spawn ipc worker thread")?;

    let result = run_pty_pump(child_argv, notify_tx);

    // Worker shuts down naturally when the sender half drops. Join so
    // the last in-flight Notify isn't truncated mid-write on exit.
    let _ = worker.join();

    result
}

fn run_pty_pump(
    child_argv: Vec<OsString>,
    notify_tx: mpsc::Sender<NotifyEvent>,
) -> anyhow::Result<i32> {
    // 1. Allocate the inner PTY pair the shell will live on.
    let OpenptyResult { master, slave } = openpty(None, None).context("openpty for inner shell")?;

    // 2. Mirror the outer terminal size onto the inner PTY *before*
    //    fork so the shell starts with the right $LINES/$COLUMNS and
    //    avoids the "shell prints `$ ` then immediately reflows on
    //    first SIGWINCH" flicker.
    if let Some(ws) = winsize_from_fd(libc::STDIN_FILENO) {
        let _ = set_winsize(master.as_raw_fd(), &ws);
    }

    // 3. Fork. The child reparents into a fresh session, attaches the
    //    inner slave as its controlling terminal, and execs the user's
    //    shell argv. The parent stays in the I/O loop.
    let child_pid = match unsafe { fork() }.context("fork inner shell")? {
        ForkResult::Child => {
            // Child path: never returns. child_exec is `-> !`.
            drop(master);
            child_exec(slave, child_argv);
        }
        ForkResult::Parent { child } => {
            drop(slave);
            child
        }
    };

    // 4. Save outer termios (so we can restore on every exit path —
    //    panic, error, child crash) and switch the outer PTY to raw
    //    mode. Without raw mode the kernel line-discipline buffers
    //    keystrokes until newline and Ctrl-C/Ctrl-D get translated
    //    into signals targeted at *us* instead of the shell.
    let _saved_termios = termios::tcgetattr(std::io::stdin())
        .ok()
        .map(SavedTermios::new);
    if let Some(ref s) = _saved_termios {
        let mut raw = s.termios.clone();
        termios::cfmakeraw(&mut raw);
        let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSAFLUSH, &raw);
    }

    // 5. Set up the self-pipe and signal handlers for SIGWINCH (window
    //    resize forwarded to inner) and SIGCHLD (so a quick-exiting
    //    child wakes the loop instead of blocking in poll for the
    //    full timeout).
    let (sig_r, sig_w) = nix::unistd::pipe().context("self-pipe for signal wakeup")?;
    set_nonblocking(sig_r.as_raw_fd())?;
    set_nonblocking(sig_w.as_raw_fd())?;
    SIGNAL_PIPE_WRITE.store(sig_w.as_raw_fd(), Ordering::Relaxed);

    let sa = nix::sys::signal::SigAction::new(
        SigHandler::Handler(signal_wakeup_handler),
        nix::sys::signal::SaFlags::SA_RESTART,
        nix::sys::signal::SigSet::empty(),
    );
    unsafe {
        let _ = signal::sigaction(Signal::SIGWINCH, &sa);
        let _ = signal::sigaction(Signal::SIGCHLD, &sa);
    }

    let master_fd = master.as_raw_fd();
    set_nonblocking(master_fd)?;
    set_nonblocking(libc::STDIN_FILENO)?;

    // 6. Pending-OSC queue lives behind a RefCell so the OscExtractor
    //    closure can push into it while the outer loop drains it
    //    immediately after each `feed()`. Single-threaded — no Mutex
    //    needed.
    let pending: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let pending_for_cb = pending.clone();
    let mut extractor = OscExtractor::new(move |payload| {
        pending_for_cb.borrow_mut().push(payload.to_string());
    });
    let mut input_modes = TerminalInputModes::default();
    let mut cwd_tracker = CwdOscTracker::default();
    cwd_tracker.emit_if_changed(child_pid);

    // 7. Pump loop.
    let mut buf = [0u8; 8192];
    let exit_code: i32;
    loop {
        let mut fds = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: master_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: sig_r.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];

        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err).context("poll on outer/inner/signal fds");
        }

        // 7a. Drain self-pipe and react to signals.
        if fds[2].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut sink = [0u8; 64];
            loop {
                let n = unsafe {
                    libc::read(sig_r.as_raw_fd(), sink.as_mut_ptr() as *mut _, sink.len())
                };
                if n <= 0 {
                    break;
                }
            }
            // Forward winsize on every wakeup. SIGWINCH is the common
            // case but it's cheap to refresh on SIGCHLD too.
            if let Some(ws) = winsize_from_fd(libc::STDIN_FILENO) {
                let _ = set_winsize(master_fd, &ws);
            }
            cwd_tracker.emit_if_changed(child_pid);
            // Reap the child non-blockingly. If it's gone we drain the
            // remaining inner-master bytes below and break.
            if let Some(code) = try_reap(child_pid) {
                drain_inner(master_fd, &mut extractor);
                flush_pending(&pending, &notify_tx);
                exit_code = code;
                break;
            }
        }

        // 7b. Outer (user keystrokes / terminal input) → inner shell.
        if fds[0].revents & libc::POLLIN != 0 {
            match read_some(libc::STDIN_FILENO, &mut buf) {
                ReadOutcome::Data(slice) => {
                    let input = input_modes.rewrite_input(slice);
                    if let Err(e) = write_all(master_fd, input.as_ref()) {
                        tracing::warn!(error = %e, "write to inner master failed; exiting");
                        let _ = signal::kill(child_pid, Signal::SIGHUP);
                        drain_inner(master_fd, &mut extractor);
                        flush_pending(&pending, &notify_tx);
                        exit_code = 130;
                        break;
                    }
                }
                ReadOutcome::WouldBlock => {}
                ReadOutcome::Eof => {
                    // The terminal pane closed the outer end.
                    // Send SIGHUP so the inner shell can clean up.
                    let _ = signal::kill(child_pid, Signal::SIGHUP);
                }
                ReadOutcome::Err(e) => {
                    tracing::warn!(error = %e, "read on outer stdin failed");
                }
            }
        }
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            let _ = signal::kill(child_pid, Signal::SIGHUP);
        }

        // 7c. Inner shell → outer terminal + OSC sniffer.
        if fds[1].revents & libc::POLLIN != 0 {
            match read_some(master_fd, &mut buf) {
                ReadOutcome::Data(slice) => {
                    input_modes.observe_output(slice);
                    extractor.feed(slice);
                    if let Err(e) = write_all(libc::STDOUT_FILENO, slice) {
                        tracing::warn!(error = %e, "write to outer stdout failed; exiting");
                        let _ = signal::kill(child_pid, Signal::SIGHUP);
                        drain_inner(master_fd, &mut extractor);
                        flush_pending(&pending, &notify_tx);
                        exit_code = 130;
                        break;
                    }
                    flush_pending(&pending, &notify_tx);
                    cwd_tracker.emit_if_changed(child_pid);
                }
                ReadOutcome::WouldBlock => {}
                ReadOutcome::Eof | ReadOutcome::Err(_) => {
                    let code = wait_blocking(child_pid).unwrap_or(0);
                    drain_inner(master_fd, &mut extractor);
                    flush_pending(&pending, &notify_tx);
                    exit_code = code;
                    break;
                }
            }
        }
        if fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0
            && fds[1].revents & libc::POLLIN == 0
        {
            drain_inner(master_fd, &mut extractor);
            flush_pending(&pending, &notify_tx);
            let code = wait_blocking(child_pid).unwrap_or(0);
            exit_code = code;
            break;
        }
    }

    // 8. Cleanup. _saved_termios restores via Drop on the way out.
    SIGNAL_PIPE_WRITE.store(-1, Ordering::Relaxed);
    drop(extractor);
    drop(_saved_termios);

    Ok(exit_code)
}

// ---- Child path: exec the shell on the inner slave ----------------

fn child_exec(slave: OwnedFd, argv: Vec<OsString>) -> ! {
    // setsid drops our parent's controlling terminal so the next
    // TIOCSCTTY actually attaches the inner slave. Failing here is a
    // catastrophic configuration error — exit so the parent observes
    // a non-zero status instead of a zombie.
    if let Err(e) = setsid() {
        let msg = format!("flowmuxctl pty-tee: setsid failed: {e}\n");
        let _ = std::io::stderr().write_all(msg.as_bytes());
        unsafe { libc::_exit(125) };
    }

    let slave_fd = slave.into_raw_fd();
    if unsafe { libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) } < 0 {
        let err = std::io::Error::last_os_error();
        let msg = format!("flowmuxctl pty-tee: TIOCSCTTY failed: {err}\n");
        let _ = std::io::stderr().write_all(msg.as_bytes());
        unsafe { libc::_exit(125) };
    }

    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if slave_fd != target && unsafe { libc::dup2(slave_fd, target) } < 0 {
            unsafe { libc::_exit(125) };
        }
    }
    if slave_fd > libc::STDERR_FILENO {
        unsafe {
            libc::close(slave_fd);
        }
    }

    // Build NUL-terminated C argv.
    let cstr_argv: Vec<CString> = argv
        .into_iter()
        .filter_map(|s| CString::new(s.into_vec()).ok())
        .collect();
    if cstr_argv.is_empty() {
        unsafe { libc::_exit(125) };
    }
    let argv_refs: Vec<&std::ffi::CStr> = cstr_argv.iter().map(|c| c.as_c_str()).collect();
    match execvp(argv_refs[0], &argv_refs) {
        Ok(_) => unreachable!(),
        Err(e) => {
            let msg = format!(
                "flowmuxctl pty-tee: execvp({:?}) failed: {}\n",
                argv_refs[0], e
            );
            let _ = std::io::stderr().write_all(msg.as_bytes());
            unsafe { libc::_exit(127) };
        }
    }
}

// ---- IPC worker thread -------------------------------------------

fn ipc_worker(
    socket: PathBuf,
    pane: Option<PaneId>,
    surface: Option<SurfaceId>,
    rx: mpsc::Receiver<NotifyEvent>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(error = %e, "ipc worker: tokio runtime; notifications disabled");
            for _ in rx.iter() {}
            return;
        }
    };

    rt.block_on(async move {
        let client = match connect_with_retry(&socket).await {
            Some(c) => c,
            None => {
                tracing::warn!(
                    socket = %socket.display(),
                    "ipc worker: daemon unreachable; notifications disabled"
                );
                for _ in rx.iter() {}
                return;
            }
        };

        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(ev) => {
                    let req = Request::Notify {
                        pane,
                        surface,
                        title: ev.title,
                        body: ev.body,
                        level: ev.level,
                    };
                    if let Err(e) = client.call(req).await {
                        tracing::warn!(error = %e, "ipc worker: notify call failed");
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}

async fn connect_with_retry(socket: &std::path::Path) -> Option<Client> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut delay = Duration::from_millis(50);
    loop {
        match Client::connect(socket).await {
            Ok(c) => return Some(c),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(500));
            }
            Err(_) => return None,
        }
    }
}

// ---- Helpers -----------------------------------------------------

fn flush_pending(pending: &Rc<RefCell<Vec<String>>>, tx: &mpsc::Sender<NotifyEvent>) {
    let drained: Vec<String> = pending.borrow_mut().drain(..).collect();
    for payload in drained {
        if let Some(n) = parse_osc(&payload) {
            // Best-effort send. Worker may have shut down on a fatal
            // error; in that case the notification is silently dropped
            // — better than blocking the I/O loop.
            let _ = tx.send(NotifyEvent {
                title: n.title,
                body: n.body,
                level: n.level,
            });
        }
    }
}

fn drain_inner<F: FnMut(&str)>(master_fd: RawFd, extractor: &mut OscExtractor<F>) {
    let mut buf = [0u8; 4096];
    while let ReadOutcome::Data(slice) = read_some(master_fd, &mut buf) {
        extractor.feed(slice);
        let _ = write_all(libc::STDOUT_FILENO, slice);
    }
}

enum ReadOutcome<'a> {
    Data(&'a [u8]),
    WouldBlock,
    Eof,
    Err(std::io::Error),
}

fn read_some<'a>(fd: RawFd, buf: &'a mut [u8]) -> ReadOutcome<'a> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
    if n > 0 {
        ReadOutcome::Data(&buf[..n as usize])
    } else if n == 0 {
        ReadOutcome::Eof
    } else {
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // EAGAIN == EWOULDBLOCK on every Linux libc; listing both
            // would be an unreachable arm.
            Some(libc::EAGAIN) | Some(libc::EINTR) => ReadOutcome::WouldBlock,
            Some(libc::EIO) => ReadOutcome::Eof, // master after slave hangup
            _ => ReadOutcome::Err(err),
        }
    }
}

fn write_all(fd: RawFd, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr() as *const _, data.len()) };
        if n > 0 {
            data = &data[n as usize..];
            continue;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            // EAGAIN == EWOULDBLOCK on every Linux libc.
            Some(libc::EAGAIN) => {
                // Tiny spin — the receiver (kernel pty buffer or terminal pane)
                // drains within microseconds in practice.
                std::thread::yield_now();
                continue;
            }
            _ => return Err(err),
        }
    }
    Ok(())
}

#[derive(Default)]
struct CwdOscTracker {
    last: Option<PathBuf>,
}

impl CwdOscTracker {
    fn emit_if_changed(&mut self, pid: Pid) {
        let Some(cwd) = child_cwd(pid) else {
            return;
        };
        if self.last.as_ref() == Some(&cwd) {
            return;
        }

        let seq = osc7_for_path(&cwd);
        if write_all(libc::STDOUT_FILENO, &seq).is_ok() {
            self.last = Some(cwd);
        }
    }
}

fn osc7_for_path(path: &Path) -> Vec<u8> {
    let mut seq = b"\x1b]7;file://".to_vec();
    for &byte in path.as_os_str().as_bytes() {
        if is_file_uri_path_byte(byte) {
            seq.push(byte);
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            seq.push(b'%');
            seq.push(HEX[(byte >> 4) as usize]);
            seq.push(HEX[(byte & 0x0f) as usize]);
        }
    }
    seq.push(b'\x07');
    seq
}

fn is_file_uri_path_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~'
    )
}

#[cfg(target_os = "linux")]
fn child_cwd(pid: Pid) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{}/cwd", pid.as_raw())).ok()
}

#[cfg(target_os = "macos")]
fn child_cwd(pid: Pid) -> Option<PathBuf> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pidinfo(
            pid.as_raw(),
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int,
        )
    };
    if rc <= 0 {
        return None;
    }

    let bytes = unsafe {
        std::slice::from_raw_parts(
            info.pvi_cdir.vip_path.as_ptr() as *const u8,
            std::mem::size_of_val(&info.pvi_cdir.vip_path),
        )
    };
    let len = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    if len == 0 {
        return None;
    }

    Some(PathBuf::from(OsString::from_vec(bytes[..len].to_vec())))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn child_cwd(_pid: Pid) -> Option<PathBuf> {
    None
}

fn set_nonblocking(fd: RawFd) -> anyhow::Result<()> {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    let flags = fcntl(fd, FcntlArg::F_GETFL).context("fcntl F_GETFL")?;
    let new = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(new)).context("fcntl F_SETFL O_NONBLOCK")?;
    Ok(())
}

fn winsize_from_fd(fd: RawFd) -> Option<libc::winsize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 {
        Some(ws)
    } else {
        None
    }
}

fn set_winsize(fd: RawFd, ws: &libc::winsize) -> std::io::Result<()> {
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, ws) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn try_reap(pid: Pid) -> Option<i32> {
    match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::Exited(_, code)) => Some(code),
        Ok(WaitStatus::Signaled(_, sig, _)) => Some(128 + sig as i32),
        _ => None,
    }
}

fn wait_blocking(pid: Pid) -> Option<i32> {
    match waitpid(pid, None) {
        Ok(WaitStatus::Exited(_, code)) => Some(code),
        Ok(WaitStatus::Signaled(_, sig, _)) => Some(128 + sig as i32),
        _ => None,
    }
}

/// RAII guard that restores the outer terminal's termios on drop.
struct SavedTermios {
    termios: Termios,
}

impl SavedTermios {
    fn new(t: Termios) -> Self {
        Self { termios: t }
    }
}

impl Drop for SavedTermios {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSAFLUSH, &self.termios);
    }
}
