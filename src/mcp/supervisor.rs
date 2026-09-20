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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_RAPID_RESTARTS: u32 = 5;
const BACKOFF_MS: u64 = 500;
const POLL: Duration = Duration::from_millis(500);
/// A child that served this long before dying was not part of a
/// crash loop: the rapid-restart counter starts over. Without
/// this the counter only ever grew, and a long-lived session gave
/// up on its fifth crash in a month.
const RAPID_WINDOW: Duration = Duration::from_secs(60);
/// After our client closes stdin, how long the daemon gets to
/// answer its in-flight requests and shut down cleanly before
/// it is killed.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

enum In {
    Data(Vec<u8>),
    Eof,
}

pub fn run() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    run_with(
        move || {
            let mut c = Command::new(&exe);
            c.arg("mcp");
            c
        },
        std::io::stdin(),
        std::io::stdout(),
    )
}

/// The supervisor loop over an arbitrary child command, input and
/// output (the real thing uses `donsetch mcp` and our own stdio).
/// Returns once the client has closed `input` AND the daemon has
/// finished: every response it produces on the way out reaches
/// `output`.
fn run_with<R, W>(
    mut child_cmd: impl FnMut() -> Command,
    input: R,
    output: W,
) -> std::io::Result<()>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    // main() restores SIGPIPE's default disposition so piped CLI
    // output dies quietly : this process must not. The crash
    // contract below depends on a write to a dead child's stdin
    // coming back as an EPIPE error (hold the bytes, restart,
    // replay) rather than a signal that kills the supervisor; and
    // a broken output pipe just means the client left (handled at
    // the write). The child daemon is unaffected : it makes its
    // own choice in its own main().
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let mut restarts: u32 = 0;
    let mut pending: Vec<u8> = Vec::new();
    // Everything written to the CURRENT child since its spawn.
    // A write that lands in a dying child's pipe buffer is
    // reported as SUCCESS by the kernel and then discarded with
    // the child, so the mid-write EPIPE arm alone cannot know
    // what was lost: bytes written before the fatal write are
    // equally gone. The unacked history is the replay window.
    let mut written: Vec<u8> = Vec::new();
    let output = Arc::new(Mutex::new(output));

    // Drain OUR stdin from a thread so the main loop can also
    // watch for child death while the client is idle.
    let (tx, rx) = mpsc::channel::<In>();
    std::thread::spawn(move || {
        let mut input = input;
        let mut buf = [0u8; 16384];
        loop {
            match input.read(&mut buf) {
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

    let mut child: Option<(Child, std::process::ChildStdin, Instant)> = None;
    loop {
        // (Re)spawn if needed.
        if child.is_none() {
            if !pending.is_empty() {
                eprintln!(
                    "[supervisor] replaying {} held bytes to the new daemon",
                    pending.len()
                );
            }
            let mut c = child_cmd()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;
            let mut stdin = c.stdin.take().expect("child stdin");
            let mut stdout = c.stdout.take().expect("child stdout");
            let out = Arc::clone(&output);
            std::thread::spawn(move || {
                let mut buf = [0u8; 16384];
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut out = out
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                // The replayed bytes are THIS child's unacked history
                // too: if it also dies before consuming them (a crash
                // loop where each child buffers-then-dies), the idle
                // poll must replay them again, not find an empty
                // history and drop the request one restart deeper.
                // Seed the history with what we just wrote instead of
                // clearing it.
                written = std::mem::take(&mut pending);
            } else {
                written.clear();
            }
            child = Some((c, stdin, Instant::now()));
        }

        let (c, stdin, born) = child.as_mut().expect("child");
        // Multiplex: new input vs idle child death.
        match rx.recv_timeout(POLL) {
            Ok(In::Data(bytes)) => {
                if stdin.write_all(&bytes).is_ok() {
                    record_written(&mut written, &bytes);
                    let _ = stdin.flush();
                } else {
                    // Child died under this write : hold this span
                    // of bytes for its replacement, never drop them.
                    // Replay the whole unacked history, not just the
                    // failed tail: every byte that reached the dead
                    // child's pipe is uncertain, and duplicate
                    // delivery is cheaper than a lost request.
                    // Fold in anything already held: a replay that
                    // failed against the previous child left `pending`
                    // populated with `written` cleared, and replacing
                    // it here would silently drop the held request
                    // one restart deeper (the exact loss this replay
                    // window exists to prevent).
                    let mut held = std::mem::take(&mut pending);
                    held.extend_from_slice(&bytes);
                    pending = replay_window(std::mem::take(&mut written), &held);
                    eprintln!("[supervisor] daemon died mid-write : holding request for restart");
                    restart_child(c, &mut restarts, born.elapsed());
                    child = None;
                }
            }
            // Our client closed stdin (or its reader thread died):
            // pass the EOF on and let the daemon finish its
            // in-flight work. Its answers travel through the
            // forwarder thread, which only lives as long as this
            // process : returning before the daemon exits would
            // drop every response still on the way out (and cut
            // its shutdown, browser cleanup included, short).
            Ok(In::Eof) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some((c, stdin, _)) = child.take() {
                    drop(stdin);
                    drain(c);
                }
                return Ok(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Idle: is the child still alive?
                if let Ok(Some(_status)) = c.try_wait() {
                    // The buffered-history case: writes that the
                    // kernel accepted are gone with the child, and
                    // no EPIPE ever fired. Replay the unacked
                    // history so the replacement serves them.
                    if !written.is_empty() {
                        pending = replay_window(std::mem::take(&mut written), &[]);
                        eprintln!(
                            "[supervisor] daemon died while idle : replaying {} unacked bytes",
                            pending.len()
                        );
                    }
                    eprintln!("[supervisor] daemon died while idle : restarting");
                    restart_child(c, &mut restarts, born.elapsed());
                    child = None;
                }
            }
        }
    }
}

/// The unacked-byte replay window: on a child death, everything
/// the dead child may not have consumed is replayed. Duplicate
/// delivery beats lost requests; cap the window so a long-lived
/// daemon does not grow the history forever. 1 MiB of JSON-RPC is
/// a lot of requests, and the overflow drops the OLDEST entries
/// (drain keeps the tail).
const REPLAY_WINDOW: usize = 1 << 20;

/// Append to the unacked history, bounded to REPLAY_WINDOW so a
/// long-lived HEALTHY child (which consumes everything immediately)
/// cannot grow the accumulator without limit. The bound is the same
/// one the replay applies, so "bounded to 1 MiB" holds for the live
/// history and not only for the bytes produced at death. Keeps the
/// tail: the newest requests are the ones a replacement still needs.
fn record_written(history: &mut Vec<u8>, bytes: &[u8]) {
    history.extend_from_slice(bytes);
    if history.len() > REPLAY_WINDOW {
        history.drain(..history.len() - REPLAY_WINDOW);
    }
}

fn replay_window(mut history: Vec<u8>, extra: &[u8]) -> Vec<u8> {
    record_written(&mut history, extra);
    history
}

/// Wait for a child that has seen EOF to exit on its own, killing
/// it only if it overstays `DRAIN_TIMEOUT`.
fn drain(mut c: Child) {
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    loop {
        match c.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if Instant::now() >= deadline => {
                eprintln!("[supervisor] daemon did not exit after stdin closed : killing it");
                let _ = c.kill();
                let _ = c.wait();
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// The restart count after a child that lived `lived` died: a
/// crash loop counts up; a child that served a full
/// `RAPID_WINDOW` first resets the count to one.
fn next_restart_count(restarts: u32, lived: Duration) -> u32 {
    if lived >= RAPID_WINDOW {
        1
    } else {
        restarts + 1
    }
}

fn restart_child(c: &mut Child, restarts: &mut u32, lived: Duration) {
    let _ = c.kill();
    let _ = c.wait();
    *restarts = next_restart_count(*restarts, lived);
    if *restarts >= MAX_RAPID_RESTARTS {
        eprintln!(
            "[supervisor] {MAX_RAPID_RESTARTS} rapid crashes : giving up (the daemon needs a look)"
        );
        std::process::exit(1);
    }
    std::thread::sleep(Duration::from_millis(BACKOFF_MS));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared sink the test can inspect after `run_with` returns.
    /// (Unix-only with its test: the child is a `sh` one-liner.)
    #[cfg(unix)]
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    #[cfg(unix)]
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // A client that writes its request and closes stdin at once
    // (a one-shot script, `printf ... | donsetch mcp --supervised`)
    // used to get nothing back: the supervisor returned on EOF
    // and the process exit took the stdout forwarder with it
    // before the daemon had answered. Reproduced with the real
    // binary: `donsetch mcp` answered, `--supervised` did not.
    #[cfg(unix)]
    #[test]
    fn responses_after_client_eof_still_reach_the_output() {
        let sink = Sink::default();
        let input = std::io::Cursor::new(b"hello\n".to_vec());
        // A child that answers late: it echoes stdin only after
        // the client has long since closed it.
        run_with(
            || {
                let mut c = Command::new("sh");
                c.args(["-c", "sleep 0.5; cat"]);
                c
            },
            input,
            sink.clone(),
        )
        .unwrap();
        let got = sink.0.lock().unwrap().clone();
        assert_eq!(String::from_utf8_lossy(&got), "hello\n");
    }

    /// A client that waits for the child to ANNOUNCE that its stdin is
    /// closed before writing, then waits to see its request come back
    /// through the replacement before closing. Both waits are on
    /// observable effects, never on a sleep: the announcement is written
    /// after the close, so the request is guaranteed to land on a closed
    /// pipe and come back EPIPE however long the runner took to start
    /// `sh`. The 300ms timer this replaces raced the shell on
    /// macos-x86_64, first as a failed spawn assert, then as a 30s
    /// nextest timeout.
    #[cfg(unix)]
    struct WaitsForReady {
        sink: Arc<Mutex<Vec<u8>>>,
        payload: &'static [u8],
        phase: u8,
    }

    #[cfg(unix)]
    impl Read for WaitsForReady {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.phase {
                0 => {
                    wait_for_marker(&self.sink, b"READY\n");
                    self.phase = 1;
                    buf[..self.payload.len()].copy_from_slice(self.payload);
                    Ok(self.payload.len())
                }
                1 => {
                    // Only the replacement echoes, so the payload
                    // showing up in the sink IS the replay: waiting for
                    // it keeps the final assert from racing the
                    // forwarder thread that writes it out.
                    wait_for_marker(&self.sink, self.payload);
                    self.phase = 2;
                    Ok(0)
                }
                _ => Ok(0),
            }
        }
    }

    /// Block until the sink shows `needle`, bounded so a regression fails
    /// the asserts below instead of hanging into nextest's slow-timeout
    /// (the shape this test took on macos-x86_64).
    #[cfg(unix)]
    fn wait_for_marker(sink: &Arc<Mutex<Vec<u8>>>, needle: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if sink
                .lock()
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle)
            {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // A client whose request lands in the pipe buffer of a child
    // that dies before consuming it. The write succeeds while the
    // child is still alive, so the mid-write EPIPE arm never
    // fires: the death surfaces on the next idle poll, and only
    // the unacked-history replay saves the request. Child 1 must
    // outlive the write, then die; the client holds its EOF long
    // enough for the idle poll to see the death first.
    /// Serves the payload once, then holds the connection open for
    /// a caller-set delay before EOF: the death surfaces on the
    /// idle poll, and the EOF must outlast every poll + restart
    /// backoff the test needs to survive.
    #[cfg(unix)]
    struct WriteThenEofAfter(&'static [u8], bool, u64);

    #[cfg(unix)]
    impl Read for WriteThenEofAfter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.1 {
                std::thread::sleep(Duration::from_millis(self.2));
                return Ok(0);
            }
            self.1 = true;
            buf[..self.0.len()].copy_from_slice(self.0);
            Ok(self.0.len())
        }
    }

    /// The macOS CI signature: the request was buffered into a
    /// dying child, the write reported success, and the request
    /// never reached the replacement.
    #[cfg(unix)]
    #[test]
    fn request_buffered_in_a_dying_child_replays_to_the_replacement() {
        let sink = Sink::default();
        let spawns = Arc::new(Mutex::new(0u32));
        let spawns2 = Arc::clone(&spawns);
        run_with(
            move || {
                let mut n = spawns2.lock().unwrap();
                *n += 1;
                let mut c = Command::new("sh");
                // First child accepts the write, dies 300ms later
                // without consuming it; its replacement serves.
                c.args(["-c", if *n == 1 { "sleep 0.3; exit 0" } else { "cat" }]);
                c
            },
            // Hold EOF past one idle poll (500ms) + the restart
            // backoff (500ms) + a slow CI spawn: 900ms lost the race
            // on a loaded macOS runner (EOF drained the corpse before
            // the replacement existed). Same reasoning as the
            // two-death test below.
            WriteThenEofAfter(b"ping\n", false, 4000),
            sink.clone(),
        )
        .unwrap();
        assert!(
            *spawns.lock().unwrap() >= 2,
            "the dead child must have been replaced"
        );
        let got = sink.0.lock().unwrap().clone();
        assert_eq!(
            String::from_utf8_lossy(&got),
            "ping\n",
            "the buffered request must replay to the restarted child"
        );
    }

    // The crash-loop case: the replacement ALSO buffers-then-dies
    // before consuming the replayed request. On respawn the replayed
    // `pending` is this child's unacked history too, so it must be
    // seeded into `written` rather than cleared. Without the seed the
    // second idle poll finds an empty history and drops the request
    // one restart deeper : the replay survives exactly one death.
    // Children 1 and 2 accept the write and die; child 3 serves.
    #[cfg(unix)]
    #[test]
    fn buffered_request_survives_two_consecutive_silent_deaths() {
        let sink = Sink::default();
        let spawns = Arc::new(Mutex::new(0u32));
        let spawns2 = Arc::clone(&spawns);
        run_with(
            move || {
                let mut n = spawns2.lock().unwrap();
                *n += 1;
                let mut c = Command::new("sh");
                // First TWO children buffer the request and die; the
                // third finally consumes and echoes it.
                c.args(["-c", if *n <= 2 { "sleep 0.3; exit 0" } else { "cat" }]);
                c
            },
            // Hold EOF well past two deaths + their restart backoffs
            // so both are seen by the idle poll before the client
            // closes and the survivor is drained.
            WriteThenEofAfter(b"ping\n", false, 4000),
            sink.clone(),
        )
        .unwrap();
        assert!(
            *spawns.lock().unwrap() >= 3,
            "both dying children must have been replaced"
        );
        let got = sink.0.lock().unwrap().clone();
        assert_eq!(
            String::from_utf8_lossy(&got),
            "ping\n",
            "the buffered request must survive a second silent death"
        );
    }

    // Bounded history: a healthy child consumes everything, but the
    // accumulator must still not grow without limit. record_written
    // keeps only the last REPLAY_WINDOW bytes (the tail a replacement
    // would need), so "bounded to 1 MiB" is true of the live history,
    // not only of the replay output.
    #[test]
    fn record_written_bounds_the_history_to_the_replay_window() {
        let mut history = Vec::new();
        // Write well past the window in small chunks.
        for i in 0..(REPLAY_WINDOW / 1000 + 50) {
            let chunk = format!("{i:04}------------------------------").repeat(30);
            record_written(&mut history, chunk.as_bytes());
        }
        assert!(
            history.len() <= REPLAY_WINDOW,
            "history {} exceeded the {REPLAY_WINDOW}-byte window",
            history.len()
        );
        // The window keeps the TAIL: the very last bytes written are
        // still present (a replacement needs the newest requests).
        record_written(&mut history, b"LAST-MARKER\n");
        assert!(history.ends_with(b"LAST-MARKER\n"));
        assert!(history.len() <= REPLAY_WINDOW);
    }

    // main() restores SIGPIPE's default disposition for the CLI
    // (quiet `donsetch --help | head` exits). The supervisor's
    // whole crash contract, though, is built on the write to a
    // dead child's stdin coming back as an EPIPE *error* (hold
    // the bytes, restart, replay): under SIG_DFL that write is a
    // SIGPIPE that kills the supervisor itself before write_all
    // returns. run_with must pin SIG_IGN for its own process no
    // matter what main() set. Without the fix this test does not
    // fail an assert : the test process dies by signal 13.
    #[cfg(unix)]
    #[test]
    fn crash_mid_write_restarts_even_with_cli_sigpipe_disposition() {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
        let (spawns, got) = sigpipe_restart_case("exec 0<&-; echo READY");
        assert!(spawns >= 2, "the dead child must have been replaced");
        assert_eq!(
            got, "READY\nping\n",
            "the held request must replay to the restarted child"
        );
    }

    // The macos-x86_64 signature, reproduced on every platform: a shell
    // that is slow to reach its own `exec 0<&-`. A client that wrote on a
    // 300ms timer landed in a LIVE pipe, got no EPIPE, and never reached
    // the restart path: first as a failed spawn assert (#236), then as a
    // 30s nextest timeout (#239). The child's announcement makes the
    // ordering causal instead.
    #[cfg(unix)]
    #[test]
    fn mid_write_restart_does_not_race_a_slow_to_start_child() {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
        let (spawns, got) = sigpipe_restart_case("sleep 1; exec 0<&-; echo READY");
        assert!(spawns >= 2, "the dead child must have been replaced");
        assert_eq!(
            got, "READY\nping\n",
            "the held request must replay to the restarted child"
        );
    }

    /// Drive the mid-write crash case. `first_child` must close its own
    /// stdin and announce it on stdout; its replacement is `cat`.
    /// Returns the spawn count and everything the client received.
    #[cfg(unix)]
    fn sigpipe_restart_case(first_child: &'static str) -> (u32, String) {
        let sink = Sink::default();
        let spawns = Arc::new(Mutex::new(0u32));
        let spawns2 = Arc::clone(&spawns);
        run_with(
            move || {
                let mut n = spawns2.lock().unwrap();
                *n += 1;
                let mut c = Command::new("sh");
                c.args(["-c", if *n == 1 { first_child } else { "cat" }]);
                c
            },
            WaitsForReady {
                sink: Arc::clone(&sink.0),
                payload: b"ping\n",
                phase: 0,
            },
            sink.clone(),
        )
        .unwrap();
        let spawns = *spawns.lock().unwrap();
        let got = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
        (spawns, got)
    }

    #[test]
    fn restart_counter_resets_after_a_long_lived_child() {
        assert_eq!(next_restart_count(0, Duration::from_millis(10)), 1);
        assert_eq!(next_restart_count(3, Duration::from_secs(5)), 4);
        // Five crashes spread over a long session are not a loop.
        assert_eq!(next_restart_count(4, RAPID_WINDOW), 1);
        assert_eq!(next_restart_count(4, Duration::from_secs(3600)), 1);
    }
}
