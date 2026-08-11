use std::path::{Path, PathBuf};

use crate::error::Error;

#[derive(Debug, Default, serde::Deserialize)]
pub struct PluginContext {
    pub focused_pane_cwd: Option<String>,
    pub workspace_cwd: Option<String>,
    pub worktree: Option<WorktreeContext>,
}

#[derive(Debug, serde::Deserialize)]
pub struct WorktreeContext {
    pub repo_root: Option<String>,
}

pub fn parse_context(raw: &str) -> PluginContext {
    serde_json::from_str(raw).unwrap_or_default()
}

pub fn load_context() -> PluginContext {
    std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .map(|raw| parse_context(&raw))
        .unwrap_or_default()
}

pub fn resolve_repo_root(ctx: &PluginContext, process_cwd: &Path) -> Result<PathBuf, Error> {
    if let Some(root) = ctx.worktree.as_ref().and_then(|w| w.repo_root.as_deref()) {
        if let Ok(canon) = Path::new(root).canonicalize() {
            return Ok(canon);
        }
    }
    let candidates = [
        ctx.focused_pane_cwd.as_deref().map(Path::new),
        ctx.workspace_cwd.as_deref().map(Path::new),
        Some(process_cwd),
    ];
    for dir in candidates.into_iter().flatten() {
        if let Some(root) = main_worktree_root(dir) {
            return Ok(root);
        }
    }
    Err(Error::NotAGitRepo {
        cwd: process_cwd.display().to_string(),
    })
}

/// The first `worktree` entry of `git worktree list --porcelain` is the main
/// worktree — the repo-scoped container identity from the spec.
fn main_worktree_root(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().find_map(|l| l.strip_prefix("worktree "))?;
    Path::new(first).canonicalize().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn make_repo(dir: &Path) {
        git(dir, &["init", "-b", "main"]);
        git(
            dir,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-m",
                "x",
            ],
        );
    }

    #[test]
    fn garbage_context_parses_to_default() {
        let ctx = parse_context("not json at all");
        assert!(ctx.focused_pane_cwd.is_none());
        assert!(ctx.worktree.is_none());
    }

    #[test]
    fn full_context_parses_and_ignores_unknown_fields() {
        let raw = r#"{"focused_pane_cwd":"/a","workspace_cwd":"/b",
                      "worktree":{"repo_root":"/c","repo_key":"k","is_linked_worktree":true},
                      "selected_text":"ignored"}"#;
        let ctx = parse_context(raw);
        assert_eq!(ctx.focused_pane_cwd.as_deref(), Some("/a"));
        assert_eq!(ctx.worktree.unwrap().repo_root.as_deref(), Some("/c"));
    }

    #[test]
    fn worktree_repo_root_wins_when_present_and_valid() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path());
        let ctx = PluginContext {
            worktree: Some(WorktreeContext {
                repo_root: Some(tmp.path().display().to_string()),
            }),
            ..Default::default()
        };
        let root = resolve_repo_root(&ctx, Path::new("/")).unwrap();
        assert_eq!(root, tmp.path().canonicalize().unwrap());
    }

    #[test]
    fn linked_worktree_resolves_to_the_main_root_via_git() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir(&main).unwrap();
        make_repo(&main);
        let linked = tmp.path().join("linked");
        git(
            &main,
            &["worktree", "add", linked.to_str().unwrap(), "-b", "wt"],
        );
        let ctx = PluginContext {
            focused_pane_cwd: Some(linked.display().to_string()),
            ..Default::default()
        };
        let root = resolve_repo_root(&ctx, Path::new("/")).unwrap();
        assert_eq!(root, main.canonicalize().unwrap());
    }

    #[test]
    fn no_repo_anywhere_is_a_classified_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = PluginContext::default();
        let err = resolve_repo_root(&ctx, tmp.path()).unwrap_err();
        assert!(matches!(err, crate::error::Error::NotAGitRepo { .. }));
    }
}
