// SPDX-License-Identifier: GPL-3.0-or-later
//! Integration test for `flowmuxctl pty-tee`.
//!
//! Spawns the real `flowmuxctl` binary as a PTY proxy in front of a
//! tiny shell command that emits OSC 9 / 99 / 777 escapes, and asserts
//! that a fake daemon listening on a Unix socket receives the matching
//! `Request::Notify` envelopes — i.e. the end-to-end path from
//! "agent prints OSC into the terminal" all the way to
//! "daemon's notify handler is invoked" is wired up.
//!
//! This is the regression guard the user asked for after we discovered
//! legacy terminal-widget paths silently swallowed these escapes.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use libc;

fn flowmuxctl_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_flowmuxctl"))
}

/// Spin up a fake daemon on `socket` that:
/// * Records every JSON envelope received.
/// * Replies to `notify` requests with a `notified` response so the
///   tee's IPC client doesn't error out.
///
/// Returns a receiver that yields each captured envelope as a String.
fn spawn_fake_daemon(socket: PathBuf) -> mpsc::Receiver<String> {
    let listener = UnixListener::bind(&socket).expect("bind fake daemon socket");
    listener
        .set_nonblocking(false)
        .expect("blocking accept on fake daemon");
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("fake-daemon-acceptor".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { continue };
                let tx = tx.clone();
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                    let mut writer = stream;
                    let mut next_id: u64 = 1;
                    loop {
                        let mut line = String::new();
                        match reader.read_line(&mut line) {
                            Ok(0) => break,
                            Ok(_) => {}
                            Err(_) => break,
                        }
                        // Forward the raw line to the test thread.
                        let _ = tx.send(line.trim_end().to_string());
                        // Reply with a minimal `notified` envelope so
                        // the tee's IPC client treats the call as
                        // successful and stays connected for the next
                        // OSC.
                        let env = serde_json::json!({
                            "id": next_id,
                            "kind": "response",
                            "notified": { "desktop_id": "desktop-1" }
                        });
                        next_id += 1;
                        // Best-effort write; the tee may have closed
                        // the connection if its child exited fast.
                        let mut s = serde_json::to_string(&env).unwrap();
                        s.push('\n');
                        let _ = writer.write_all(s.as_bytes());
                    }
                });
            }
        })
        .expect("spawn acceptor thread");
    rx
}

/// Run the tee with a child that emits `osc_payload` (between BEL
/// terminators) and return all envelopes the fake daemon recorded
/// before the child exited.
fn run_tee_with_osc(osc_payloads: &[&str]) -> Vec<String> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket = tmp.path().join("flowmux.sock");
    let rx = spawn_fake_daemon(socket.clone());

    // Build a tiny sh script that prints each OSC escape, surrounds
    // them with normal terminal output to make sure non-OSC bytes
    // pass through cleanly, and exits 0.
    //
    // Using printf with `\033` so we don't have to worry about shell
    // quoting differences across distributions.
    let mut script = String::from("printf 'pre\\n'; ");
    for p in osc_payloads {
        // BEL-terminated. parse_osc handles both BEL and ST terminators
        // uniformly inside the OscExtractor stream.
        script.push_str(&format!("printf '\\033]{}\\007'; ", p));
    }
    script.push_str("printf 'post\\n'; sleep 0.1; exit 0");

    let pane_id = "11111111-1111-1111-1111-111111111111";
    let surface_id = "22222222-2222-2222-2222-222222222222";

    let mut cmd = Command::new(flowmuxctl_path());
    cmd.arg("pty-tee")
        .arg("--pane")
        .arg(pane_id)
        .arg("--surface")
        .arg(surface_id)
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(&script)
        .env("FLOWMUX_SOCKET_PATH", &socket)
        // Suppress the tracing output so test logs stay clean.
        .env("FLOWMUX_LOG", "off")
        // Keep stdin a live pipe (not /dev/null) so the tee does not
        // see an immediate EOF and HUP the child. Real terminal keeps the
        // outer end open for the lifetime of the pane; the test must
        // mimic that by holding ChildStdin until after the child
        // finishes.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn flowmuxctl pty-tee");
    let _stdin_keepalive = child.stdin.take();

    let status = child.wait().expect("wait flowmuxctl pty-tee");
    drop(_stdin_keepalive);
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut e) = child.stderr.take() {
            use std::io::Read as _;
            let _ = e.read_to_string(&mut stderr);
        }
        panic!("pty-tee exited unsuccessfully: {status:?}\nstderr: {stderr}");
    }

    // Drain everything the fake daemon captured. Allow a short grace
    // window for the IPC worker thread to flush its last in-flight
    // notify before we close down.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut envelopes = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(line) => envelopes.push(line),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !envelopes.is_empty() {
                    // Got at least one — do a brief drain pass and stop.
                    while let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
                        envelopes.push(line);
                    }
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    envelopes
}

