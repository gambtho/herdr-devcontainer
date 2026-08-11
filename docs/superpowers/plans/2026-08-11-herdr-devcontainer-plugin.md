# herdr-devcontainer Plugin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A herdr plugin whose panes run inside the Dev Container of the current repo: a Rust wrapper binary (`herdr-devc`) declared in `herdr-plugin.toml` that resolves the repo, runs `devcontainer up`, and execs into `docker exec`.

**Architecture:** Library crate (`herdr_devcontainer`) with one focused module per concern (context, config, detect, preflight, lock, up, workdir, exec, stop) plus a thin `herdr-devc` binary that dispatches `pane | stop | open` and enforces hold-on-error. All host subprocesses are direct argv (no shell); the wrapper replaces itself with `docker exec` via `exec()`. Design rationale and all herdr/ProjectMux evidence: `docs/superpowers/specs/2026-08-11-herdr-devcontainer-plugin-design.md` — read it first.

**Tech Stack:** Rust 2021 (toolchain ≥ 1.74). Dependencies: `serde`+`serde_json` (context/up JSON), `toml` (config), `thiserror` (error enum), `nix` (flock, killpg), `sha2` (lock filename). Dev: `tempfile`.

## Global Constraints

- Linux/WSL2 only. No macOS/Windows code paths.
- Work in a linked worktree, NOT the primary checkout — a worktree guard blocks writes at `~/workspace/herdr-devcontainer`. The branch `design` already exists at `.worktrees/design` with the spec committed; continue on it (`cd .worktrees/design`).
- Never interpolate repo-derived strings into a host shell. Host spawns are direct argv only. Only the *container-side* command goes through `sh -lc`.
- Never `docker rm`. Never stop containers except in the explicit `stop` subcommand.
- The command executed with user privileges comes only from the user config file or the built-in default `claude` — never from repo content.
- On any user-facing failure, print the classified error + hint and wait for Enter before exiting (herdr may close the pane on exit; the message must survive).
- `min_herdr_version = "0.8.0"` in the plugin manifest.
- Binary name: `herdr-devc`. Plugin id: `devcontainer`. Config: `~/.config/herdr-devcontainer/config.toml`. Locks: `$XDG_STATE_HOME/herdr-devcontainer/locks/` (default `~/.local/state/...`).
- Run `cargo fmt` before every commit; run `cargo clippy --all-targets` at the end of each task and fix warnings in code you wrote.

---

### Task 1: Cargo scaffold + `util::tail`

**Files:**
- Create: `Cargo.toml`, `src/lib.rs`, `src/main.rs`, `src/util.rs`

**Interfaces:**
- Produces: crate `herdr_devcontainer` (lib) + bin `herdr-devc`; `util::tail(s: &str, max_chars: usize) -> String` used by later tasks for bounded error tails.

- [ ] **Step 1: Create `Cargo.toml`**

```toml
[package]
name = "herdr-devcontainer"
version = "0.1.0"
edition = "2021"
rust-version = "1.74"

[lib]
name = "herdr_devcontainer"
path = "src/lib.rs"

[[bin]]
name = "herdr-devc"
path = "src/main.rs"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
thiserror = "1"
nix = { version = "0.29", features = ["fs", "signal"] }
sha2 = "0.10"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Create `src/lib.rs` and `src/util.rs` with a failing test**

`src/lib.rs`:

```rust
pub mod util;
```

`src/util.rs`:

```rust
/// Last `max_chars` characters of `s`, on a char boundary.
pub fn tail(s: &str, max_chars: usize) -> String {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_returns_short_strings_unchanged() {
        assert_eq!(tail("abc", 10), "abc");
    }

    #[test]
    fn tail_cuts_to_the_last_max_chars() {
        assert_eq!(tail("abcdef", 3), "def");
    }

    #[test]
    fn tail_respects_multibyte_boundaries() {
        assert_eq!(tail("héllo", 4), "éllo");
    }
}
```

`src/main.rs`:

```rust
fn main() {
    eprintln!("usage: herdr-devc <pane [--shell] | stop | open <entrypoint>>");
    std::process::exit(2);
}
```

- [ ] **Step 3: Run tests — expect FAIL** — `cargo test` → the three `util` tests panic with `not implemented`.

- [ ] **Step 4: Implement `tail`**

```rust
/// Last `max_chars` characters of `s`, on a char boundary.
pub fn tail(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        s.chars().skip(count - max_chars).collect()
    }
}
```

- [ ] **Step 5: Run tests — expect PASS** — `cargo test`

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src
git commit -m "feat: cargo scaffold with lib/bin split and util::tail"
```

---

### Task 2: Error classification (`src/error.rs`)

**Files:**
- Create: `src/error.rs`; Modify: `src/lib.rs` (add `pub mod error;`)

**Interfaces:**
- Produces: `error::Error` (all variants below) and `Error::hint(&self) -> Option<&str>`. Every later module returns `Result<_, Error>`; `Io` has `#[from] std::io::Error`.

