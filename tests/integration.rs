//! End-to-end bring-up against a throwaway fixture repo.
//! Requires docker and the Dev Containers CLI. Run explicitly:
//!     cargo test --test integration -- --ignored

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use herdr_devcontainer::{detect, discover, exec, preflight, shell, up};

fn sh_ok(dir: &Path, cmd: &str, args: &[&str]) {
    let status = Command::new(cmd)
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "{cmd} {args:?} failed");
}

/// Stops whatever the Dev Containers CLI labelled for this fixture repo, so a
/// failed assertion unwinds without leaving a container running on the host.
struct StopFixtureContainer {
    repo: std::path::PathBuf,
}

impl Drop for StopFixtureContainer {
    fn drop(&mut self) {
        // The label carries the canonical root, the same form the plugin uses.
        let root = self
            .repo
            .canonicalize()
            .unwrap_or_else(|_| self.repo.clone());
        let Ok(out) = Command::new("docker")
            .args(["ps", "-q", "--filter"])
            .arg(format!(
                "label=devcontainer.local_folder={}",
                root.display()
            ))
            .output()
        else {
            return;
        };
        for id in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            let _ = Command::new("docker").args(["stop", id]).status();
        }
    }
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

/// A container labelled the way VS Code on Windows labels one: `local_folder`
/// holds the host's UNC rendering of the WSL path, which no POSIX repo root can
/// equal, while `config_file` holds the POSIX path the CLI resolved inside WSL.
/// Discovery keyed only on `local_folder` reports "no container" for a
/// container that is running in front of the user — the reported regression.
#[test]
#[ignore = "requires docker"]
fn a_container_labelled_by_vs_code_on_windows_is_still_found() {
    preflight::check_docker("docker").expect("docker daemon");

    let tmp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(tmp.path());
    let repo = repo.canonicalize().unwrap();
    let config_file = repo.join(".devcontainer/devcontainer.json");

    let out = Command::new("docker")
        .args(["run", "-d", "--rm", "--label"])
        // Deliberately unrelated to `repo`: the whole point is that this value
        // cannot be derived from the POSIX root we compute.
        .arg("devcontainer.local_folder=\\\\wsl.localhost\\Ubuntu\\home\\u\\elsewhere")
        .arg("--label")
        .arg(format!(
            "devcontainer.config_file={}",
            config_file.display()
        ))
        .args(["alpine:3.20", "sleep", "300"])
        .output()
        .unwrap();
    assert!(out.status.success(), "docker run failed");
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    struct Rm(String);
    impl Drop for Rm {
        fn drop(&mut self) {
            let _ = Command::new("docker").args(["rm", "-f", &self.0]).status();
        }
    }
    let _cleanup = Rm(id.clone());

    // Guards this test's own honesty: with no config candidates, discovery is
    // exactly what it was before the fix, and must find nothing. If this ever
    // starts finding the container, the test below stopped proving anything.
    let by_folder_only = discover::list(&repo, &[]).expect("docker ps");
    assert!(
        by_folder_only.is_empty(),
        "local_folder alone should not match a UNC-labelled container, got {by_folder_only:?}"
    );

    let candidates = detect::config_candidates(&repo, None);
    let found = discover::list(&repo, &candidates).expect("docker ps");
    let running = discover::select_running(&found, &repo).expect("unambiguous");
    let running = running.expect("the running container must be discoverable");
    assert!(
        id.starts_with(&running.id),
        "found {} but the container is {id}",
        running.id
    );
}

#[test]
#[ignore = "requires docker and @devcontainers/cli"]
fn bring_up_exec_and_stop_roundtrip() {
    let path_var = std::env::var("PATH").unwrap();
    let devcontainer_bin = preflight::find_devcontainer(&path_var).expect("devcontainer CLI");
    preflight::check_docker("docker").expect("docker daemon");

    let tmp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(tmp.path());
    let _cleanup = StopFixtureContainer { repo: repo.clone() };

    let result = up::bring_up(&devcontainer_bin, &repo, None, Duration::from_secs(300))
        .expect("devcontainer up");
    assert!(!result.container_id.is_empty());
    assert!(result.remote_workspace_folder.starts_with('/'));

    // The probe must name a real shell for this image — `devcontainers/base`
    // gives its user /bin/bash — so a `None` here is a regression in the probe,
    // not a property of the fixture.
    let exec_shell = shell::probe(&result.container_id, result.remote_user.as_deref())
        .expect("probe names the container user's login shell");
    assert!(exec_shell.starts_with('/'), "{exec_shell} is not a path");
    let payload = exec::Payload::Command("echo alive $ALIVE_MARKER".into());
    let argv = exec::exec_argv(&exec::ExecSpec {
        container_id: &result.container_id,
        remote_user: result.remote_user.as_deref(),
        workdir: &result.remote_workspace_folder,
        shell: &exec_shell,
        env: &["ALIVE_MARKER=yes".to_string()],
        payload: &payload,
    });
    // Drop -t for a non-tty test environment.
    let argv: Vec<&str> = argv
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "-t")
        .collect();
    let out = Command::new(argv[0]).args(&argv[1..]).output().unwrap();
    // "alive yes" rather than "alive": the marker proves `-e` reached the
    // container, which is the only way repo-configured env gets in.
    assert!(String::from_utf8_lossy(&out.stdout).contains("alive yes"));

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
    let _cleanup = StopFixtureContainer { repo: repo.clone() };
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

    // And `stop` finds it by label and reports the container it killed. Stopping
    // is confirmed, and only an explicit yes proceeds — a plain `output()` call
    // hands the child no stdin at all, which the product correctly reads as a
    // cancel. So drive it through a pipe: answer `y`, then close stdin so the
    // "press Enter to close" hold that follows a successful stop sees EOF and
    // returns instead of blocking the test forever.
    let mut child = Command::new(bin)
        .arg("stop")
        .current_dir(&repo)
        .env("HERDR_PLUGIN_CONTEXT_JSON", &ctx)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"y\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("stopping container"), "stdout: {stdout}");
    assert!(stdout.contains("stopped."), "stdout: {stdout}");
}