fn run_tee_capture_stdout(script: &str, test_cwd: &Path) -> Vec<u8> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket = tmp.path().join("flowmux.sock");
    let _rx = spawn_fake_daemon(socket.clone());

    let mut cmd = Command::new(flowmuxctl_path());
    cmd.arg("pty-tee")
        .arg("--pane")
        .arg("11111111-1111-1111-1111-111111111111")
        .arg("--surface")
        .arg("22222222-2222-2222-2222-222222222222")
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(script)
        .env("FLOWMUX_SOCKET_PATH", &socket)
        .env("FLOWMUX_LOG", "off")
        .env("FLOWMUX_TEST_CWD", test_cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn flowmuxctl pty-tee");
    let stdin_keepalive = child.stdin.take();
    let mut stdout = child.stdout.take().expect("pty-tee stdout");
    let stdout_thread = thread::spawn(move || {
        let mut output = Vec::new();
        stdout
            .read_to_end(&mut output)
            .expect("read pty-tee stdout");
        output
    });

    let status = child.wait().expect("wait flowmuxctl pty-tee");
    drop(stdin_keepalive);
    let output = stdout_thread.join().expect("join stdout reader");
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut stderr);
        }
        panic!("pty-tee exited unsuccessfully: {status:?}\nstderr: {stderr}");
    }

    output
}