- [ ] **Step 1: Write failing tests** (append to `src/error.rs` after the enum stub; start with only the enum's derive line and `unimplemented!()`-free — the enum itself is data, so write it fully in this step; the *tests* pin the messages)

```rust
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
    #[error("`devcontainer up` failed (exit code {exit_code:?})\n--- last output ---\n{output_tail}")]
    UpFailed {
        exit_code: Option<i32>,
        output_tail: String,
    },
    #[error("could not parse `devcontainer up` output: {detail}\nlast line: {last_line}")]
    UpOutputUnparseable { detail: String, last_line: String },
    #[error("could not parse `docker ps` output line: {line}")]
    MalformedDockerOutput { line: String },
    #[error("multiple running dev containers carry label devcontainer.local_folder={repo_root}; refusing to choose")]
    MultipleRunningContainers { repo_root: String, ids: Vec<String> },
    #[error("docker command failed: {detail}")]
    DockerCommandFailed { detail: String },
    #[error("{0}")]
    Other(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
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
```

- [ ] **Step 2: Run — expect FAIL** — `cargo test error` fails: `hint` not defined.

- [ ] **Step 3: Implement `hint`**

```rust
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
            _ => None,
        }
    }
}
```

- [ ] **Step 4: Run — expect PASS** — `cargo test error`

- [ ] **Step 5: Commit** — `git add src && git commit -m "feat: classified error type with user hints"`

---

### Task 3: Subprocess runner (`src/run.rs`)

Ported ProjectMux subprocess hygiene: own process group, group SIGKILL on timeout (a grandchild holding the stdout pipe would otherwise block forever), bounded capture.

**Files:**
- Create: `src/run.rs`; Modify: `src/lib.rs` (add `pub mod run;`)

**Interfaces:**
- Produces:
  - `run::CAPTURE_LIMIT: usize` (64 * 1024)
  - `run::StderrMode { Capture, Inherit }` — `Inherit` streams stderr live to the pane (used by `devcontainer up`), `Capture` collects it (used by docker probes).
  - `run::RunResult { exit_code: Option<i32>, stdout: String, stderr: String, timed_out: bool }`
  - `run::run(argv: &[String], timeout: Duration, stderr_mode: StderrMode) -> std::io::Result<RunResult>`

- [ ] **Step 1: Write failing tests** in `src/run.rs` (module body starts with just the public items as `unimplemented!()` stubs matching the signatures above)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn captures_stdout_and_exit_code() {
        let res = run(
            &sh("echo hi; exit 3"),
            Duration::from_secs(5),
            StderrMode::Capture,
        )
        .unwrap();
        assert_eq!(res.stdout.trim(), "hi");
        assert_eq!(res.exit_code, Some(3));
        assert!(!res.timed_out);
    }

    #[test]
    fn captures_stderr_in_capture_mode() {
        let res = run(
            &sh("echo oops >&2"),
            Duration::from_secs(5),
            StderrMode::Capture,
        )
        .unwrap();
        assert_eq!(res.stderr.trim(), "oops");
    }

    #[test]
    fn timeout_kills_the_whole_process_group() {
        // The backgrounded sleep inherits the stdout pipe; without a group
        // kill, draining stdout would block until it exits on its own.
        let start = Instant::now();
        let res = run(
            &sh("sleep 30 & sleep 30"),
            Duration::from_millis(300),
            StderrMode::Capture,
        )
        .unwrap();
        assert!(res.timed_out);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stdout_capture_is_bounded() {
        let res = run(
            &sh("head -c 200000 /dev/zero | tr '\\0' 'a'"),
            Duration::from_secs(10),
            StderrMode::Capture,
        )
        .unwrap();
        assert_eq!(res.stdout.len(), CAPTURE_LIMIT);
    }

    #[test]
    fn missing_binary_is_an_io_error() {
        let argv = vec!["/nonexistent/definitely-not-a-binary".to_string()];
        assert!(run(&argv, Duration::from_secs(1), StderrMode::Capture).is_err());
    }
}
```

- [ ] **Step 2: Run — expect FAIL** — `cargo test run::` panics on `unimplemented!()`.

- [ ] **Step 3: Implement**

```rust
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const CAPTURE_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StderrMode {
    Capture,
    Inherit,
}

#[derive(Debug)]
pub struct RunResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

pub fn run(
    argv: &[String],
    timeout: Duration,
    stderr_mode: StderrMode,
) -> std::io::Result<RunResult> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0);
    match stderr_mode {
        StderrMode::Capture => cmd.stderr(Stdio::piped()),
        StderrMode::Inherit => cmd.stderr(Stdio::inherit()),
    };
    let mut child = cmd.spawn()?;
    let stdout_thread = capture_thread(child.stdout.take());
    let stderr_thread = match stderr_mode {
        StderrMode::Capture => Some(capture_thread(child.stderr.take())),
        StderrMode::Inherit => None,
    };

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_group(&child);
            break child.wait().ok();
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();
    Ok(RunResult {
        exit_code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        timed_out,
    })
}

fn kill_group(child: &Child) {
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
}

fn capture_thread<R: Read + Send + 'static>(stream: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(mut stream) = stream {
            let mut buf = [0u8; 8192];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out.len() < CAPTURE_LIMIT {
                            let take = n.min(CAPTURE_LIMIT - out.len());
                            out.extend_from_slice(&buf[..take]);
                        }
                    }
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    })
}
```

- [ ] **Step 4: Run — expect PASS** — `cargo test run::`

- [ ] **Step 5: Commit** — `git add src && git commit -m "feat: subprocess runner with timeout, group kill, bounded capture"`

---

### Task 4: Plugin context + repo resolution (`src/context.rs`)

The fallback chain from the spec: `worktree.repo_root` from `HERDR_PLUGIN_CONTEXT_JSON`, else derive the **main worktree root** via git from `focused_pane_cwd`, then `workspace_cwd`, then the process cwd. The first entry of `git worktree list --porcelain` is the main worktree.

The process-cwd rung is spec step 1d: herdr sets the pane cwd from the focused pane, so it is a meaningful last resort when the context JSON is absent entirely (a bare `herdr-devc pane` run from a terminal, for instance). It comes *after* every context-supplied candidate, so it never overrides herdr's own answer.

**Files:**
- Create: `src/context.rs`; Modify: `src/lib.rs` (add `pub mod context;`)

**Interfaces:**
- Consumes: `error::Error`.
- Produces:
  - `context::PluginContext { focused_pane_cwd: Option<String>, workspace_cwd: Option<String>, worktree: Option<WorktreeContext> }` (serde `Deserialize`, `Default`; unknown JSON fields ignored)
  - `context::WorktreeContext { repo_root: Option<String> }`
  - `context::parse_context(raw: &str) -> PluginContext` (garbage → default)
  - `context::load_context() -> PluginContext` (reads `HERDR_PLUGIN_CONTEXT_JSON`)
  - `context::resolve_repo_root(ctx: &PluginContext, process_cwd: &Path) -> Result<PathBuf, Error>` (canonicalized)

- [ ] **Step 1: Write failing tests**

```rust
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
```

- [ ] **Step 2: Run — expect FAIL** — `cargo test context::` (stubs `unimplemented!()`).

- [ ] **Step 3: Implement**

```rust
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
```

- [ ] **Step 4: Run — expect PASS** — `cargo test context::`

- [ ] **Step 5: Commit** — `git add src && git commit -m "feat: plugin context parsing and main-repo-root resolution"`

---

### Task 5: Config (`src/config.rs`)

Flat TOML, zero-config defaults, unknown keys are warnings (never errors). `enabled` is the *string* `"auto" | "true" | "false"` (matching the spec), not a TOML boolean.

**Files:**
- Create: `src/config.rs`; Modify: `src/lib.rs` (add `pub mod config;`)

**Interfaces:**
- Produces:
  - `config::Enabled { Auto, True, False }` (Copy, PartialEq)
  - `config::RepoConfig { enabled: Enabled, config: Option<String> }` (Clone, Default = Auto/None)
  - `config::Config { command: String, up_timeout_secs: u64, warnings: Vec<String>, .. }`
  - `Config::default_config() -> Config` (command `"claude"`, timeout 300)
  - `config::parse(text: &str) -> Config` (never fails; syntax errors → default + warning)
  - `config::load_from(path: &Path) -> Result<Config, Error>` (`NotFound` → defaults; any other I/O error → `Error::ConfigUnreadable`)
  - `config::load() -> Result<Config, Error>` (`load_from` on `$XDG_CONFIG_HOME|~/.config` + `herdr-devcontainer/config.toml`)
  - `Config::repo(&self, canonical_root: &Path) -> RepoConfig`
  - `config::resolve_repo_relative(repo_root: &Path, rel: &str) -> Result<PathBuf, Error>` (rejects absolute paths and `..`)

`load` returns `Result` rather than swallowing errors because `Path::read_to_string` failing on permissions is not the same fact as the file being absent. Collapsing them lets an unreadable config silently re-enable a repo the user set to `enabled = "false"` — a config file that fails open on the *disable* switch.

- [ ] **Step 1: Write failing tests**

```rust
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
```

- [ ] **Step 2: Run — expect FAIL** — `cargo test config::`

- [ ] **Step 3: Implement**

```rust
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
```

- [ ] **Step 4: Run — expect PASS** — `cargo test config::`

- [ ] **Step 5: Commit** — `git add src && git commit -m "feat: flat TOML config with warnings-not-errors semantics"`

---

### Task 6: Detection (`src/detect.rs`)

Stat-only (never parse devcontainer.json); not-exist continues, any other stat error is an error — ProjectMux's "unknown funnel".

**Files:**
- Create: `src/detect.rs`; Modify: `src/lib.rs` (add `pub mod detect;`)

**Interfaces:**
- Consumes: `config::{Enabled, RepoConfig}`, `error::Error`.
- Produces: `detect::Detection { config_arg: Option<PathBuf> }`; `detect::detect(repo_root: &Path, rc: &RepoConfig) -> Result<Detection, Error>`.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Enabled, RepoConfig};

    fn rc(enabled: Enabled, config: Option<&str>) -> RepoConfig {
        RepoConfig {
            enabled,
            config: config.map(String::from),
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
            assert!(matches!(
                err,
                crate::error::Error::InvalidConfigPath { .. }
            ));
            let err = detect(tmp.path(), &rc(mode, Some("../devc.json"))).unwrap_err();
            assert!(matches!(
                err,
                crate::error::Error::InvalidConfigPath { .. }
            ));
        }
    }
}
```

