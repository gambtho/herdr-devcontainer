#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Shell,
    Command(String),
}

/// Everything the container-side invocation needs. A struct rather than a
/// parameter list because `workdir`, `shell` and the container id are all bare
/// strings — an argument-order slip between them would produce a plausible
/// argv that fails only at runtime.
#[derive(Debug, Clone)]
pub struct ExecSpec<'a> {
    pub container_id: &'a str,
    pub remote_user: Option<&'a str>,
    pub workdir: &'a str,
    pub shell: &'a str,
    pub env: &'a [String],
    pub payload: &'a Payload,
}

/// The startup flags that make `shell` read the rc file its container's setup
/// scripts write to.
///
/// Interactive is the point: zsh sources `~/.zshrc`, and bash `~/.bashrc`,
/// *only* when interactive, and that is where Dev Container setup scripts put
/// PATH entries and API endpoints. A login-only shell reads `~/.zprofile` and
/// stops, which is how an agent ends up bypassing a configured proxy.
///
/// bash is then the exception on the *login* half. Its manual is explicit: an
/// interactive login shell reads `/etc/profile` and the first of
/// `~/.bash_profile`, `~/.bash_login`, `~/.profile` — and reads `~/.bashrc`
/// only when interactive and **not** a login shell. Images that ship a
/// `~/.bashrc` and no profile file at all are common (`devcontainers/base` is
/// one), so adding `-l` for bash silently loses the very environment this is
/// here to collect.
///
/// Everything else keeps `-l`, and the `/etc/profile` values that come with it,
/// because zsh reads `~/.zshrc` for any interactive shell and the ash/dash
/// family has no rc file to miss. Two cases fall between the cracks and are
/// accepted: a `/bin/sh` that is really bash runs in POSIX mode and reads
/// neither, and a shell whose basename is not `bash` but whose behavior is
/// bash's gets login treatment.
fn startup_flags(shell: &str, interactive_only: bool) -> &'static str {
    let is_bash = shell.rsplit('/').next().unwrap_or_default() == "bash";
    match (is_bash, interactive_only) {
        (true, true) => "-ic",
        (true, false) => "-i",
        (false, true) => "-lic",
        (false, false) => "-li",
    }
}