fn osc7_for_test_path(path: &Path) -> Vec<u8> {
    let mut seq = b"\x1b]7;file://".to_vec();
    for &byte in path.as_os_str().as_bytes() {
        if matches!(
            byte,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~'
        ) {
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

#[test]
fn pty_tee_emits_osc7_when_child_cwd_changes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().join("cwd with spaces");
    std::fs::create_dir(&cwd).expect("create target cwd");

    let output = run_tee_capture_stdout(
        "cd \"$FLOWMUX_TEST_CWD\"; printf 'READY\\n'; sleep 0.2; exit 0",
        &cwd,
    );
    let expected_cwd = cwd.canonicalize().unwrap_or(cwd);
    let expected = osc7_for_test_path(&expected_cwd);

    assert!(
        output
            .windows(expected.len())
            .any(|window| window == expected),
        "expected stdout to contain OSC 7 for {}; stdout was {:?}",
        expected_cwd.display(),
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn application_cursor_mode_rewrites_normal_up_arrow_for_tig() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket = tmp.path().join("flowmux.sock");
    let _rx = spawn_fake_daemon(socket.clone());
    let script = r#"
import os, termios, tty

fd = 0
old = termios.tcgetattr(fd)
tty.setraw(fd)
try:
    os.write(1, b'\x1b[?1h\x1b=READY\n')
    data = os.read(0, 3)
    os.write(1, b'KEY:' + data.hex().encode('ascii') + b'\n')
finally:
    termios.tcsetattr(fd, termios.TCSANOW, old)
"#;

    let mut cmd = Command::new(flowmuxctl_path());
    cmd.arg("pty-tee")
        .arg("--pane")
        .arg("11111111-1111-1111-1111-111111111111")
        .arg("--surface")
        .arg("22222222-2222-2222-2222-222222222222")
        .arg("--")
        .arg("python3")
        .arg("-c")
        .arg(script)
        .env("FLOWMUX_SOCKET_PATH", &socket)
        .env("FLOWMUX_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn flowmuxctl pty-tee");
    let mut stdin = child.stdin.take().expect("pty-tee stdin");
    let mut stdout = child.stdout.take().expect("pty-tee stdout");
    let mut stderr = child.stderr.take().expect("pty-tee stderr");

    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !seen.ends_with(b"READY\n") {
        assert!(
            Instant::now() < deadline,
            "pty child did not announce READY; saw {seen:?}"
        );
        let mut byte = [0u8; 1];
        stdout.read_exact(&mut byte).expect("read READY byte");
        seen.push(byte[0]);
    }

    stdin
        .write_all(b"\x1b[A")
        .expect("send normal-mode Up arrow");

    let mut rest = Vec::new();
    stdout.read_to_end(&mut rest).expect("read pty output");
    let status = child.wait().expect("wait pty-tee");
    drop(stdin);
    let mut stderr_text = String::new();
    let _ = stderr.read_to_string(&mut stderr_text);
    assert!(
        status.success(),
        "pty-tee failed with {status:?}; stderr: {stderr_text}"
    );

    let output = String::from_utf8_lossy(&rest);
    assert!(
        output.contains("KEY:1b4f41"),
        "expected application-mode Up (ESC O A) after smkx, got output {output:?}"
    );
}

#[test]
fn osc_9_round_trips_to_daemon_notify_request() {
    let envelopes = run_tee_with_osc(&["9;Build complete"]);
    assert!(
        !envelopes.is_empty(),
        "fake daemon received nothing — pty-tee did not snoop OSC 9"
    );
    let notify = envelopes
        .iter()
        .find(|line| line.contains("\"verb\":\"notify\""))
        .expect("expected a notify envelope; got: {envelopes:?}");
    assert!(
        notify.contains("\"body\":\"Build complete\""),
        "notify envelope missing OSC 9 body: {notify}"
    );
    // OSC 9 maps to Info unless the body contains a "waiting" marker.
    assert!(
        notify.contains("\"level\":\"info\""),
        "OSC 9 'Build complete' should map to NotificationLevel::Info: {notify}"
    );
}

#[test]
fn osc_99_promotes_to_needs_input_when_body_says_waiting() {
    let envelopes = run_tee_with_osc(&["99;urgency=critical;Claude is waiting for your input"]);
    assert!(
        !envelopes.is_empty(),
        "fake daemon received nothing for OSC 99"
    );
    let notify = envelopes
        .iter()
        .find(|line| line.contains("\"verb\":\"notify\""))
        .expect("expected a notify envelope");
    assert!(
        notify.contains("waiting for your input"),
        "missing OSC 99 body: {notify}"
    );
    assert!(
        notify.contains("\"level\":\"needs_input\"")
            || notify.contains("\"level\":\"error\""),
        "OSC 99 with explicit critical urgency or 'waiting' marker should escalate above info: {notify}"
    );
}

#[test]
fn osc_777_urxvt_round_trips_with_summary_and_body() {
    let envelopes = run_tee_with_osc(&["777;notify;Codex;needs approval"]);
    assert!(
        !envelopes.is_empty(),
        "fake daemon received nothing for OSC 777"
    );
    let notify = envelopes
        .iter()
        .find(|line| line.contains("\"verb\":\"notify\""))
        .expect("expected a notify envelope");
    assert!(
        notify.contains("\"title\":\"Codex\""),
        "OSC 777 summary did not become title: {notify}"
    );
    assert!(
        notify.contains("\"body\":\"needs approval\""),
        "OSC 777 body missing: {notify}"
    );
}

#[test]
fn notify_stream_forwards_osc_payload_to_daemon_notify_request() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket = tmp.path().join("flowmux.sock");
    let rx = spawn_fake_daemon(socket.clone());
    let pane = "11111111-1111-1111-1111-111111111111";

    let mut child = Command::new(flowmuxctl_path())
        .arg("--socket")
        .arg(&socket)
        .arg("notify-stream")
        .arg("--pane")
        .arg(pane)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn flowmuxctl notify-stream");

    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"\x1b]777;notify;Codex;needs approval\x07")
        .expect("write OSC payload");

    let status = child.wait().expect("wait flowmuxctl notify-stream");
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut stderr);
        }
        panic!("notify-stream exited unsuccessfully: {status:?}\nstderr: {stderr}");
    }

    let notify = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("fake daemon should receive notify request");
    assert!(
        notify.contains("\"verb\":\"notify\""),
        "expected notify envelope, got {notify}"
    );
    assert!(
        notify.contains(&format!("\"pane\":\"{pane}\"")),
        "notify-stream pane attribution missing: {notify}"
    );
    assert!(
        notify.contains("\"title\":\"Codex\""),
        "notify-stream title missing: {notify}"
    );
    assert!(
        notify.contains("\"body\":\"needs approval\""),
        "notify-stream body missing: {notify}"
    );
}

