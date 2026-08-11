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
