use std::path::Path;
use std::time::Duration;

use crate::error::Error;
use crate::run::{run, StderrMode};
use crate::util::tail;

#[derive(Debug, PartialEq, serde::Deserialize)]
pub struct UpResult {
    #[serde(rename = "containerId", default)]
    pub container_id: String,
    #[serde(rename = "remoteUser", default)]
    pub remote_user: Option<String>,
    #[serde(rename = "remoteWorkspaceFolder", default)]
    pub remote_workspace_folder: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub message: Option<String>,
}

pub fn up_argv(
    devcontainer_bin: &Path,
    repo_root: &Path,
    config_arg: Option<&Path>,
) -> Vec<String> {
    let mut argv = vec![
        devcontainer_bin.display().to_string(),
        "up".to_string(),
        "--workspace-folder".to_string(),
        repo_root.display().to_string(),
    ];
    if let Some(cfg) = config_arg {
        argv.push("--config".to_string());
        argv.push(cfg.display().to_string());
    }
    argv
}

pub fn parse_up_output(stdout: &str) -> Result<UpResult, Error> {
    let last = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| Error::UpOutputUnparseable {
            detail: "empty stdout".into(),
            last_line: String::new(),
        })?;
    let parsed: UpResult =
        serde_json::from_str(last.trim()).map_err(|e| Error::UpOutputUnparseable {
            detail: e.to_string(),
            last_line: last.to_string(),
        })?;
    if parsed.outcome != "success" {
        return Err(Error::UpFailed {
            exit_code: None,
            output_tail: parsed.message.clone().unwrap_or_else(|| last.to_string()),
        });
    }
    if parsed.container_id.is_empty() || parsed.remote_workspace_folder.is_empty() {
        return Err(Error::UpOutputUnparseable {
            detail: "success result missing containerId or remoteWorkspaceFolder".into(),
            last_line: last.to_string(),
        });
    }
    Ok(parsed)
}

pub fn bring_up(
    devcontainer_bin: &Path,
    repo_root: &Path,
    config_arg: Option<&Path>,
    timeout: Duration,
) -> Result<UpResult, Error> {
    eprintln!("bringing up dev container for {} ...", repo_root.display());
    let argv = up_argv(devcontainer_bin, repo_root, config_arg);
    let res = run(&argv, timeout, StderrMode::Inherit)?;
    if res.timed_out {
        return Err(Error::UpTimeout {
            secs: timeout.as_secs(),
            output_tail: tail(&res.stdout, 2000),
        });
    }
    if res.exit_code != Some(0) {
        return Err(Error::UpFailed {
            exit_code: res.exit_code,
            output_tail: tail(&res.stdout, 2000),
        });
    }
    parse_up_output(&res.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const OK: &str = r#"{"outcome":"success","containerId":"c0ffee","remoteUser":"vscode","remoteWorkspaceFolder":"/workspaces/proj"}"#;

    #[test]
    fn argv_without_custom_config() {
        let argv = up_argv(Path::new("/usr/bin/devcontainer"), Path::new("/r"), None);
        assert_eq!(
            argv,
            vec!["/usr/bin/devcontainer", "up", "--workspace-folder", "/r"]
        );
    }

    #[test]
    fn argv_with_custom_config() {
        let argv = up_argv(
            Path::new("devcontainer"),
            Path::new("/r"),
            Some(Path::new("/r/alt.json")),
        );
        assert_eq!(
            argv[4..],
            ["--config".to_string(), "/r/alt.json".to_string()]
        );
    }

    #[test]
    fn parses_a_success_line() {
        let up = parse_up_output(OK).unwrap();
        assert_eq!(up.container_id, "c0ffee");
        assert_eq!(up.remote_user.as_deref(), Some("vscode"));
        assert_eq!(up.remote_workspace_folder, "/workspaces/proj");
    }

    #[test]
    fn takes_the_last_nonempty_line() {
        let noisy = format!("progress 1\nprogress 2\n{OK}\n\n");
        assert!(parse_up_output(&noisy).is_ok());
    }

    #[test]
    fn empty_stdout_is_unparseable() {
        assert!(matches!(
            parse_up_output("  \n \n"),
            Err(crate::error::Error::UpOutputUnparseable { .. })
        ));
    }

    #[test]
    fn garbage_last_line_is_unparseable() {
        assert!(matches!(
            parse_up_output("something went wrong"),
            Err(crate::error::Error::UpOutputUnparseable { .. })
        ));
    }

    #[test]
    fn non_success_outcome_is_up_failed() {
        let line = r#"{"outcome":"error","message":"build failed"}"#;
        assert!(matches!(
            parse_up_output(line),
            Err(crate::error::Error::UpFailed { .. })
        ));
    }

    #[test]
    fn success_without_container_id_is_unparseable() {
        let line = r#"{"outcome":"success","remoteWorkspaceFolder":"/w"}"#;
        assert!(matches!(
            parse_up_output(line),
            Err(crate::error::Error::UpOutputUnparseable { .. })
        ));
    }
}
