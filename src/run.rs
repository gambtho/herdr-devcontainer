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

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();
    Ok(RunResult {
        exit_code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        timed_out,
    })
}

fn kill_group(child: &Child) {
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
}

fn capture_thread<R: Read + Send + 'static>(stream: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(mut stream) = stream {
            let mut buf = [0u8; 8192];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out.len() < CAPTURE_LIMIT {
                            let take = n.min(CAPTURE_LIMIT - out.len());
                            out.extend_from_slice(&buf[..take]);
                        }
                    }
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
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
    }

    #[test]
    fn missing_binary_is_an_io_error() {
        let argv = vec!["/nonexistent/definitely-not-a-binary".to_string()];
        assert!(run(&argv, Duration::from_secs(1), StderrMode::Capture).is_err());
    }
}
