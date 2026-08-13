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
/// `config`.
///
/// A `config` that will not resolve is an error rather than a fallback to the
/// standard locations. It named the only value a container's `config_file`
/// label could hold, so silently searching narrower would end in "no running
/// dev container" for a container that is plainly running — uncertainty
/// reported as absence. Erroring here matches what the README already promises
/// about an unreadable config, and what `detect` does on the pane path.
fn discovery_config_files(
    repo_root: &Path,
    rc: &crate::config::RepoConfig,
) -> Result<Vec<std::path::PathBuf>, Error> {
    let configured = crate::detect::resolve_config_arg(repo_root, rc)?;
    Ok(crate::detect::config_candidates(
        repo_root,
        configured.as_deref(),
    ))
}

pub fn run_stop() -> Result<(), Error> {
    let ctx = context::load_context();
    let process_cwd = std::env::current_dir()?;
    let repo_root = context::resolve_repo_root(&ctx, &process_cwd)?;
    preflight::check_docker("docker")?;

    // Not `detect::detect`: a repo with `enabled = "false"` still has a
    // container worth stopping if one is somehow running, and refusing here
    // would strand it.
    let cfg = crate::config::load()?;
    for warning in &cfg.warnings {
        eprintln!("config: {warning}");
    }
    let config_files = discovery_config_files(&repo_root, &cfg.repo(&repo_root))?;

    let containers = discover::list(&repo_root, &config_files)?;
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

    // A `config` we cannot resolve must not degrade into a narrower search. If
    // it named the only path a container's `config_file` label could hold,
    // quietly dropping it and reporting "no running dev container" would invert
    // uncertainty into absence — the one thing this codebase refuses to do. The
    // README says the same about config generally: an unreadable one is an
    // error, not a silent fallback.
    #[test]
    fn an_unresolvable_custom_config_is_an_error_not_a_narrower_search() {
        let rc = crate::config::RepoConfig {
            config: Some("../escape/devc.json".to_string()),
            ..crate::config::RepoConfig::default()
        };
        let err = discovery_config_files(std::path::Path::new("/r"), &rc).unwrap_err();
        assert!(matches!(err, Error::InvalidConfigPath { .. }));
    }

    #[test]
    fn a_resolvable_custom_config_is_the_only_lookup_path() {
        let rc = crate::config::RepoConfig {
            config: Some("alt/devc.json".to_string()),
            ..crate::config::RepoConfig::default()
        };
        let got = discovery_config_files(std::path::Path::new("/r"), &rc).unwrap();
        assert_eq!(got, vec![std::path::PathBuf::from("/r/alt/devc.json")]);
    }

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