- [ ] **Step 2: Run — expect FAIL** — `cargo test detect::`

- [ ] **Step 3: Implement**

```rust
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
```

- [ ] **Step 4: Run — expect PASS** — `cargo test detect::`

- [ ] **Step 5: Commit** — `git add src && git commit -m "feat: stat-only devcontainer detection with enabled semantics"`

---

### Task 7: Preflight (`src/preflight.rs`)

Docker reachability requires exit 0 **and** non-empty stdout — with the daemon down, some docker versions exit 0 with the error on stderr (ProjectMux, verified).

**Files:**
- Create: `src/preflight.rs`; Modify: `src/lib.rs` (add `pub mod preflight;`)

**Interfaces:**
- Consumes: `run::{run, StderrMode}`, `error::Error`, `util::tail`.
- Produces: `preflight::find_devcontainer(path_var: &str) -> Result<PathBuf, Error>`; `preflight::check_docker(docker_bin: &str) -> Result<(), Error>`. (Both take their lookup input as a parameter so tests never mutate the process PATH.)

- [ ] **Step 1: Write failing tests**

```rust
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
```

- [ ] **Step 2: Run — expect FAIL** — `cargo test preflight::`

- [ ] **Step 3: Implement**

```rust
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
```

- [ ] **Step 4: Run — expect PASS** — `cargo test preflight::`

- [ ] **Step 5: Commit** — `git add src && git commit -m "feat: preflight checks for devcontainer CLI and docker daemon"`

---

### Task 8: Per-repo lock (`src/lockfile.rs`)

flock in the XDG state dir, keyed by sha256 of the canonical repo root. Try nonblocking first; on EWOULDBLOCK print a waiting note and block.

**Files:**
- Create: `src/lockfile.rs`; Modify: `src/lib.rs` (add `pub mod lockfile;`)

**Interfaces:**
- Produces: `lockfile::RepoLock` (RAII guard; drop releases); `lockfile::lock_path(repo_root: &Path) -> PathBuf`; `lockfile::acquire(repo_root: &Path) -> Result<RepoLock, Error>`.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use nix::fcntl::{Flock, FlockArg};
    use std::path::Path;

    #[test]
    fn lock_paths_are_stable_and_distinct() {
        let a1 = lock_path(Path::new("/repo/a"));
        let a2 = lock_path(Path::new("/repo/a"));
        let b = lock_path(Path::new("/repo/b"));
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert!(a1.to_string_lossy().ends_with(".lock"));
    }

    #[test]
    fn acquired_lock_excludes_a_second_flock() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path()); // isolate this test's state dir
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();

        let guard = acquire(&repo).unwrap();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path(&repo))
            .unwrap();
        let second = Flock::lock(file, FlockArg::LockExclusiveNonblock);
        assert!(second.is_err(), "second flock should be excluded");
        drop(guard);

        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path(&repo))
            .unwrap();
        assert!(Flock::lock(file, FlockArg::LockExclusiveNonblock).is_ok());
    }
}
```

Note: `set_var` is process-global; this is the only test in the crate that touches `XDG_STATE_HOME`, and `lock_path` reads it per call, so isolation holds. Do not add other env-mutating tests.

- [ ] **Step 2: Run — expect FAIL** — `cargo test lockfile::`

- [ ] **Step 3: Implement**

```rust
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use sha2::{Digest, Sha256};

use crate::error::Error;

pub struct RepoLock(#[allow(dead_code)] Flock<File>);

pub fn lock_path(repo_root: &Path) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let digest = Sha256::digest(repo_root.as_os_str().as_bytes());
    state_dir().join("locks").join(format!("{digest:x}.lock"))
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("herdr-devcontainer")
}