pub fn exec_argv(spec: &ExecSpec) -> Vec<String> {
    let mut argv: Vec<String> = ["docker", "exec", "-i", "-t"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(user) = spec.remote_user.filter(|u| !u.is_empty()) {
        argv.push("-u".to_string());
        argv.push(user.to_string());
    }
    for assignment in spec.env {
        argv.push("-e".to_string());
        argv.push(assignment.clone());
    }
    argv.push("-w".to_string());
    argv.push(spec.workdir.to_string());
    argv.push(spec.container_id.to_string());
    argv.push(spec.shell.to_string());
    match spec.payload {
        // The pane's tty already makes a `-c`-less shell interactive; `-i` is
        // passed anyway so the shape does not depend on how the pane was wired.
        Payload::Shell => argv.push(startup_flags(spec.shell, false).to_string()),
        Payload::Command(cmd) => {
            argv.push(startup_flags(spec.shell, true).to_string());
            // Deliberately *not* prefixed with `exec`. The payload is an
            // arbitrary shell fragment, and `exec` only accepts a command:
            // `exec source env.sh && claude` dies with "exec: source: not
            // found". The shell stays the payload's parent — `-i` turns on job
            // control, which suppresses the usual exec optimization (verified
            // under a tty: zsh and dash both fork) — but job control is also
            // what hands the payload its own foreground process group, so the
            // pane's signals still reach it.
            argv.push(cmd.clone());
        }
    }
    argv
}

/// Replaces the current process. Returns only if exec failed.
pub fn exec_into(argv: &[String]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(&argv[0]).args(&argv[1..]).exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_ENV: &[String] = &[];

    fn spec<'a>(workdir: &'a str, shell: &'a str, payload: &'a Payload) -> ExecSpec<'a> {
        ExecSpec {
            container_id: "c0ffee",
            remote_user: None,
            workdir,
            shell,
            env: NO_ENV,
            payload,
        }
    }

    #[test]
    fn shell_variant_builds_the_full_argv() {
        let argv = exec_argv(&ExecSpec {
            remote_user: Some("vscode"),
            ..spec("/workspaces/p/sub", "/bin/zsh", &Payload::Shell)
        });
        assert_eq!(
            argv,
            vec![
                "docker",
                "exec",
                "-i",
                "-t",
                "-u",
                "vscode",
                "-w",
                "/workspaces/p/sub",
                "c0ffee",
                "/bin/zsh",
                "-li"
            ]
        );
    }

    #[test]
    fn command_variant_uses_an_interactive_login_shell() {
        let argv = exec_argv(&spec("/w", "/bin/zsh", &Payload::Command("claude".into())));
        assert_eq!(
            argv[argv.len() - 3..],
            ["/bin/zsh".to_string(), "-lic".into(), "claude".into()]
        );
    }

    #[test]
    fn bash_drops_the_login_flag_that_would_skip_bashrc() {
        // Verified against mcr.microsoft.com/devcontainers/base:alpine, whose
        // vscode user has a ~/.bashrc and no profile file: `bash -lic` reports
        // an empty marker there, `bash -ic` reports it.
        let argv = exec_argv(&spec("/w", "/bin/bash", &Payload::Command("claude".into())));
        assert_eq!(
            argv[argv.len() - 3..],
            ["/bin/bash".to_string(), "-ic".into(), "claude".into()]
        );
        let argv = exec_argv(&spec("/w", "/bin/bash", &Payload::Shell));
        assert_eq!(argv.last().unwrap(), "-i");
    }

    #[test]
    fn a_shell_merely_containing_bash_still_gets_login() {
        // Only the basename decides; /usr/local/bin/bashly is not bash.
        let argv = exec_argv(&spec("/w", "/usr/local/bin/bashly", &Payload::Shell));
        assert_eq!(argv.last().unwrap(), "-li");
    }

    #[test]
    fn a_compound_payload_is_passed_through_untouched() {
        // Wrapping this in `exec` would turn a working payload into
        // "exec: source: not found".
        let payload = Payload::Command("source ~/env.sh && claude".into());
        let argv = exec_argv(&spec("/w", "sh", &payload));
        assert_eq!(argv.last().unwrap(), "source ~/env.sh && claude");
    }

    #[test]
    fn the_command_payload_stays_a_single_argv_element() {
        let payload = Payload::Command("claude --model opus --flag 'a b'".into());
        let argv = exec_argv(&spec("/w", "sh", &payload));
        assert_eq!(argv.last().unwrap(), "claude --model opus --flag 'a b'");
    }

    #[test]
    fn env_assignments_become_dash_e_pairs() {
        let env = vec!["FOO=bar".to_string(), "BAZ=qu ux".to_string()];
        let argv = exec_argv(&ExecSpec {
            env: &env,
            ..spec("/w", "sh", &Payload::Shell)
        });
        let pairs: Vec<&String> = argv
            .iter()
            .skip_while(|a| *a != "-e")
            .take(4)
            .collect::<Vec<_>>();
        assert_eq!(pairs, vec!["-e", "FOO=bar", "-e", "BAZ=qu ux"]);
    }

    #[test]
    fn no_env_means_no_dash_e() {
        let argv = exec_argv(&spec("/w", "sh", &Payload::Shell));
        assert!(!argv.contains(&"-e".to_string()));
    }

    #[test]
    fn empty_remote_user_is_omitted() {
        let argv = exec_argv(&ExecSpec {
            remote_user: Some(""),
            ..spec("/w", "sh", &Payload::Shell)
        });
        assert!(!argv.contains(&"-u".to_string()));
    }

    #[test]
    fn hostile_workdir_stays_one_argv_element() {
        let hostile = "/w/di r/'quoted'";
        let argv = exec_argv(&spec(hostile, "sh", &Payload::Shell));
        assert!(argv.contains(&hostile.to_string()));
    }
}