#[test]
fn pane_and_surface_ids_propagate_into_the_notify_envelope() {
    let envelopes = run_tee_with_osc(&["9;Routed"]);
    let notify = envelopes
        .iter()
        .find(|line| line.contains("\"verb\":\"notify\""))
        .expect("expected a notify envelope");
    assert!(
        notify.contains("\"pane\":\"11111111-1111-1111-1111-111111111111\""),
        "pane id did not propagate from --pane into the daemon envelope: {notify}"
    );
    assert!(
        notify.contains("\"surface\":\"22222222-2222-2222-2222-222222222222\""),
        "surface id did not propagate from --surface into the daemon envelope: {notify}"
    );
}

// ---- Bounded process-lifecycle regression --------------------------

fn with_deadline<T, F>(deadline: Instant, step: Duration, mut probe: F) -> Option<T>
where
    F: FnMut() -> Option<T>,
{
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        thread::sleep(step.min(remaining));
    }
}

fn set_nonblocking(fd: &impl AsRawFd) {
    let fd = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(
        flags >= 0,
        "F_GETFL failed: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0,
        "F_SETFL(O_NONBLOCK) failed: {}",
        std::io::Error::last_os_error()
    );
}

fn process_state(pid: libc::pid_t) -> Option<u8> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let close_paren = stat.windows(2).rposition(|pair| pair == b") ")?;
    stat.get(close_paren + 2).copied()
}

fn process_is_running(pid: libc::pid_t) -> bool {
    match process_state(pid) {
        None | Some(b'Z') => false,
        Some(_) => unsafe { libc::kill(pid, 0) == 0 },
    }
}

