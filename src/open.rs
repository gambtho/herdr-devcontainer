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
/// `HERDR_BIN_PATH` is a *preference*, not ground truth. herdr fills it from its
/// own `/proc/self/exe`, so upgrading herdr while the server keeps running
/// leaves the literal value `/home/u/.local/bin/herdr (deleted)` — a path that
/// cannot exist. Exec'ing it dies with a bare `ENOENT` that names nothing the
/// user can act on, so a value that is not an executable yields to a PATH lookup
/// instead of taking the process down.
pub fn resolve_herdr_bin(env_value: Option<&str>, path_var: &str) -> Result<String, Error> {
    if let Some(bin) = env_value
        .filter(|v| !v.is_empty())
        .filter(|v| preflight::is_executable(std::path::Path::new(v)))
    {
        return Ok(bin.to_string());
    }
    preflight::find_in_path(path_var, "herdr")
        .map(|p| p.display().to_string())
        .ok_or_else(|| {
            Error::Other(
                "cannot locate the herdr binary (HERDR_BIN_PATH unset or stale, none on PATH)"
                    .into(),
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

    fn write_herdr(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("herdr");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn env_var_wins_when_set() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_herdr(tmp.path());
        let bin = resolve_herdr_bin(Some(path.to_str().unwrap()), "").unwrap();
        assert_eq!(bin, path.display().to_string());
    }

    // herdr fills HERDR_BIN_PATH from its own /proc/self/exe, so upgrading herdr
    // while the server keeps running leaves the literal value
    // "/home/u/.local/bin/herdr (deleted)". Exec'ing that dies with a bare
    // ENOENT that names nothing, which is exactly how this surfaced in the TUI.
    // The variable is a preference, so an unusable one must yield to PATH rather
    // than take the process down.
    #[test]
    fn a_stale_env_var_yields_to_the_path_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_herdr(tmp.path());
        let deleted = format!("{} (deleted)", path.display());
        assert_eq!(
            resolve_herdr_bin(Some(&deleted), tmp.path().to_str().unwrap()).unwrap(),
            path.display().to_string()
        );
    }

    // A non-executable file is just as unusable as a missing one.
    #[test]
    fn a_non_executable_env_var_yields_to_the_path_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_herdr(tmp.path());
        let plain = tmp.path().join("not-executable");
        std::fs::write(&plain, "x").unwrap();
        assert_eq!(
            resolve_herdr_bin(Some(plain.to_str().unwrap()), tmp.path().to_str().unwrap()).unwrap(),
            path.display().to_string()
        );
    }

    // Falling back is only safe if the fallback failing still says why.
    #[test]
    fn a_stale_env_var_with_nothing_on_path_is_a_classified_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_herdr_bin(Some("/nope/herdr (deleted)"), "").unwrap_err();
        assert!(err.to_string().contains("herdr binary"), "{err}");
        assert!(resolve_herdr_bin(Some("/nope/herdr"), tmp.path().to_str().unwrap()).is_err());
    }

    #[test]
    fn falls_back_to_path_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_herdr(tmp.path());

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
