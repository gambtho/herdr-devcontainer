use std::path::{Path, PathBuf};

use crate::config::{Enabled, RepoConfig};
use crate::error::Error;

#[derive(Debug, PartialEq, Eq)]
pub struct Detection {
    /// Explicit `--config` argument for `devcontainer up`, when configured.
    pub config_arg: Option<PathBuf>,
}

pub fn detect(repo_root: &Path, rc: &RepoConfig) -> Result<Detection, Error> {
    // Validate before branching on `enabled`: `True` skips the stat, so a check
    // placed later would let an escaping path through in that mode.
    let config_arg = match rc.config.as_deref() {
        Some(rel) => Some(crate::config::resolve_repo_relative(repo_root, rel)?),
        None => None,
    };
    match rc.enabled {
        Enabled::False => Err(Error::DisabledByConfig {
            repo_root: repo_root.display().to_string(),
        }),
        Enabled::True => Ok(Detection { config_arg }),
        Enabled::Auto => {
            let candidates: Vec<PathBuf> = match &config_arg {
                Some(p) => vec![p.clone()],
                None => vec![
                    repo_root.join(".devcontainer").join("devcontainer.json"),
                    repo_root.join(".devcontainer.json"),
                ],
            };
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
