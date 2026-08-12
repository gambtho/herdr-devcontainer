use std::time::Duration;

use crate::run::{run, StderrMode};
use crate::util::tail;

/// The shell to reach for when the container cannot name one. Present in
/// essentially every image that is not distroless or `FROM scratch`; where it
/// is absent, `docker exec` fails with its own error naming the missing path.
pub const FALLBACK_SHELL: &str = "sh";

/// Read the remote user's login shell out of the container's passwd database.
///
/// `getent` is absent from some minimal images, so fall back to `/etc/passwd`
/// directly. The container side is a fixed literal — no repository-controlled
/// value is interpolated into it — and `id -u` resolves inside the container so
/// the answer belongs to the user the real exec will run as.
pub fn probe_argv(container_id: &str, remote_user: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = ["docker", "exec"].iter().map(|s| s.to_string()).collect();
    if let Some(user) = remote_user.filter(|u| !u.is_empty()) {
        argv.push("-u".to_string());
        argv.push(user.to_string());
    }
    argv.push(container_id.to_string());
    argv.push("sh".to_string());
    argv.push("-c".to_string());
    argv.push(
        r#"getent passwd "$(id -u)" 2>/dev/null || grep -E "^[^:]*:[^:]*:$(id -u):" /etc/passwd"#
            .to_string(),
    );
    argv
}

/// Field 7 of a passwd entry, when it names a shell we can actually exec.
///
/// A `nologin`/`false` shell is a refusal, not a shell: honoring it would turn
/// a working pane into an immediate exit. Anything non-absolute is treated as
/// malformed rather than resolved through the container's PATH.
pub fn parse_login_shell(stdout: &str) -> Option<String> {
    let line = stdout.lines().find(|l| !l.trim().is_empty())?;
    let shell = line.split(':').nth(6)?.trim();
    if !shell.starts_with('/') {
        return None;
    }
    let base = shell.rsplit('/').next().unwrap_or_default();
    if base == "nologin" || base == "false" {
        return None;
    }
    Some(shell.to_string())
}

/// The container user's login shell, or the reason we could not learn it.
///
/// Reporting a reason rather than quietly substituting `sh` keeps "the probe
/// failed" distinguishable from "the answer is `sh`", and keeps the captured
/// stderr from being thrown away. That distinction matters here: falling back
/// to `sh` reinstates exactly the bug this probe exists to prevent, so it
/// should never happen invisibly or unexplained.
pub fn probe(container_id: &str, remote_user: Option<&str>) -> Result<String, String> {
    let res = run(
        &probe_argv(container_id, remote_user),
        Duration::from_secs(5),
        StderrMode::Capture,
    )
    .map_err(|e| format!("could not run docker: {e}"))?;
    if res.timed_out {
        return Err("the probe timed out after 5s".to_string());
    }
    if res.exit_code != Some(0) {
        let detail = tail(res.stderr.trim(), 200);
        return Err(match (res.exit_code, detail.is_empty()) {
            (Some(code), false) => format!("docker exec exited {code}: {detail}"),
            (Some(code), true) => format!("docker exec exited {code}"),
            (None, _) => "docker exec was killed by a signal".to_string(),
        });
    }
    parse_login_shell(&res.stdout).ok_or_else(|| {
        format!(
            "no usable shell in the passwd entry: {}",
            tail(res.stdout.trim(), 200)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_runs_as_the_remote_user_when_there_is_one() {
        let argv = probe_argv("c0ffee", Some("developer"));
        assert_eq!(argv[..4], ["docker", "exec", "-u", "developer"]);
        assert!(argv.contains(&"c0ffee".to_string()));
    }

    #[test]
    fn probe_omits_an_empty_remote_user() {
        let argv = probe_argv("c0ffee", Some(""));
        assert!(!argv.contains(&"-u".to_string()));
    }

    #[test]
    fn parses_the_shell_field_of_a_passwd_entry() {
        let line = "developer:x:1000:1000::/home/developer:/bin/zsh";
        assert_eq!(parse_login_shell(line).as_deref(), Some("/bin/zsh"));
    }

    #[test]
    fn ignores_leading_blank_lines() {
        let out = "\n\nroot:x:0:0:root:/root:/bin/bash\n";
        assert_eq!(parse_login_shell(out).as_deref(), Some("/bin/bash"));
    }

    #[test]
    fn a_nologin_shell_is_not_a_shell() {
        let line = "svc:x:999:999::/nonexistent:/usr/sbin/nologin";
        assert_eq!(parse_login_shell(line), None);
    }

    #[test]
    fn a_false_shell_is_not_a_shell() {
        let line = "svc:x:999:999::/nonexistent:/bin/false";
        assert_eq!(parse_login_shell(line), None);
    }

    #[test]
    fn an_empty_shell_field_falls_back() {
        let line = "developer:x:1000:1000::/home/developer:";
        assert_eq!(parse_login_shell(line), None);
    }

    #[test]
    fn a_relative_shell_is_refused_rather_than_path_resolved() {
        let line = "developer:x:1000:1000::/home/developer:zsh";
        assert_eq!(parse_login_shell(line), None);
    }

    #[test]
    fn a_truncated_entry_is_not_a_shell() {
        assert_eq!(parse_login_shell("developer:x:1000"), None);
        assert_eq!(parse_login_shell(""), None);
    }

    #[test]
    fn a_probe_against_a_container_that_cannot_answer_explains_itself() {
        // Whether docker is missing entirely or the id does not exist, the
        // answer is a stated reason, never a shell.
        let err = probe("no-such-container", None).unwrap_err();
        assert!(!err.is_empty(), "the failure must name a reason");
    }
}
