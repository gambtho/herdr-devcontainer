use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const CAPTURE_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StderrMode {
    Capture,
    Inherit,
}

#[derive(Debug)]
pub struct RunResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    /// Reading stdout ended on an error rather than EOF, so `stdout` may be a
    /// prefix of what the process wrote. Callers that parse it for an answer
    /// must not treat a short result as a complete one.
    pub stdout_incomplete: bool,
    pub stderr: String,
    pub timed_out: bool,
}

pub fn run(
    argv: &[String],
    timeout: Duration,
    stderr_mode: StderrMode,
) -> std::io::Result<RunResult> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0);
    match stderr_mode {
        StderrMode::Capture => cmd.stderr(Stdio::piped()),
        StderrMode::Inherit => cmd.stderr(Stdio::inherit()),
    };
    let mut child = cmd.spawn()?;
    let stdout_thread = capture_thread(child.stdout.take());
    let stderr_thread = match stderr_mode {
        StderrMode::Capture => Some(capture_thread(child.stderr.take())),
        StderrMode::Inherit => None,
    };

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_group(&child);
            break child.wait().ok();
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let stdout = stdout_thread.join().unwrap_or_else(|_| Capture {
        text: String::new(),
        // A panicked capture thread is not an empty stream.
        incomplete: true,
    });
    let stderr = stderr_thread
        .map(|t| t.join().map(|c| c.text).unwrap_or_else(|_| String::new()))
        .unwrap_or_default();
    Ok(RunResult {
        exit_code: status.and_then(|s| s.code()),
        stdout: stdout.text,
        stdout_incomplete: stdout.incomplete,
        stderr,
        timed_out,
    })
}

fn kill_group(child: &Child) {
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
}

/// A captured stream, plus whether reading it ended early.
///
/// `incomplete` covers both ways bytes go missing: a read error that used to be
/// indistinguishable from EOF, and output past `CAPTURE_LIMIT`. A caller that
/// parses the result would otherwise treat a truncated stream as the whole
/// answer — and truncation on a line boundary parses perfectly.
pub struct Capture {
    pub text: String,
    pub incomplete: bool,
}

fn capture_thread<R: Read + Send + 'static>(stream: Option<R>) -> std::thread::JoinHandle<Capture> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut incomplete = false;
        if let Some(mut stream) = stream {
            let mut buf = [0u8; 8192];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    // A signal arriving mid-read says nothing about the stream.
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        incomplete = true;
                        break;
                    }
                    Ok(n) => {
                        // Keep reading past the cap rather than breaking: the
                        // child blocks on a full pipe if nobody drains it. Only
                        // the appending stops.
                        let room = CAPTURE_LIMIT.saturating_sub(out.len());
                        let take = n.min(room);
                        out.extend_from_slice(&buf[..take]);
                        if take < n {
                            incomplete = true;
                        }
                    }
                }
            }
        }
        Capture {
            text: String::from_utf8_lossy(&out).into_owned(),
            incomplete,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    // A read error was indistinguishable from EOF, so a truncated stream became
    // a short-but-complete-looking capture. If truncation lands on a line
    // boundary, `docker ps` output parses cleanly and a caller reads the short
    // list as authoritative — "no running dev container" for a container that
    // is running. The capture must say it was cut off.
    #[test]
    fn a_read_error_marks_the_capture_incomplete() {
        struct FailsAfterFirstRead(bool);
        impl Read for FailsAfterFirstRead {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 {
                    return Err(std::io::Error::other("device went away"));
                }
                self.0 = true;
                buf[..4].copy_from_slice(b"abc\n");
                Ok(4)
            }
        }
        let cap = capture_thread(Some(FailsAfterFirstRead(false)))
            .join()
            .unwrap();
        assert_eq!(cap.text, "abc\n");
        assert!(cap.incomplete, "a read error is not an EOF");
    }

    // EINTR is not a failure — a signal arriving mid-read says nothing about
    // the stream. Treating it as one would make every capture a coin flip.
    #[test]
    fn an_interrupted_read_is_retried_not_reported() {
        struct InterruptsOnce(u8);
        impl Read for InterruptsOnce {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0 += 1;
                match self.0 {
                    1 => Err(std::io::Error::from(std::io::ErrorKind::Interrupted)),
                    2 => {
                        buf[..2].copy_from_slice(b"ok");
                        Ok(2)
                    }
                    _ => Ok(0),
                }
            }
        }
        let cap = capture_thread(Some(InterruptsOnce(0))).join().unwrap();
        assert_eq!(cap.text, "ok");
        assert!(!cap.incomplete);
    }

    #[test]
    fn captures_stdout_and_exit_code() {
        let res = run(
            &sh("echo hi; exit 3"),
            Duration::from_secs(5),
            StderrMode::Capture,
        )
        .unwrap();
        assert_eq!(res.stdout.trim(), "hi");
        assert_eq!(res.exit_code, Some(3));
        assert!(!res.timed_out);
    }

    #[test]
    fn captures_stderr_in_capture_mode() {
        let res = run(
            &sh("echo oops >&2"),
            Duration::from_secs(5),
            StderrMode::Capture,
        )
        .unwrap();
        assert_eq!(res.stderr.trim(), "oops");
    }

    #[test]
    fn timeout_kills_the_whole_process_group() {
        // The backgrounded sleep inherits the stdout pipe; without a group
        // kill, draining stdout would block until it exits on its own.
        let start = Instant::now();
        let res = run(
            &sh("sleep 30 & sleep 30"),
            Duration::from_millis(300),
            StderrMode::Capture,
        )
        .unwrap();
        assert!(res.timed_out);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stdout_capture_is_bounded() {
        let res = run(
            &sh("head -c 200000 /dev/zero | tr '\\0' 'a'"),
            Duration::from_secs(10),
            StderrMode::Capture,
        )
        .unwrap();
        assert_eq!(res.stdout.len(), CAPTURE_LIMIT);
        // Hitting the cap drops bytes just as surely as a read error does. A
        // `docker ps` listing cut at the cap still parses, so without this the
        // truncated list would be indexed as every container that exists.
        assert!(res.stdout_incomplete, "a capped capture is not a whole one");
    }

    #[test]
    fn missing_binary_is_an_io_error() {
        let argv = vec!["/nonexistent/definitely-not-a-binary".to_string()];
        assert!(run(&argv, Duration::from_secs(1), StderrMode::Capture).is_err());
    }
}