pub fn acquire(repo_root: &Path) -> Result<RepoLock, Error> {
    let path = lock_path(repo_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let open = || {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
    };
    match Flock::lock(open()?, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(RepoLock(lock)),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => {
            eprintln!("waiting for another bring-up of this repo to finish...");
            Flock::lock(open()?, FlockArg::LockExclusive)
                .map(RepoLock)
                .map_err(|(_, e)| Error::Io(std::io::Error::from(e)))
        }
        Err((_, e)) => Err(Error::Io(std::io::Error::from(e))),
    }
}
```

- [ ] **Step 4: Run — expect PASS** — `cargo test lockfile::`

- [ ] **Step 5: Commit** — `git add src && git commit -m "feat: per-repo flock for bring-up serialization"`

---

### Task 9: Bring-up (`src/up.rs`)

`devcontainer up` invocation and result parsing. The result JSON is the **last non-empty line of stdout** (progress goes to stderr — verified on Dev Containers CLI 0.86). Validation exactly as ProjectMux: `outcome == "success"`, non-empty `containerId` and `remoteWorkspaceFolder`; `remoteUser` optional.

**Files:**
- Create: `src/up.rs`; Modify: `src/lib.rs` (add `pub mod up;`)

**Interfaces:**
- Consumes: `run::{run, StderrMode}`, `error::Error`, `util::tail`.
- Produces:
  - `up::UpResult { container_id: String, remote_user: Option<String>, remote_workspace_folder: String, outcome: String, message: Option<String> }` (serde with camelCase renames)
  - `up::up_argv(devcontainer_bin: &Path, repo_root: &Path, config_arg: Option<&Path>) -> Vec<String>`
  - `up::parse_up_output(stdout: &str) -> Result<UpResult, Error>`
  - `up::bring_up(devcontainer_bin: &Path, repo_root: &Path, config_arg: Option<&Path>, timeout: Duration) -> Result<UpResult, Error>` (stderr inherited → live progress in the pane)

- [ ] **Step 1: Write failing tests**

```rust
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
```

- [ ] **Step 2: Run — expect FAIL** — `cargo test up::`

- [ ] **Step 3: Implement**

```rust
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
```

- [ ] **Step 4: Run — expect PASS** — `cargo test up::`

- [ ] **Step 5: Commit** — `git add src && git commit -m "feat: devcontainer up invocation and last-line JSON parsing"`

---

### Task 10: Workdir mapping (`src/workdir.rs`)

Repo-relative cwd POSIX-joined onto `remoteWorkspaceFolder`. A cwd outside the repo root (worktree checked out as a sibling directory) is not mounted in the container — fall back to the workspace root and flag it.

**Files:**
- Create: `src/workdir.rs`; Modify: `src/lib.rs` (add `pub mod workdir;`)

**Interfaces:**
- Produces: `workdir::Workdir { path: String, outside_repo: bool }`; `workdir::map_workdir(repo_root: &Path, cwd: &Path, remote_root: &str) -> Workdir`. Callers canonicalize both paths first.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn repo_root_maps_to_remote_root() {
        let wd = map_workdir(Path::new("/r"), Path::new("/r"), "/workspaces/p");
        assert_eq!(wd.path, "/workspaces/p");
        assert!(!wd.outside_repo);
    }

    #[test]
    fn subdirectory_is_joined_posix_style() {
        let wd = map_workdir(Path::new("/r"), Path::new("/r/sub/dir"), "/workspaces/p");
        assert_eq!(wd.path, "/workspaces/p/sub/dir");
    }

    #[test]
    fn trailing_slash_on_remote_root_does_not_double() {
        let wd = map_workdir(Path::new("/r"), Path::new("/r/sub"), "/workspaces/p/");
        assert_eq!(wd.path, "/workspaces/p/sub");
    }

    #[test]
    fn cwd_outside_the_repo_falls_back_to_remote_root() {
        let wd = map_workdir(Path::new("/r"), Path::new("/elsewhere/wt"), "/workspaces/p");
        assert_eq!(wd.path, "/workspaces/p");
        assert!(wd.outside_repo);
    }
}
```

- [ ] **Step 2: Run — expect FAIL** — `cargo test workdir::`

- [ ] **Step 3: Implement**

```rust
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub struct Workdir {
    pub path: String,
    pub outside_repo: bool,
}

pub fn map_workdir(repo_root: &Path, cwd: &Path, remote_root: &str) -> Workdir {
    match cwd.strip_prefix(repo_root) {
        Ok(rel) if rel.as_os_str().is_empty() => Workdir {
            path: remote_root.trim_end_matches('/').to_string(),
            outside_repo: false,
        },
        Ok(rel) => {
            let mut path = remote_root.trim_end_matches('/').to_string();
            for comp in rel.components() {
                path.push('/');
                path.push_str(&comp.as_os_str().to_string_lossy());
            }
            Workdir {
                path,
                outside_repo: false,
            }
        }
        Err(_) => Workdir {
            path: remote_root.trim_end_matches('/').to_string(),
            outside_repo: true,
        },
    }
}
```

- [ ] **Step 4: Run — expect PASS** — `cargo test workdir::`

- [ ] **Step 5: Commit** — `git add src && git commit -m "feat: container workdir mapping with outside-repo fallback"`

---

### Task 11: Docker exec (`src/exec.rs`)

Direct argv on the host — hostile paths need no quoting because nothing goes through a host shell. Only the container-side command payload runs via `sh -lc`.

**Files:**
- Create: `src/exec.rs`; Modify: `src/lib.rs` (add `pub mod exec;`)

**Interfaces:**
- Produces:
  - `exec::Payload { Shell, Command(String) }`
  - `exec::exec_argv(container_id: &str, remote_user: Option<&str>, workdir: &str, payload: &Payload) -> Vec<String>`
  - `exec::exec_into(argv: &[String]) -> std::io::Error` (process-replacing `CommandExt::exec`; returns only on failure)

- [ ] **Step 1: Write failing tests**

```rust
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
```

- [ ] **Step 2: Run — expect FAIL** — `cargo test exec::`

- [ ] **Step 3: Implement**

```rust
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
```

- [ ] **Step 4: Run — expect PASS** — `cargo test exec::`

- [ ] **Step 5: Commit** — `git add src && git commit -m "feat: docker exec argv construction and process replacement"`

---

### Task 11b: Container discovery (`src/discover.rs`)

Label-based lookup of the repo's dev containers. Both the pane flow (spec step 4, ambiguity check) and `stop` (Task 13) consume this, so it lands before either — the pane flow cannot refuse an ambiguous repo with a helper that does not exist yet.

Two rules carry over from ProjectMux and the spec's uncertainty-is-never-absence line: more than one *running* match is a hard error, and a `docker ps` line we cannot parse is an error rather than a dropped row. Silently discarding a malformed line turns "I could not tell" into "there is no container", which is the exact inversion the rule forbids.

**Files:**
- Create: `src/discover.rs`; Modify: `src/lib.rs` (add `pub mod discover;`)

**Interfaces:**
- Consumes: `error::Error`, `run::{run, StderrMode}`, `util::tail`.
- Produces:
  - `discover::Container { id: String, name: String, state: String }` (Clone, Debug, PartialEq)
  - `discover::ps_argv(repo_root: &Path) -> Vec<String>`
  - `discover::parse_ps(stdout: &str) -> Result<Vec<Container>, Error>`
  - `discover::select_running(containers: &[Container], repo_root: &Path) -> Result<Option<Container>, Error>`
  - `discover::list(repo_root: &Path) -> Result<Vec<Container>, Error>` (runs docker, 5s timeout)

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn c(id: &str, name: &str, state: &str) -> Container {
        Container {
            id: id.to_string(),
            name: name.to_string(),
            state: state.to_string(),
        }
    }

    #[test]
    fn ps_argv_filters_by_the_devcontainer_label() {
        let argv = ps_argv(Path::new("/r"));
        assert!(argv.contains(&"label=devcontainer.local_folder=/r".to_string()));
        assert!(argv.contains(&"-a".to_string()));
        // The spec's stop output prints id *and* name, so name must be queried.
        assert!(argv.contains(&"{{.ID}}\t{{.Names}}\t{{.State}}".to_string()));
    }

    #[test]
    fn parse_ps_reads_id_name_and_lowercased_state() {
        let parsed = parse_ps("abc123\tfoo_devcontainer\tRunning\ndef456\tbar\texited\n\n").unwrap();
        assert_eq!(
            parsed,
            vec![
                c("abc123", "foo_devcontainer", "running"),
                c("def456", "bar", "exited")
            ]
        );
    }

    // Uncertainty is never absence: a line we cannot read must not silently
    // shrink the result set into "no container".
    #[test]
    fn parse_ps_rejects_malformed_lines() {
        let err = parse_ps("abc123\tonly-two-fields\n").unwrap_err();
        assert!(matches!(err, Error::MalformedDockerOutput { .. }));
        let err = parse_ps("\t\trunning\n").unwrap_err();
        assert!(matches!(err, Error::MalformedDockerOutput { .. }));
    }

    #[test]
    fn parse_ps_accepts_empty_output() {
        assert_eq!(parse_ps("").unwrap(), vec![]);
        assert_eq!(parse_ps("\n\n").unwrap(), vec![]);
    }

    #[test]
    fn select_none_running_returns_none() {
        assert_eq!(
            select_running(&[c("a", "an", "exited")], Path::new("/r")).unwrap(),
            None
        );
    }

    #[test]
    fn select_one_running_returns_it() {
        let got = select_running(
            &[c("a", "an", "exited"), c("b", "bn", "running")],
            Path::new("/r"),
        )
        .unwrap();
        assert_eq!(got, Some(c("b", "bn", "running")));
    }

    #[test]
    fn select_multiple_running_refuses_to_choose() {
        let err = select_running(
            &[c("a", "an", "running"), c("b", "bn", "running")],
            Path::new("/r"),
        )
        .unwrap_err();
        assert!(matches!(err, Error::MultipleRunningContainers { .. }));
    }
}
```

- [ ] **Step 2: Run — expect FAIL** — `cargo test discover::`

- [ ] **Step 3: Implement**

```rust
use std::path::Path;
use std::time::Duration;

