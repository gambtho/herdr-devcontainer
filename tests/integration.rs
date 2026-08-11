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
#[ignore = "requires docker, @devcontainers/cli, and script(1)"]
fn the_binary_brings_up_and_execs_from_a_pane_invocation() {
    preflight::check_docker("docker").expect("docker daemon");

    let tmp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(tmp.path());
    let bin = env!("CARGO_BIN_EXE_herdr-devc");

    // No --shell: the pane runs the configured command payload, so we point it
    // at something that terminates and prints a marker. The path below is the
    // one `config::config_path()` reads: $XDG_CONFIG_HOME/herdr-devcontainer/config.toml
    let cfg_dir = tmp.path().join("cfg");
    std::fs::create_dir_all(cfg_dir.join("herdr-devcontainer")).unwrap();
    std::fs::write(
        cfg_dir.join("herdr-devcontainer/config.toml"),
        "command = \"echo alive\"\n",
    )
    .unwrap();

    // `resolve_repo_root` reads the repo root from the nested worktree object.
    let ctx = format!(r#"{{"worktree":{{"repo_root":"{}"}}}}"#, repo.display());
    // The pane path always execs `docker exec -i -t`, and docker refuses `-t`
    // when stdin is not a terminal — which it never is under a test harness.
    // A herdr pane always has one, so allocate a pty here rather than making
    // the product's argv depend on the caller's tty.
    let out = Command::new("script")
        .args(["-qec", &format!("{bin} pane"), "/dev/null"])
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