fn direct_children(pid: libc::pid_t) -> Vec<libc::pid_t> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .ok()
        .into_iter()
        .flat_map(|children| {
            children
                .split_whitespace()
                .filter_map(|pid| pid.parse().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Ensures a failed assertion or timeout cannot leave either pty-tee or its
/// separate inner session alive. Before PID discovery, the inner shell is
/// found through Linux's direct-children procfs entry.
struct PtyTeeGuard {
    child: Child,
    inner_pgid: Option<libc::pid_t>,
}

impl Drop for PtyTeeGuard {
    fn drop(&mut self) {
        let outer_pid = self.child.id() as libc::pid_t;
        let groups = self
            .inner_pgid
            .into_iter()
            .chain(direct_children(outer_pid));
        for pgid in groups {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Closing pty-tee's outer input must boundedly kill a real inner session and
/// all of its SIGHUP/SIGTERM-ignoring members. Every potentially blocking
/// observation is non-blocking and guarded by a wall-clock deadline.
#[test]
fn pty_tee_escalates_to_sigkill_for_sighup_ignoring_descendant() {
    const POLL_STEP: Duration = Duration::from_millis(20);
    const PID_TIMEOUT: Duration = Duration::from_secs(3);
    const EXIT_TIMEOUT: Duration = Duration::from_secs(3);
    const GONE_TIMEOUT: Duration = Duration::from_secs(3);

    let tmp = tempfile::tempdir().expect("tempdir");
    let socket = tmp.path().join("flowmux.sock");
    let _rx = spawn_fake_daemon(socket.clone());

    // The shell is the inner session/process-group leader. Ignored signal
    // dispositions survive exec, so both shell and sleep require SIGKILL.
    let script = r#"
trap '' HUP TERM
sleep 300 &
DESC=$!
printf 'PIDS:%d:%d\n' "$$" "$DESC"
wait "$DESC"
"#;

    let mut command = Command::new(flowmuxctl_path());
    command
        .arg("pty-tee")
        .arg("--pane")
        .arg("11111111-1111-1111-1111-111111111111")
        .arg("--surface")
        .arg("22222222-2222-2222-2222-222222222222")
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(script)
        .env("FLOWMUX_SOCKET_PATH", &socket)
        .env("FLOWMUX_LOG", "off")
        .env("FLOWMUX_PTY_TEE_SIGHUP_GRACE_S", "0")
        .env("FLOWMUX_PTY_TEE_SIGTERM_GRACE_S", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let child = command.spawn().expect("spawn flowmuxctl pty-tee");
    let mut guard = PtyTeeGuard {
        child,
        inner_pgid: None,
    };
    let stdin = guard.child.stdin.take().expect("pty-tee stdin");
    let mut stdout = guard.child.stdout.take().expect("pty-tee stdout");
    set_nonblocking(&stdout);

    let mut captured = Vec::new();
    let discovered = with_deadline(Instant::now() + PID_TIMEOUT, POLL_STEP, || {
        let mut chunk = [0u8; 256];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => {
                    captured.extend_from_slice(&chunk[..n]);
                    assert!(captured.len() <= 4096, "unexpectedly noisy test child");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("read pty-tee stdout: {error}"),
            }
        }

        String::from_utf8_lossy(&captured).lines().find_map(|line| {
            let marker = line.find("PIDS:")?;
            let mut pids = line[marker + "PIDS:".len()..].trim().split(':');
            Some((pids.next()?.parse().ok()?, pids.next()?.parse().ok()?))
        })
    });
    let (shell_pid, descendant_pid) = discovered.unwrap_or_else(|| {
        panic!(
            "pty-tee did not report inner shell and descendant PIDs within 3s; captured={:?}",
            String::from_utf8_lossy(&captured)
        )
    });
    guard.inner_pgid = Some(shell_pid);

    assert!(process_is_running(shell_pid), "inner shell was not running");
    assert!(
        process_is_running(descendant_pid),
        "inner descendant was not running"
    );

    let close_started = Instant::now();
    drop(stdin);
    let status = with_deadline(Instant::now() + EXIT_TIMEOUT, POLL_STEP, || {
        guard.child.try_wait().expect("try_wait pty-tee")
    })
    .expect("pty-tee did not exit within 3s after outer EOF");

    assert!(
        close_started.elapsed() < EXIT_TIMEOUT,
        "pty-tee close exceeded its deadline"
    );
    assert!(
        !status.success(),
        "SIGKILL-escalated inner shell unexpectedly exited successfully: {status:?}"
    );

    let both_gone = with_deadline(Instant::now() + GONE_TIMEOUT, POLL_STEP, || {
        (!process_is_running(shell_pid) && !process_is_running(descendant_pid)).then_some(())
    })
    .is_some();
    assert!(
        both_gone,
        "inner process group still has running members after bounded escalation: shell={shell_pid}, descendant={descendant_pid}"
    );

    // The group is confirmed gone; do not signal a potentially reused pgid
    // from the guard's normal-success cleanup.
    guard.inner_pgid = None;
}
