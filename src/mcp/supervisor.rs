//! Crash-only supervisor (v3): a panic anywhere in the daemon is a
//! blip, not a death.
//!
//! `donsetch mcp --supervised` spawns the real daemon as a child
//! and proxies stdio. Release builds run `panic = "abort"` : a
//! hostile page that trips an unguarded path would otherwise take
//! the whole MCP session down. Under the supervisor the child
//! restarts (500ms backoff, honest give-up after 5 rapid
//! crashes), reloads its persistent state from disk, and keeps
//! serving.
//!
//! Structure: our stdin is drained by a reader thread into a
//! channel; the main loop multiplexes (new input | child death)
//! with a poll timeout, so an idle crash is caught within 500ms
//! and any bytes read-but-not-yet-forwarded when a child died are
//! held as `pending` and written to the NEXT child : a request is
//! never silently dropped. The MCP surface is stateless here (the
//! daemon answers requests without gating on `initialize`), so a
//! restarted child resumes the session as-is.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const MAX_RAPID_RESTARTS: u32 = 5;
const BACKOFF_MS: u64 = 500;
const POLL: Duration = Duration::from_millis(500);

enum In {
    Data(Vec<u8>),
    Eof,
}

pub fn run() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut restarts: u32 = 0;
    let mut pending: Vec<u8> = Vec::new();

    // Drain OUR stdin from a thread so the main loop can also
    // watch for child death while the client is idle.
    let (tx, rx) = mpsc::channel::<In>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 16384];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(In::Eof);
                    return;
                }
                Ok(n) => {
                    if tx.send(In::Data(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });

    let mut child: Option<(Child, std::process::ChildStdin)> = None;
    loop {
        // (Re)spawn if needed.
        if child.is_none() {
            if !pending.is_empty() {
                eprintln!(
                    "[supervisor] replaying {} held bytes to the new daemon",
                    pending.len()
                );
            }
            let mut c = Command::new(&exe)
                .arg("mcp")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;
            let mut stdin = c.stdin.take().expect("child stdin");
            let mut stdout = c.stdout.take().expect("child stdout");
            std::thread::spawn(move || {
                let mut out = std::io::stdout();
                let mut buf = [0u8; 16384];
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if out.write_all(&buf[..n]).is_err() {
                                break; // our client is gone
                            }
                            let _ = out.flush();
                        }
                    }
                }
            });
            // Held bytes first : they predate this child.
            // (Write failure: this child already died; keep pending.)
            if !pending.is_empty() && stdin.write_all(&pending).is_ok() {
                let _ = stdin.flush();
                pending.clear();
            }
            child = Some((c, stdin));
        }

        let (c, stdin) = child.as_mut().expect("child");
        // Multiplex: new input vs idle child death.
        match rx.recv_timeout(POLL) {
            Ok(In::Data(bytes)) => {
                if stdin.write_all(&bytes).is_ok() {
                    let _ = stdin.flush();
                } else {
                    // Child died under this write : hold the bytes
                    // for its replacement, never drop them.
                    pending = bytes;
                    eprintln!("[supervisor] daemon died mid-write : holding request for restart");
                    restart_child(c, &mut restarts);
                    child = None;
                }
            }
            Ok(In::Eof) => {
                // Our client closed stdin: signal EOF, let the
                // daemon finish its in-flight work, clean exit.
                drop(child.take());
                return Ok(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Idle: is the child still alive?
                if let Ok(Some(_status)) = c.try_wait() {
                    eprintln!("[supervisor] daemon died while idle : restarting");
                    restart_child(c, &mut restarts);
                    child = None;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                drop(child.take());
                return Ok(());
            }
        }
    }
}

fn restart_child(c: &mut Child, restarts: &mut u32) {
    let _ = c.kill();
    let _ = c.wait();
    *restarts += 1;
    if *restarts >= MAX_RAPID_RESTARTS {
        eprintln!(
            "[supervisor] {MAX_RAPID_RESTARTS} rapid crashes : giving up (the daemon needs a look)"
        );
        std::process::exit(1);
    }
    std::thread::sleep(Duration::from_millis(BACKOFF_MS));
}
