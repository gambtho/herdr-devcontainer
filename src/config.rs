use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::Error;

pub const DEFAULT_COMMAND: &str = "claude";
pub const DEFAULT_UP_TIMEOUT_SECS: u64 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enabled {
    Auto,
    True,
    False,
}

#[derive(Clone, Debug)]
pub struct RepoConfig {
    pub enabled: Enabled,
    pub config: Option<String>,
}

impl Default for RepoConfig {
    fn default() -> Self {
        RepoConfig {
            enabled: Enabled::Auto,
            config: None,
        }
    }
}

#[derive(Debug)]
pub struct Config {
    pub command: String,
    pub up_timeout_secs: u64,
    pub warnings: Vec<String>,
    repos: BTreeMap<PathBuf, RepoConfig>,
}

impl Config {
    pub fn default_config() -> Self {
        Config {
            command: DEFAULT_COMMAND.to_string(),
            up_timeout_secs: DEFAULT_UP_TIMEOUT_SECS,
            warnings: Vec::new(),
            repos: BTreeMap::new(),
        }
    }

    /// Per-repo settings; `canonical_root` should already be canonicalized.
    pub fn repo(&self, canonical_root: &Path) -> RepoConfig {
        for (key, rc) in &self.repos {
            let matches = key
                .canonicalize()
                .map(|c| c == canonical_root)
                .unwrap_or_else(|_| key.as_path() == canonical_root);
            if matches {
                return rc.clone();
            }
        }
        RepoConfig::default()
    }
}

pub fn load() -> Result<Config, Error> {
    load_from(&config_path())
}

pub fn load_from(path: &Path) -> Result<Config, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default_config()),
        Err(e) => Err(Error::ConfigUnreadable {
            path: path.display().to_string(),
            detail: e.to_string(),
        }),
    }
}

/// Resolve a repo-relative `config` value. `Path::join` replaces the base
/// wholesale when handed an absolute path, so an unguarded join would let
/// `config = "/etc/x.json"` escape the repo entirely.
pub fn resolve_repo_relative(repo_root: &Path, rel: &str) -> Result<PathBuf, Error> {
    let candidate = Path::new(rel);
    let escapes = candidate.is_absolute()
        || candidate
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
    if escapes {
        return Err(Error::InvalidConfigPath {
            repo_root: repo_root.display().to_string(),
            value: rel.to_string(),
        });
    }
    Ok(repo_root.join(candidate))
}

fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("herdr-devcontainer").join("config.toml")
}

pub fn parse(text: &str) -> Config {
    let mut cfg = Config::default_config();
    let table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            cfg.warnings
                .push(format!("config unreadable, using defaults: {e}"));
            return cfg;
        }
    };
    for (key, value) in table {
        match key.as_str() {
            "command" => match value.as_str() {
                Some(s) if !s.trim().is_empty() => cfg.command = s.to_string(),
                _ => cfg
                    .warnings
                    .push("`command` must be a non-empty string".into()),
            },
            "up_timeout_secs" => match value.as_integer() {
                Some(n) if n > 0 => cfg.up_timeout_secs = n as u64,
                _ => cfg
                    .warnings
                    .push("`up_timeout_secs` must be a positive integer".into()),
            },
            "repos" => parse_repos(value, &mut cfg),
            other => cfg
                .warnings
                .push(format!("unknown config key `{other}` ignored")),
        }
    }
    cfg
}

