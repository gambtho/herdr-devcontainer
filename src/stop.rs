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

pub fn confirm_prompt(c: &Container, repo_root: &Path) -> String {
    format!(
        "stop container {} ({}) for {}? [y/N]: ",
        c.id,
        c.name,
        repo_root.display()
    )
}

/// Only an explicit yes proceeds. A bare Enter, an unrecognized answer, and an
/// unreadable one all cancel: stopping discards a running container's state
/// with no undo, and the entrypoint is a single keystroke away from herdr's
/// built-in bindings, so a mis-key must not be able to spend that.
pub fn confirmed(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Reads a single line. `read_line` (not `read_to_string`) so an interactive
/// pane returns on Enter rather than blocking until the user closes stdin.
fn read_answer(reader: &mut impl std::io::BufRead) -> Result<String, Error> {
    let mut buf = String::new();
    // EOF leaves `buf` empty, which `confirmed` reads as a cancel.
    reader.read_line(&mut buf)?;
    Ok(buf)
}

/// The config paths to look a container up by, honouring a repo-configured
/// `config` when it resolves.
///
/// Best-effort by design: this list is a *search*, not an authorization
/// decision, so a `config` value that cannot be resolved falls back to the
/// standard locations instead of failing the stop. Refusing here would take
/// away the `local_folder` lookup too — turning a bad config line into "no
/// container found" for a container that is plainly running.
fn discovery_config_files(repo_root: &Path) -> Vec<std::path::PathBuf> {
    let configured = crate::config::load()
        .ok()
        .and_then(|cfg| cfg.repo(repo_root).config)
        .and_then(|rel| crate::config::resolve_repo_relative(repo_root, &rel).ok());
    crate::detect::config_candidates(repo_root, configured.as_deref())
}

pub fn run_stop() -> Result<(), Error> {
    let ctx = context::load_context();
    let process_cwd = std::env::current_dir()?;
    let repo_root = context::resolve_repo_root(&ctx, &process_cwd)?;
    preflight::check_docker("docker")?;

    let containers = discover::list(&repo_root, &discovery_config_files(&repo_root))?;
    match discover::select_running(&containers, &repo_root)? {
        None => {
            println!("no running dev container for {}", repo_root.display());
            Ok(())
        }
        Some(c) => {
            print!("{}", confirm_prompt(&c, &repo_root));
            std::io::Write::flush(&mut std::io::stdout())?;
            let answer = read_answer(&mut std::io::stdin().lock())?;
            if !confirmed(&answer) {
                println!("cancelled; container left running.");
                return Ok(());
            }
            println!("{}", stopping_message(&c, &repo_root));
            // docker's SIGTERM grace is 10s before SIGKILL; give the CLI 30s.
            let res = run(
                &stop_argv(&c.id),
                Duration::from_secs(30),
                StderrMode::Capture,
            )?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_argv_targets_the_id() {
        assert_eq!(stop_argv("abc123"), vec!["docker", "stop", "abc123"]);
    }

    // Stopping is destructive and one mis-keyed binding away: `prefix+shift+s`
    // sits next to herdr's built-in `prefix+shift+d` (close_workspace), and a
    // mistaken keypress is indistinguishable from an intended one. Only an
    // explicit yes proceeds; everything else — including a bare Enter — cancels.
    #[test]
    fn only_an_explicit_yes_confirms() {
        for yes in ["y", "Y", "yes", "YES", "  y\n", "Yes\r\n"] {
            assert!(confirmed(yes), "{yes:?} should confirm");
        }
        for no in ["", "\n", "n", "N", "no", "stop", "yep", "q"] {
            assert!(!confirmed(no), "{no:?} should cancel");
        }
    }

    // Uncertainty is never absence: with no readable answer (closed stdin, a
    // non-interactive invocation) we have not been told to proceed, so we don't.
    #[test]
    fn an_unreadable_answer_is_a_cancel() {
        let answer = read_answer(&mut std::io::empty()).unwrap();
        assert!(!confirmed(&answer));
    }

    #[test]
    fn the_confirm_prompt_names_what_will_be_stopped() {
        let c = crate::discover::Container {
            id: "abc123".to_string(),
            name: "herdr_devcontainer".to_string(),
            state: "running".to_string(),
        };
        let p = confirm_prompt(&c, std::path::Path::new("/r"));
        assert!(p.contains("abc123"), "{p}");
        assert!(p.contains("herdr_devcontainer"), "{p}");
        assert!(p.contains("/r"), "{p}");
        assert!(p.contains("[y/N]"), "{p}");
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
