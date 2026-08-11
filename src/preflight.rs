use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::Error;
use crate::run::{run, StderrMode};
use crate::util::tail;

pub fn find_devcontainer(path_var: &str) -> Result<PathBuf, Error> {
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join("devcontainer");
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(Error::DevcontainerCliMissing)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub fn check_docker(docker_bin: &str) -> Result<(), Error> {
    let argv = vec![
        docker_bin.to_string(),
        "version".into(),
        "--format".into(),
        "{{.Server.Version}}".into(),
    ];
    let res = run(&argv, Duration::from_secs(5), StderrMode::Capture).map_err(|e| {
        Error::DockerUnreachable {
            detail: e.to_string(),
        }
    })?;
    if res.exit_code == Some(0) && !res.stdout.trim().is_empty() {
        Ok(())
    } else {
        Err(Error::DockerUnreachable {
            detail: tail(res.stderr.trim(), 500),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn write_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn finds_devcontainer_on_the_given_path() {
        let tmp = tempfile::tempdir().unwrap();
        write_script(tmp.path(), "devcontainer", "exit 0");
        let found = find_devcontainer(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(found, tmp.path().join("devcontainer"));
    }

    #[test]
    fn missing_devcontainer_is_classified() {
        let tmp = tempfile::tempdir().unwrap();
        let err = find_devcontainer(tmp.path().to_str().unwrap()).unwrap_err();
        assert!(matches!(err, crate::error::Error::DevcontainerCliMissing));
    }

    #[test]
    fn docker_ok_requires_exit_zero_and_nonempty_stdout() {
        let tmp = tempfile::tempdir().unwrap();
        let ok = write_script(tmp.path(), "docker-ok", "echo 27.0.1");
        assert!(check_docker(ok.to_str().unwrap()).is_ok());
    }

    #[test]
    fn docker_exit_zero_with_empty_stdout_is_unreachable() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = write_script(tmp.path(), "docker-silent", "echo down >&2; exit 0");
        let err = check_docker(bad.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, crate::error::Error::DockerUnreachable { .. }));
    }

    #[test]
    fn docker_nonzero_exit_is_unreachable() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = write_script(tmp.path(), "docker-fail", "echo err >&2; exit 1");
        let err = check_docker(bad.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, crate::error::Error::DockerUnreachable { .. }));
    }

    #[test]
    fn docker_binary_missing_is_unreachable() {
        let err = check_docker("/nonexistent/docker").unwrap_err();
        assert!(matches!(err, crate::error::Error::DockerUnreachable { .. }));
    }
}