fn parse_repos(value: toml::Value, cfg: &mut Config) {
    let Some(table) = value.as_table() else {
        cfg.warnings.push("`repos` must be a table".into());
        return;
    };
    for (root, entry) in table {
        let Some(entry) = entry.as_table() else {
            cfg.warnings
                .push(format!("repos.\"{root}\" must be a table"));
            continue;
        };
        let mut rc = RepoConfig::default();
        for (key, val) in entry {
            match key.as_str() {
                "enabled" => match val.as_str() {
                    Some("auto") => rc.enabled = Enabled::Auto,
                    Some("true") => rc.enabled = Enabled::True,
                    Some("false") => rc.enabled = Enabled::False,
                    _ => cfg.warnings.push(format!(
                        "repos.\"{root}\".enabled must be \"auto\", \"true\", or \"false\""
                    )),
                },
                "config" => match val.as_str() {
                    Some(s) => rc.config = Some(s.to_string()),
                    None => cfg
                        .warnings
                        .push(format!("repos.\"{root}\".config must be a string")),
                },
                other => cfg
                    .warnings
                    .push(format!("unknown key repos.\"{root}\".{other} ignored")),
            }
        }
        cfg.repos.insert(PathBuf::from(root), rc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn empty_text_gives_defaults() {
        let cfg = parse("");
        assert_eq!(cfg.command, "claude");
        assert_eq!(cfg.up_timeout_secs, 300);
        assert!(cfg.warnings.is_empty());
    }

    #[test]
    fn invalid_toml_gives_defaults_plus_warning() {
        let cfg = parse("this is [not toml");
        assert_eq!(cfg.command, "claude");
        assert_eq!(cfg.warnings.len(), 1);
    }

    #[test]
    fn full_config_parses() {
        let cfg = parse(
            r#"
            command = "codex"
            up_timeout_secs = 60

            [repos."/x/repo"]
            enabled = "false"
            config = ".devcontainer/alt.json"
            "#,
        );
        assert_eq!(cfg.command, "codex");
        assert_eq!(cfg.up_timeout_secs, 60);
        let rc = cfg.repo(Path::new("/x/repo"));
        assert_eq!(rc.enabled, Enabled::False);
        assert_eq!(rc.config.as_deref(), Some(".devcontainer/alt.json"));
    }

    #[test]
    fn unknown_keys_warn_but_do_not_fail() {
        let cfg = parse("command = \"claude\"\nshiny = true\n");
        assert_eq!(cfg.warnings.len(), 1);
        assert!(cfg.warnings[0].contains("shiny"));
    }

    #[test]
    fn bad_enabled_value_warns_and_defaults_to_auto() {
        let cfg = parse("[repos.\"/x\"]\nenabled = \"maybe\"\n");
        assert_eq!(cfg.repo(Path::new("/x")).enabled, Enabled::Auto);
        assert_eq!(cfg.warnings.len(), 1);
    }

    #[test]
    fn unlisted_repo_gets_the_default() {
        let cfg = parse("");
        assert_eq!(cfg.repo(Path::new("/nowhere")).enabled, Enabled::Auto);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let cfg = load_from(Path::new("/nonexistent/herdr-devcontainer/config.toml")).unwrap();
        assert_eq!(cfg.command, "claude");
    }

    // A directory at the config path reproduces "readable path, failing read"
    // without needing root or a chmod dance that CI may run as uid 0.
    #[test]
    fn unreadable_file_is_an_error_not_a_default() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_from(dir.path()).unwrap_err();
        assert!(matches!(err, Error::ConfigUnreadable { .. }));
    }

    #[test]
    fn repo_relative_paths_resolve_under_the_root() {
        let got = resolve_repo_relative(Path::new("/r"), ".devcontainer/alt.json").unwrap();
        assert_eq!(got, Path::new("/r/.devcontainer/alt.json"));
    }

    // Path::join silently discards the root for an absolute argument, so an
    // absolute `config` would escape the repo without this guard.
    #[test]
    fn absolute_config_paths_are_rejected() {
        let err = resolve_repo_relative(Path::new("/r"), "/etc/devcontainer.json").unwrap_err();
        assert!(matches!(err, Error::InvalidConfigPath { .. }));
    }

    #[test]
    fn parent_traversal_is_rejected() {
        let err = resolve_repo_relative(Path::new("/r"), "../other/devcontainer.json").unwrap_err();
        assert!(matches!(err, Error::InvalidConfigPath { .. }));
    }
}