use crate::error::Error;
use crate::run::{run, StderrMode};
use crate::util::tail;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub state: String,
}

pub fn ps_argv(repo_root: &Path) -> Vec<String> {
    vec![
        "docker".to_string(),
        "ps".to_string(),
        "-a".to_string(),
        "--filter".to_string(),
        format!("label=devcontainer.local_folder={}", repo_root.display()),
        "--format".to_string(),
        "{{.ID}}\t{{.Names}}\t{{.State}}".to_string(),
    ]
}

pub fn parse_ps(stdout: &str) -> Result<Vec<Container>, Error> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').map(str::trim).collect();
        // An id-less or short row means the format changed under us; refuse to
        // guess rather than dropping the row.
        if fields.len() != 3 || fields[0].is_empty() {
            return Err(Error::MalformedDockerOutput {
                line: line.to_string(),
            });
        }
        out.push(Container {
            id: fields[0].to_string(),
            name: fields[1].to_string(),
            state: fields[2].to_lowercase(),
        });
    }
    Ok(out)
}

pub fn select_running(
    containers: &[Container],
    repo_root: &Path,
) -> Result<Option<Container>, Error> {
    let running: Vec<&Container> = containers.iter().filter(|c| c.state == "running").collect();
    match running.len() {
        0 => Ok(None),
        1 => Ok(Some(running[0].clone())),
        _ => Err(Error::MultipleRunningContainers {
            repo_root: repo_root.display().to_string(),
            ids: running
                .iter()
                .map(|c| format!("{} ({})", c.id, c.name))
                .collect(),
        }),
    }
}

