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
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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
    let deadline = Instant::now() + Duration::from_secs(2);
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
fn osc_99_promotes_to_attention_needed_when_body_says_waiting() {
    let envelopes = run_tee_with_osc(&[
        "99;urgency=critical;Claude is waiting for your input",
    ]);
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
        notify.contains("\"level\":\"attention_needed\"")
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
