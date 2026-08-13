#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not inside a git repository (cwd: {cwd})")]
    NotAGitRepo { cwd: String },
    #[error("the Dev Containers CLI (`devcontainer`) was not found on PATH")]
    DevcontainerCliMissing,
    #[error("docker daemon unreachable: {detail}")]
    DockerUnreachable { detail: String },
    #[error("could not read {path}: {detail}")]
    ConfigUnreadable { path: String, detail: String },
    #[error("config path for {repo_root} must be repo-relative, got {value}")]
    InvalidConfigPath { repo_root: String, value: String },
    #[error("no devcontainer config found under {repo_root} (looked for .devcontainer/devcontainer.json and .devcontainer.json)")]
    NoDevcontainerConfig { repo_root: String },
    #[error("dev container support is disabled for {repo_root} in config")]
    DisabledByConfig { repo_root: String },
    #[error("`devcontainer up` timed out after {secs}s\n--- last output ---\n{output_tail}")]
    UpTimeout { secs: u64, output_tail: String },
    #[error(
        "`devcontainer up` failed (exit code {exit_code:?})\n--- last output ---\n{output_tail}"
    )]
    UpFailed {
        exit_code: Option<i32>,
        output_tail: String,
    },
    #[error("could not parse `devcontainer up` output: {detail}\nlast line: {last_line}")]
    UpOutputUnparseable { detail: String, last_line: String },
    #[error("could not parse `docker ps` output line: {line}")]
    MalformedDockerOutput { line: String },
    #[error("multiple running dev containers for {repo_root}; refusing to choose: {ids:?}")]
    MultipleRunningContainers { repo_root: String, ids: Vec<String> },
    #[error("these containers did not stop: {ids:?}\n{detail}")]
    ContainersNotStopped { ids: Vec<String>, detail: String },
    #[error("docker command failed: {detail}")]
    DockerCommandFailed { detail: String },
    #[error("{0}")]
    Other(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn hint(&self) -> Option<&str> {
        match self {
            Error::DevcontainerCliMissing => {
                Some("install it: npm install -g @devcontainers/cli")
            }
            Error::DockerUnreachable { .. } => Some(
                "start the docker daemon (WSL2: ensure Docker Desktop or the docker service is running)",
            ),
            Error::NoDevcontainerConfig { .. } => Some(
                "add a devcontainer.json, or set enabled = \"true\" for this repo in ~/.config/herdr-devcontainer/config.toml",
            ),
            Error::InvalidConfigPath { .. } => Some(
                "`config` must be relative to the repo root; use a path like .devcontainer/alt/devcontainer.json",
            ),
            Error::ConfigUnreadable { .. } => {
                Some("fix the file's permissions, or remove it to fall back to defaults")
            }
            Error::MalformedDockerOutput { .. } => {
                Some("this is a docker output-format change; report it rather than assuming no container is running")
            }
            Error::MultipleRunningContainers { .. } => {
                Some("stop the extras with `docker stop <id>` and retry")
            }
            Error::ContainersNotStopped { .. } => {
                Some("the rest of the project stopped; stop these with `docker stop <id>`")
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_exist_for_actionable_errors() {
        assert!(Error::DevcontainerCliMissing
            .hint()
            .unwrap()
            .contains("npm"));
        assert!(Error::DockerUnreachable { detail: "x".into() }
            .hint()
            .unwrap()
            .contains("daemon"));
        assert!(Error::NoDevcontainerConfig {
            repo_root: "/r".into()
        }
        .hint()
        .unwrap()
        .contains("config.toml"));
        assert!(Error::MultipleRunningContainers {
            repo_root: "/r".into(),
            ids: vec![]
        }
        .hint()
        .unwrap()
        .contains("docker stop"));
        assert!(Error::InvalidConfigPath {
            repo_root: "/r".into(),
            value: "/etc/x".into()
        }
        .hint()
        .unwrap()
        .contains("relative"));
        assert!(Error::NotAGitRepo { cwd: "/tmp".into() }.hint().is_none());
    }

    #[test]
    fn display_includes_the_repo_path() {
        let msg = Error::DisabledByConfig {
            repo_root: "/home/x/repo".into(),
        }
        .to_string();
        assert!(msg.contains("/home/x/repo"));
    }

    // The whole point of capturing a tail is showing it. An exit code with no
    // diagnostics is the failure mode spec step 9 exists to prevent.
    #[test]
    fn up_failures_render_their_captured_tail() {
        let msg = Error::UpFailed {
            exit_code: Some(1),
            output_tail: "ERROR: build step failed".into(),
        }
        .to_string();
        assert!(msg.contains("exit code"));
        assert!(msg.contains("ERROR: build step failed"));

        let msg = Error::UpTimeout {
            secs: 300,
            output_tail: "still resolving features".into(),
        }
        .to_string();
        assert!(msg.contains("300"));
        assert!(msg.contains("still resolving features"));
    }

    // The hint says "stop the extras with `docker stop <id>`", which is only
    // actionable if the ids are actually on screen.
    #[test]
    fn multiple_running_containers_render_their_ids() {
        let msg = Error::MultipleRunningContainers {
            repo_root: "/r".into(),
            ids: vec!["abc123 (foo_devcontainer)".into(), "def456 (bar)".into()],
        }
        .to_string();
        assert!(msg.contains("abc123 (foo_devcontainer)"), "{msg}");
        assert!(msg.contains("def456 (bar)"), "{msg}");
    }

    // Discovery is a union over two labels, so the containers named here need
    // not share one. The case this error most often reports — a VS Code-created
    // container beside a plugin-created one — is exactly the case where they do
    // not: the first carries a UNC `local_folder`. Naming that label as if both
    // carried it sends the user to a `docker ps --filter` that finds one
    // container, and makes the tool look wrong when it is right.
    #[test]
    fn multiple_running_containers_do_not_claim_a_shared_label() {
        let msg = Error::MultipleRunningContainers {
            repo_root: "/r".into(),
            ids: vec!["abc123 (foo)".into(), "def456 (bar)".into()],
        }
        .to_string();
        assert!(msg.contains("/r"), "{msg} should name the repo");
        assert!(
            !msg.contains("devcontainer.local_folder"),
            "{msg} asserts a label the containers may not carry"
        );
    }

    #[test]
    fn unparseable_output_shows_the_offending_line() {
        let msg = Error::UpOutputUnparseable {
            detail: "expected object".into(),
            last_line: "<<garbage>>".into(),
        }
        .to_string();
        assert!(msg.contains("<<garbage>>"));
    }
}
