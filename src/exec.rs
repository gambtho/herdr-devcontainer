#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Shell,
    Command(String),
}

pub fn exec_argv(
    container_id: &str,
    remote_user: Option<&str>,
    workdir: &str,
    payload: &Payload,
) -> Vec<String> {
    let mut argv: Vec<String> = ["docker", "exec", "-i", "-t"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(user) = remote_user.filter(|u| !u.is_empty()) {
        argv.push("-u".to_string());
        argv.push(user.to_string());
    }
    argv.push("-w".to_string());
    argv.push(workdir.to_string());
    argv.push(container_id.to_string());
    match payload {
        Payload::Shell => {
            argv.push("sh".to_string());
            argv.push("-l".to_string());
        }
        Payload::Command(cmd) => {
            argv.push("sh".to_string());
            argv.push("-lc".to_string());
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

    #[test]
    fn shell_variant_builds_the_full_argv() {
        let argv = exec_argv(
            "c0ffee",
            Some("vscode"),
            "/workspaces/p/sub",
            &Payload::Shell,
        );
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
                "sh",
                "-l"
            ]
        );
    }

    #[test]
    fn command_variant_wraps_in_sh_lc() {
        let argv = exec_argv("c0ffee", None, "/w", &Payload::Command("claude".into()));
        assert_eq!(
            argv[argv.len() - 3..],
            ["sh".to_string(), "-lc".into(), "claude".into()]
        );
    }

    #[test]
    fn empty_remote_user_is_omitted() {
        let argv = exec_argv("c0ffee", Some(""), "/w", &Payload::Shell);
        assert!(!argv.contains(&"-u".to_string()));
    }

    #[test]
    fn hostile_workdir_stays_one_argv_element() {
        let hostile = "/w/di r/'quoted'";
        let argv = exec_argv("c0ffee", None, hostile, &Payload::Shell);
        assert!(argv.contains(&hostile.to_string()));
    }
}