pub fn list(repo_root: &Path) -> Result<Vec<Container>, Error> {
    let res = run(
        &ps_argv(repo_root),
        Duration::from_secs(5),
        StderrMode::Capture,
    )?;
    if res.exit_code != Some(0) {
        return Err(Error::DockerCommandFailed {
            detail: tail(res.stderr.trim(), 500),
        });
    }
    parse_ps(&res.stdout)
}
```

- [ ] **Step 4: Run — expect PASS** — `cargo test discover::`

- [ ] **Step 5: Commit** — `git add src && git commit -m "feat: label-based container discovery with refuse-to-choose semantics"`

---

### Task 12: Pane orchestration + final `main.rs` (`src/pane.rs`, `src/open.rs`)

**Files:**
- Create: `src/pane.rs`, `src/open.rs`; Modify: `src/lib.rs` (add `pub mod pane; pub mod open;`), rewrite `src/main.rs`

**Interfaces:**
- Consumes: everything above, including `discover` (Task 11b).
- Produces: `pane::run_pane(shell: bool) -> Result<(), Error>`; `open::open_argv(herdr_bin: &str, entrypoint: &str) -> Vec<String>`; `open::run_open(entrypoint: Option<&str>) -> Result<(), Error>`.

**Before Step 1 — verify `HERDR_BIN_PATH` on the action path.** `run_open` below is
built on the assumption that herdr injects `HERDR_BIN_PATH` when it spawns an
*action*. That injection is confirmed only for the **pane** spawn path
(`src/app/api/plugins/panes.rs:232-264` in herdr); actions are a different code
path and were never checked. Confirm it before writing `run_open`:

```bash
rg -n 'HERDR_BIN_PATH' ~/workspace/herdr/src
```

If the action spawn path also injects it, proceed as written. If it does not,
resolve herdr another way instead of failing hard — a PATH lookup for `herdr`,
falling back to a configured path — and adjust the test in Step 1 to cover
whichever resolution order you implement. Do not build the `HERDR_BIN_PATH`
branch and discover at runtime that the variable is never set.

- [ ] **Step 1: Write the failing tests**

`run_pane` is composition of already-tested parts, but the ambiguity check added
below is new behavior and gets its own test here; end-to-end coverage of the pane
flow lands in Task 14.

```rust
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
}
```

And in `src/pane.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::Container;
    use std::path::Path;

    fn c(id: &str, state: &str) -> Container {
        Container {
            id: id.to_string(),
            name: format!("{id}_name"),
            state: state.to_string(),
        }
    }

    // Spec wrapper-flow step 4: bring-up must not run when the repo has more
    // than one running dev container — `devcontainer up` would silently pick
    // one, which is exactly the case we refuse.
    #[test]
    fn ambiguous_repos_are_refused_before_bring_up() {
        let err = check_unambiguous(
            &[c("a", "running"), c("b", "running")],
            Path::new("/r"),
        )
        .unwrap_err();
        assert!(matches!(err, Error::MultipleRunningContainers { .. }));
    }

    #[test]
    fn zero_or_one_running_proceeds() {
        assert!(check_unambiguous(&[], Path::new("/r")).is_ok());
        assert!(check_unambiguous(&[c("a", "running"), c("b", "exited")], Path::new("/r")).is_ok());
    }
}
```

- [ ] **Step 2: Run — expect FAIL** — `cargo test open:: pane::`

- [ ] **Step 3: Implement all three files**

`src/open.rs`:

```rust
use crate::error::Error;
use crate::exec;

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

pub fn run_open(entrypoint: Option<&str>) -> Result<(), Error> {
    let entrypoint = entrypoint.unwrap_or("shell");
    let herdr_bin = std::env::var("HERDR_BIN_PATH").map_err(|_| {
        Error::Other("HERDR_BIN_PATH not set (run from a herdr plugin action)".into())
    })?;
    Err(Error::Io(exec::exec_into(&open_argv(
        &herdr_bin, entrypoint,
    ))))
}
```

`src/pane.rs`:

```rust
use std::path::Path;
use std::time::Duration;

use crate::discover::{self, Container};
use crate::error::Error;
use crate::exec::{self, Payload};
use crate::{config, context, detect, lockfile, preflight, up, workdir};

/// Spec wrapper-flow step 4. `select_running` already refuses to choose between
/// several running matches; here we only need its error, not its selection.
pub fn check_unambiguous(containers: &[Container], repo_root: &Path) -> Result<(), Error> {
    discover::select_running(containers, repo_root).map(|_| ())
}

pub fn run_pane(shell: bool) -> Result<(), Error> {
    let cfg = config::load()?;
    for warning in &cfg.warnings {
        eprintln!("config: {warning}");
    }

    let ctx = context::load_context();
    let process_cwd = std::env::current_dir()?;
    let repo_root = context::resolve_repo_root(&ctx, &process_cwd)?;

    let repo_cfg = cfg.repo(&repo_root);
    let detection = detect::detect(&repo_root, &repo_cfg)?;

    let path_var = std::env::var("PATH").unwrap_or_default();
    let devcontainer_bin = preflight::find_devcontainer(&path_var)?;
    preflight::check_docker("docker")?;

    // Before taking the lock or starting anything: refuse an ambiguous repo.
    check_unambiguous(&discover::list(&repo_root)?, &repo_root)?;

    let up_result = {
        let _lock = lockfile::acquire(&repo_root)?;
        up::bring_up(
            &devcontainer_bin,
            &repo_root,
            detection.config_arg.as_deref(),
            Duration::from_secs(cfg.up_timeout_secs),
        )?
    };

    let cwd = process_cwd.canonicalize().unwrap_or(process_cwd);
    let wd = workdir::map_workdir(&repo_root, &cwd, &up_result.remote_workspace_folder);
    if wd.outside_repo {
        eprintln!(
            "note: current directory is not under {}; starting at the container workspace root",
            repo_root.display()
        );
    }

    let payload = if shell {
        Payload::Shell
    } else {
        Payload::Command(cfg.command.clone())
    };
    let argv = exec::exec_argv(
        &up_result.container_id,
        up_result.remote_user.as_deref(),
        &wd.path,
        &payload,
    );
    // exec_into replaces the process; reaching the line below means it failed.
    Err(Error::Io(exec::exec_into(&argv)))
}
```

`src/main.rs` (full rewrite):

```rust
use herdr_devcontainer::error::Error;
use herdr_devcontainer::{open, pane};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result: Result<(), Error> = match args.first().map(String::as_str) {
        Some("pane") => pane::run_pane(args.iter().any(|a| a == "--shell")),
        Some("stop") => {
            eprintln!("stop: not implemented yet");
            std::process::exit(2);
        }
        Some("open") => open::run_open(args.get(1).map(String::as_str)),
        _ => {
            eprintln!("usage: herdr-devc <pane [--shell] | stop | open <entrypoint>>");
            std::process::exit(2);
        }
    };
    if let Err(err) = result {
        eprintln!("error: {err}");
        if let Some(hint) = err.hint() {
            eprintln!("hint: {hint}");
        }
        hold();
        std::process::exit(1);
    }
}

