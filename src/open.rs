use crate::error::Error;
use crate::exec;
use crate::preflight;

pub fn open_argv(herdr_bin: &str, entrypoint: &str) -> Vec<String> {
    vec![
        herdr_bin.to_string(),
        "plugin".to_string(),
        "pane".to_string(),
        "open".to_string(),
        "--plugin".to_string(),
        "devcontainer".to_string(),
        "--entrypoint".to_string(),
        entrypoint.to_string(),
    ]
}

/// Resolve the herdr binary to re-invoke.
///
/// `HERDR_BIN_PATH` injection is confirmed only for herdr's *pane* spawn path;
/// the action path this runs on was never verified, and the herdr source was
/// not available to check. So the variable is a preference, not a requirement:
/// fall back to a PATH lookup rather than hard-failing on a variable that may
/// simply never be set here.
pub fn resolve_herdr_bin(env_value: Option<&str>, path_var: &str) -> Result<String, Error> {
    if let Some(bin) = env_value.filter(|v| !v.is_empty()) {
        return Ok(bin.to_string());
    }
    preflight::find_in_path(path_var, "herdr")
        .map(|p| p.display().to_string())
        .ok_or_else(|| {
            Error::Other(
                "cannot locate the herdr binary (HERDR_BIN_PATH unset, none on PATH)".into(),
            )
        })
}

pub fn run_open(entrypoint: Option<&str>) -> Result<(), Error> {
    let entrypoint = entrypoint.unwrap_or("shell");
    let env_value = std::env::var("HERDR_BIN_PATH").ok();
    let path_var = std::env::var("PATH").unwrap_or_default();
    let herdr_bin = resolve_herdr_bin(env_value.as_deref(), &path_var)?;
    Err(Error::Io(exec::exec_into(&open_argv(
        &herdr_bin, entrypoint,
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_argv_targets_our_plugin_entrypoint() {
        let argv = open_argv("/usr/bin/herdr", "shell");
        assert_eq!(
            argv,
            vec![
                "/usr/bin/herdr",
                "plugin",
                "pane",
                "open",
                "--plugin",
                "devcontainer",
                "--entrypoint",
                "shell"
            ]
        );
    }

    #[test]
    fn env_var_wins_when_set() {
        let bin = resolve_herdr_bin(Some("/opt/herdr"), "").unwrap();
        assert_eq!(bin, "/opt/herdr");
    }

    #[test]
    fn falls_back_to_path_lookup() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("herdr");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let dir = tmp.path().to_str().unwrap();
        assert_eq!(
            resolve_herdr_bin(None, dir).unwrap(),
            path.display().to_string()
        );
        assert_eq!(
            resolve_herdr_bin(Some(""), dir).unwrap(),
            path.display().to_string()
        );
    }

    #[test]
    fn unresolvable_herdr_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_herdr_bin(None, tmp.path().to_str().unwrap()).is_err());
    }
}
