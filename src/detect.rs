use std::path::{Path, PathBuf};

use crate::config::{Enabled, RepoConfig};
use crate::error::Error;

#[derive(Debug, PartialEq, Eq)]
pub struct Detection {
    /// Explicit `--config` argument for `devcontainer up`, when configured.
    pub config_arg: Option<PathBuf>,
}

/// The config paths a `devcontainer.config_file` label could name for this
/// repo. Shared with discovery: the CLI writes the config path it resolved into
/// that label, so these are exactly the values worth looking a container up by.
pub fn config_candidates(repo_root: &Path, config_arg: Option<&Path>) -> Vec<PathBuf> {
    match config_arg {
        Some(p) => vec![p.to_path_buf()],
        None => vec![
            repo_root.join(".devcontainer").join("devcontainer.json"),
            repo_root.join(".devcontainer.json"),
        ],
    }
}

/// Resolve a repo-configured `config` to an absolute path, rejecting one that
/// escapes the repo.
///
/// Shared with `stop`, which needs the same value for discovery but must not
/// run the rest of `detect` — a repo with `enabled = "false"` still has a
/// container worth stopping. Two independent derivations would eventually
/// drift, and a `stop` looking up a path no container's label holds is exactly
/// the "no running dev container" failure this discovery path exists to fix.
pub fn resolve_config_arg(repo_root: &Path, rc: &RepoConfig) -> Result<Option<PathBuf>, Error> {
    match rc.config.as_deref() {
        Some(rel) => Ok(Some(crate::config::resolve_repo_relative(repo_root, rel)?)),
        None => Ok(None),
    }
}

pub fn detect(repo_root: &Path, rc: &RepoConfig) -> Result<Detection, Error> {
    // Validate before branching on `enabled`: `True` skips the stat, so a check
    // placed later would let an escaping path through in that mode.
    let config_arg = resolve_config_arg(repo_root, rc)?;
    match rc.enabled {
        Enabled::False => Err(Error::DisabledByConfig {
            repo_root: repo_root.display().to_string(),
        }),
        Enabled::True => Ok(Detection { config_arg }),
        Enabled::Auto => {
            let candidates = config_candidates(repo_root, config_arg.as_deref());
            for path in &candidates {
                match std::fs::metadata(path) {
                    Ok(_) => return Ok(Detection { config_arg }),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(Error::Io(e)),
                }
            }
            Err(Error::NoDevcontainerConfig {
                repo_root: repo_root.display().to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Enabled, RepoConfig};

    fn rc(enabled: Enabled, config: Option<&str>) -> RepoConfig {
        RepoConfig {
            enabled,
            config: config.map(String::from),
            ..RepoConfig::default()
        }
    }

    // Discovery needs the same paths `detect` stats, because the CLI writes the
    // resolved config path into `devcontainer.config_file`. A custom `config`
    // replaces the standard pair rather than adding to it: that is the only
    // path `devcontainer up --config` would have used, so it is the only value
    // the label can hold.
    #[test]
    fn config_candidates_are_the_two_standard_locations() {
        let got = config_candidates(Path::new("/r"), None);
        assert_eq!(
            got,
            vec![
                PathBuf::from("/r/.devcontainer/devcontainer.json"),
                PathBuf::from("/r/.devcontainer.json"),
            ]
        );
    }

    #[test]
    fn a_custom_config_is_the_only_candidate() {
        let got = config_candidates(Path::new("/r"), Some(Path::new("/r/alt/devc.json")));
        assert_eq!(got, vec![PathBuf::from("/r/alt/devc.json")]);
    }

    #[test]
    fn disabled_is_a_classified_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = detect(tmp.path(), &rc(Enabled::False, None)).unwrap_err();
        assert!(matches!(err, crate::error::Error::DisabledByConfig { .. }));
    }

    #[test]
    fn forced_true_skips_the_stat() {
        let tmp = tempfile::tempdir().unwrap(); // no devcontainer files at all
        let det = detect(tmp.path(), &rc(Enabled::True, None)).unwrap();
        assert_eq!(det.config_arg, None);
    }

    #[test]
    fn auto_finds_the_standard_directory_config() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("devcontainer.json"), "{}").unwrap();
        assert!(detect(tmp.path(), &rc(Enabled::Auto, None)).is_ok());
    }

    #[test]
    fn auto_finds_the_top_level_config() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".devcontainer.json"), "{}").unwrap();
        assert!(detect(tmp.path(), &rc(Enabled::Auto, None)).is_ok());
    }

    #[test]
    fn auto_with_custom_path_stats_only_that_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".devcontainer.json"), "{}").unwrap(); // standard exists
        let err = detect(tmp.path(), &rc(Enabled::Auto, Some("alt/devc.json"))).unwrap_err();
        assert!(matches!(
            err,
            crate::error::Error::NoDevcontainerConfig { .. }
        ));
    }

    #[test]
    fn auto_without_any_config_is_a_classified_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = detect(tmp.path(), &rc(Enabled::Auto, None)).unwrap_err();
        assert!(matches!(
            err,
            crate::error::Error::NoDevcontainerConfig { .. }
        ));
    }

    #[test]
    fn custom_config_becomes_the_config_arg() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("alt")).unwrap();
        std::fs::write(tmp.path().join("alt/devc.json"), "{}").unwrap();
        let det = detect(tmp.path(), &rc(Enabled::Auto, Some("alt/devc.json"))).unwrap();
        assert_eq!(det.config_arg, Some(tmp.path().join("alt/devc.json")));
    }

    // Rejection must happen before the stat, or `Enabled::True` would skip the
    // check entirely and hand an escaping path straight to `devcontainer up`.
    #[test]
    fn escaping_custom_paths_are_rejected_in_every_mode() {
        let tmp = tempfile::tempdir().unwrap();
        for mode in [Enabled::Auto, Enabled::True] {
            let err = detect(tmp.path(), &rc(mode, Some("/etc/devc.json"))).unwrap_err();
            assert!(matches!(err, crate::error::Error::InvalidConfigPath { .. }));
            let err = detect(tmp.path(), &rc(mode, Some("../devc.json"))).unwrap_err();
            assert!(matches!(err, crate::error::Error::InvalidConfigPath { .. }));
        }
    }
}