/// Herdr may close the pane the moment its command exits; keep the message
/// on screen until the user acknowledges it.
fn hold() {
    eprintln!("press Enter to close");
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
}
```

- [ ] **Step 4: Run — expect PASS** — `cargo test` (all), then a CLI smoke test:

```bash
cargo build && cd /tmp && HERDR_PLUGIN_CONTEXT_JSON='' ~/workspace/herdr-devcontainer/.worktrees/design/target/debug/herdr-devc pane </dev/null; echo "exit: $?"
```

Expected: `error: not inside a git repository (cwd: /tmp)`, `press Enter to close`, `exit: 1` (stdin EOF releases the hold). Run it from `/tmp` so no enclosing git repo is found.

- [ ] **Step 5: Commit** — `git add src && git commit -m "feat: pane orchestration, open helper, and CLI dispatch"`

---

### Task 13: Stop (`src/stop.rs`)

The lookup and refuse-to-choose logic live in `discover` (Task 11b); this task is only the stop flow on top of them. The spec requires stop's output to identify the container the user is about to lose, so both id and name are printed — `discover::Container` already carries both.

**Files:**
- Create: `src/stop.rs`; Modify: `src/lib.rs` (add `pub mod stop;`), `src/main.rs` (replace the `stop` stub arm)

**Interfaces:**
- Consumes: `discover::{list, select_running, Container}`, `context`, `preflight`, `run`, `util::tail`.
- Produces: `stop::stop_argv(id: &str) -> Vec<String>`; `stop::run_stop() -> Result<(), Error>`.

- [ ] **Step 1: Write failing tests**

Discovery is covered by Task 11b; what is new here is the stop command itself and the fact that the printed line names the container.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_argv_targets_the_id() {
        assert_eq!(stop_argv("abc123"), vec!["docker", "stop", "abc123"]);
    }

    #[test]
    fn the_stop_message_names_both_id_and_name() {
        let c = crate::discover::Container {
            id: "abc123".to_string(),
            name: "herdr_devcontainer".to_string(),
            state: "running".to_string(),
        };
        let msg = stopping_message(&c, std::path::Path::new("/r"));
        assert!(msg.contains("abc123"), "{msg}");
        assert!(msg.contains("herdr_devcontainer"), "{msg}");
        assert!(msg.contains("/r"), "{msg}");
    }
}
```

- [ ] **Step 2: Run — expect FAIL** — `cargo test stop::`

- [ ] **Step 3: Implement**

```rust
use std::path::Path;
use std::time::Duration;

use crate::discover::{self, Container};
use crate::error::Error;
use crate::run::{run, StderrMode};
use crate::util::tail;
use crate::{context, preflight};

pub fn stop_argv(id: &str) -> Vec<String> {
    vec!["docker".to_string(), "stop".to_string(), id.to_string()]
}

pub fn stopping_message(c: &Container, repo_root: &Path) -> String {
    format!(
        "stopping container {} ({}) for {}",
        c.id,
        c.name,
        repo_root.display()
    )
}

pub fn run_stop() -> Result<(), Error> {
    let ctx = context::load_context();
    let process_cwd = std::env::current_dir()?;
    let repo_root = context::resolve_repo_root(&ctx, &process_cwd)?;
    preflight::check_docker("docker")?;

    let containers = discover::list(&repo_root)?;
    match discover::select_running(&containers, &repo_root)? {
        None => {
            println!("no running dev container for {}", repo_root.display());
            Ok(())
        }
        Some(c) => {
            println!("{}", stopping_message(&c, &repo_root));
            // docker's SIGTERM grace is 10s before SIGKILL; give the CLI 30s.
            let res = run(&stop_argv(&c.id), Duration::from_secs(30), StderrMode::Capture)?;
            let already_gone = res.stderr.to_lowercase().contains("no such container");
            if res.exit_code == Some(0) || already_gone {
                println!("stopped.");
                Ok(())
            } else {
                Err(Error::DockerCommandFailed {
                    detail: tail(res.stderr.trim(), 500),
                })
            }
        }
    }
}
```

Replace the `stop` arm in `src/main.rs`:

```rust
        Some("stop") => {
            let result = herdr_devcontainer::stop::run_stop();
            if result.is_ok() {
                hold();
            }
            result
        }
```

(Adjust the `use` list at the top of `main.rs` accordingly — the error arm already holds on failure, so `stop` holds only on success here to avoid a double hold.)

- [ ] **Step 4: Run — expect PASS** — `cargo test` (all modules)

- [ ] **Step 5: Commit** — `git add src && git commit -m "feat: explicit container stop with refuse-to-choose semantics"`

---

### Task 14: Plugin manifest, README, integration test

**Files:**
- Create: `herdr-plugin.toml`, `README.md`, `tests/integration.rs`

**Interfaces:**
- Consumes: the built binary at `target/release/herdr-devc`; lib functions `preflight::*`, `up::bring_up`, `exec::exec_argv`.

- [ ] **Step 1: Verify manifest syntax against the real sources.** The block in Step 2 is the structure from the spec; before committing, fetch and compare field names against BOTH:
  - the plugin docs: https://herdr.dev/docs/plugins/ (WebFetch)
  - a real manifest: https://raw.githubusercontent.com/cloudmanic/herdr-plus/master/herdr-plugin.toml

  Check specifically: top-level required fields (`id`, `name`, `version`, `min_herdr_version`), whether `[[build]]`/`[[panes]]`/`[[actions]]` use these exact key names (`id`, `title`, `placement`, `command`, `platforms`, `contexts`), and whether pane/action `command` paths are resolved relative to the plugin root (herdr-plus references its build artifact this way — confirm). Fix the manifest to match reality; reality wins over this plan.

- [ ] **Step 2: Write `herdr-plugin.toml`**

```toml
id = "devcontainer"
name = "Dev Container"
version = "0.1.0"
min_herdr_version = "0.8.0"

[[build]]
command = ["cargo", "build", "--release"]

[[panes]]
id = "shell"
title = "Container Shell"
platforms = ["linux"]
placement = "split"
command = ["target/release/herdr-devc", "pane", "--shell"]

[[panes]]
id = "command"
title = "Container Agent"
platforms = ["linux"]
placement = "split"
command = ["target/release/herdr-devc", "pane"]

[[panes]]
id = "stop"
title = "Stop Dev Container"
platforms = ["linux"]
placement = "popup"
command = ["target/release/herdr-devc", "stop"]

[[actions]]
id = "open-shell"
title = "Open Dev Container Shell"
command = ["target/release/herdr-devc", "open", "shell"]

[[actions]]
id = "open-command"
title = "Open Dev Container Agent"
command = ["target/release/herdr-devc", "open", "command"]

[[actions]]
id = "open-stop"
title = "Stop Dev Container"
command = ["target/release/herdr-devc", "open", "stop"]
```

- [ ] **Step 3: Write `tests/integration.rs`** (requires real docker + devcontainer CLI; excluded from normal runs)

Two tests, because they cover different things. The first exercises the library
functions directly and proves bring-up and exec work against a real container.
The second drives the **built binary** end to end, which is the only test that
covers `main.rs` dispatch, context resolution, config load, the ambiguity check,
and the wiring between them — the composition Task 12 declines to unit-test. A
library-level test does not stand in for it: every one of those steps is skipped
when you call `up::bring_up` yourself.

