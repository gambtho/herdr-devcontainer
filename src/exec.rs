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

/// Shells verified to accept combined POSIX-style startup flags (`-li`,
/// `-lic`). Not every shell does: `tcsh -lic` fails outright with "Unknown
/// option", and a shell that rejects its own flags produces a pane that never
/// opens — worse than the plain `sh` this replaced. Anything not on this list
/// is driven with bare `-c`, which every shell worth naming accepts, and its
/// own rc handling is left to it.
const LOGIN_INTERACTIVE_SHELLS: &[&str] = &[
    "ash", "busybox", "dash", "fish", "ksh", "ksh93", "mksh", "pdksh", "sh", "yash", "zsh",
];

/// The startup flags that make `shell` read the rc file its container's setup
/// scripts write to, or `None` for a shell whose flags we cannot assume.
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
/// The listed shells keep `-l`, and the `/etc/profile` values that come with
/// it, because zsh reads `~/.zshrc` for any interactive shell and the ash/dash
/// family has no rc file to miss. Two cases fall between the cracks and are
/// accepted: a `/bin/sh` that is really bash runs in POSIX mode and reads
/// neither, and a shell whose basename is not `bash` but whose behavior is
/// bash's gets login treatment.
fn startup_flags(shell: &str, interactive_only: bool) -> Option<&'static str> {
    let base = shell.rsplit('/').next().unwrap_or_default();
    match base {
        "bash" if interactive_only => Some("-ic"),
        "bash" => Some("-i"),
        b if LOGIN_INTERACTIVE_SHELLS.contains(&b) && interactive_only => Some("-lic"),
        b if LOGIN_INTERACTIVE_SHELLS.contains(&b) => Some("-li"),
        _ => None,
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
        // The pane's tty already makes a flagless shell interactive; `-i` is
        // passed anyway, where it is safe to, so the shape does not depend on
        // how the pane was wired.
        Payload::Shell => {
            if let Some(flags) = startup_flags(spec.shell, false) {
                argv.push(flags.to_string());
            }
        }
        Payload::Command(cmd) => {
            argv.push(startup_flags(spec.shell, true).unwrap_or("-c").to_string());
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
    fn a_shell_merely_containing_bash_is_not_bash() {
        // Only the basename decides; /usr/local/bin/bashly is not bash — and it
        // is not a known shell either, so it gets the portable form.
        let argv = exec_argv(&spec("/w", "/usr/local/bin/bashly", &Payload::Shell));
        assert_eq!(argv.last().unwrap(), "/usr/local/bin/bashly");
    }

    #[test]
    fn an_unknown_shell_gets_the_portable_form_rather_than_broken_flags() {
        // `tcsh -lic` fails with "Unknown option: `-lic'" — a pane that never
        // opens, which is worse than the plain `sh` this replaced. Verified in
        // an alpine container with tcsh installed.
        let argv = exec_argv(&spec("/w", "/bin/tcsh", &Payload::Command("claude".into())));
        assert_eq!(
            argv[argv.len() - 3..],
            ["/bin/tcsh".to_string(), "-c".into(), "claude".into()]
        );
        // Nothing but the shell itself: a tty makes it interactive already.
        let argv = exec_argv(&spec("/w", "/bin/tcsh", &Payload::Shell));
        assert_eq!(argv.last().unwrap(), "/bin/tcsh");
    }

    #[test]
    fn the_known_shells_all_take_login_interactive_flags() {
        // Each verified to accept the combined flags before being listed.
        for shell in [
            "/bin/zsh",
            "/bin/dash",
            "/bin/ash",
            "/bin/mksh",
            "/usr/bin/fish",
        ] {
            let argv = exec_argv(&spec("/w", shell, &Payload::Command("claude".into())));
            assert_eq!(argv[argv.len() - 2], "-lic", "{shell} should get -lic");
        }
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