```rust
//! End-to-end bring-up against a throwaway fixture repo.
//! Requires docker and the Dev Containers CLI. Run explicitly:
//!     cargo test --test integration -- --ignored

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use herdr_devcontainer::{exec, preflight, up};

fn sh_ok(dir: &Path, cmd: &str, args: &[&str]) {
    let status = Command::new(cmd)
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "{cmd} {args:?} failed");
}

/// Build a throwaway git repo with a minimal devcontainer config.
fn fixture_repo(tmp: &Path) -> std::path::PathBuf {
    let repo = tmp.join("fixture");
    std::fs::create_dir_all(repo.join(".devcontainer")).unwrap();
    std::fs::write(
        repo.join(".devcontainer/devcontainer.json"),
        r#"{"image": "mcr.microsoft.com/devcontainers/base:alpine"}"#,
    )
    .unwrap();
    sh_ok(&repo, "git", &["init", "-b", "main"]);
    repo
}

#[test]
#[ignore = "requires docker and @devcontainers/cli"]
fn bring_up_exec_and_stop_roundtrip() {
    let path_var = std::env::var("PATH").unwrap();
    let devcontainer_bin = preflight::find_devcontainer(&path_var).expect("devcontainer CLI");
    preflight::check_docker("docker").expect("docker daemon");

    let tmp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(tmp.path());

    let result = up::bring_up(&devcontainer_bin, &repo, None, Duration::from_secs(300))
        .expect("devcontainer up");
    assert!(!result.container_id.is_empty());
    assert!(result.remote_workspace_folder.starts_with('/'));

    // A non-interactive exec proves the container answers.
    let argv = exec::exec_argv(
        &result.container_id,
        result.remote_user.as_deref(),
        &result.remote_workspace_folder,
        &exec::Payload::Command("echo alive".into()),
    );
    // Drop -t for a non-tty test environment.
    let argv: Vec<&str> = argv
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "-t")
        .collect();
    let out = Command::new(argv[0]).args(&argv[1..]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("alive"));

    sh_ok(&repo, "docker", &["stop", &result.container_id]);
}

/// The whole pane path through the real binary: dispatch, context resolution,
/// config load, ambiguity check, bring-up, workdir mapping, exec. Nothing below
/// the binary boundary is stubbed.
#[test]
#[ignore = "requires docker and @devcontainers/cli"]
fn the_binary_brings_up_and_execs_from_a_pane_invocation() {
    preflight::check_docker("docker").expect("docker daemon");

    let tmp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(tmp.path());
    let bin = env!("CARGO_BIN_EXE_herdr-devc");

    // No --shell: the pane runs the configured command payload, so we point it
    // at something that terminates and prints a marker.
    let cfg_dir = tmp.path().join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("devcontainer.toml"), "command = \"echo alive\"\n").unwrap();

    let ctx = format!(r#"{{"repo_root": "{}"}}"#, repo.display());
    let out = Command::new(bin)
        .arg("pane")
        .current_dir(&repo)
        .env("HERDR_PLUGIN_CONTEXT_JSON", &ctx)
        .env("XDG_CONFIG_HOME", &cfg_dir)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("alive"),
        "binary did not reach the container payload\nstdout: {stdout}\nstderr: {stderr}"
    );

    // And `stop` finds it by label and reports the container it killed.
    let out = Command::new(bin)
        .arg("stop")
        .current_dir(&repo)
        .env("HERDR_PLUGIN_CONTEXT_JSON", &ctx)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("stopping container"), "stdout: {stdout}");
    assert!(stdout.contains("stopped."), "stdout: {stdout}");
}
```

When you write this, confirm the `XDG_CONFIG_HOME` path the binary actually
reads (Task 5's `config_path()`) and the exact context key `resolve_repo_root`
consumes (Task 4) — transcribe them rather than trusting the sketch above. If
the config layout does not key off `XDG_CONFIG_HOME`, drop that env override and
let the default `command` run instead, adjusting the marker assertion.

- [ ] **Step 4: Write `README.md`** — cover: what the plugin does (one paragraph); requirements (herdr ≥ 0.8.0, docker, `npm install -g @devcontainers/cli`, Rust toolchain for the build hook); install (`herdr plugin install <path-or-url>` — transcribe the exact command from the herdr CLI reference while you have it fetched in Step 1); the three entrypoints and how to keybind them (`[[keys.command]]` with `type = "plugin_action"` and the qualified action id); the config file with the example from the spec; a "how it works" section pointing at the spec; the security note (devcontainer up executes repo-controlled build hooks — same trust model as VS Code Dev Containers; the plugin never takes its payload command from repo content).

- [ ] **Step 5: Verify** — `cargo test` (both integration tests must be listed as ignored, not run); `cargo build --release` succeeds; if docker + devcontainer CLI are available on this machine, run `cargo test --test integration -- --ignored` and fix what fails (this is the single most valuable verification in the whole plan — the fixture pull may take a few minutes on first run).

  Note what these tests do **not** cover: `herdr-plugin.toml` itself is never
  parsed by anything in this repo, so no test can tell you the manifest is
  valid. That is verified only by installing the plugin into a real herdr and
  running the entrypoints — Task 15 Step 2. Do not report the manifest as
  verified on the strength of a green `cargo test`.

- [ ] **Step 6: Commit** — `git add herdr-plugin.toml README.md tests && git commit -m "feat: plugin manifest, README, and ignored integration test"`

---

### Task 15: Final verification and manual checklist

- [ ] **Step 1: Full gate** — run and require clean: `cargo fmt --check`, `cargo clippy --all-targets` (no warnings in our code), `cargo test`, `cargo build --release`.

- [ ] **Step 2: Manual verification in herdr** (requires a herdr ≥ 0.8.0 install; if unavailable, record that this step was skipped and why in the final report — do not claim it passed):
  1. Install the plugin from the repo directory (command from the CLI reference, per Task 14 Step 4).
  2. In a herdr workspace on a repo **with** a devcontainer: invoke the `shell` pane → expect a split pane, live bring-up progress, then a container login shell in the mapped workdir (`pwd` shows the remote workspace folder path).
  3. Invoke it again → the existing container is reused (fast, no rebuild).
  4. In a repo **without** a devcontainer: expect the classified "no devcontainer config" error, held until Enter.
  5. Invoke `stop` → popup reports the container and stops it; a second `stop` reports none running.
  6. Keybind test: `[[keys.command]]` with `type = "plugin_action"` and the qualified action id opens the shell pane.

- [ ] **Step 3: Update the spec's "Risks / open items"** — resolve the two verify-at-implementation items (split close-on-exit behavior as observed; final manifest syntax) with what you actually found, and commit.

- [ ] **Step 4: Commit any fixes** — `git add -A && git commit -m "chore: final verification fixes"`
