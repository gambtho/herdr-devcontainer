use std::path::Path;
use std::time::Duration;

use crate::discover::{self, Container};
use crate::error::Error;
use crate::run::{run, StderrMode};
use crate::util::tail;
use crate::{compose, context, preflight};

pub fn stop_argv(ids: &[String]) -> Vec<String> {
    let mut argv = vec!["docker".to_string(), "stop".to_string()];
    argv.extend(ids.iter().cloned());
    argv
}

/// Decide whether the stop actually stopped everything.
///
/// `docker stop` echoes each container it stopped, so anything absent from
/// stdout did not stop. A container docker reports as gone counts as stopped —
/// that is the state the user asked for. Anything else is named in the error:
/// with several containers in play, a partial stop that printed "stopped."
/// would send the user away believing a database is down when it is running.
fn classify_stop(requested: &[String], res: &crate::run::RunResult) -> Result<(), Error> {
    let stopped: std::collections::HashSet<&str> = res
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let stderr_lower = res.stderr.to_lowercase();
    let still_running: Vec<String> = requested
        .iter()
        .filter(|id| !stopped.contains(id.as_str()))
        .filter(|id| {
            // "No such container: <id>" means it is already gone, not stuck.
            !stderr_lower
                .lines()
                .any(|l| l.contains("no such container") && l.contains(&id.to_lowercase()))
        })
        .cloned()
        .collect();
    if still_running.is_empty() {
        return Ok(());
    }
    Err(Error::ContainersNotStopped {
        ids: still_running,
        detail: tail(res.stderr.trim(), 500),
    })
}

/// What is being stopped, printed after the user commits.
///
/// Names every container for the same reason the prompt does: this line is what
/// stays on screen in the pane afterwards, and it is the record of what the
/// keystroke actually spent.
pub fn stopping_message(targets: &[compose::Member], repo_root: &Path) -> String {
    if let [only] = targets {
        return format!(
            "stopping container {} ({}) for {}",
            only.id,
            only.name,
            repo_root.display()
        );
    }
    let names: Vec<&str> = targets.iter().map(|m| m.name.as_str()).collect();
    format!(
        "stopping {} containers for {}: {}",
        targets.len(),
        repo_root.display(),
        names.join(", ")
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

/// The confirmation for everything that is about to stop.
///
/// Every container is named, not counted. This prompt is the only thing between
/// a mis-keyed binding and a stopped database, so the user has to be able to see
/// that the database is in the list — "stop 6 containers?" does not tell them
/// what they are spending.
///
/// One member renders exactly as it always did: nothing about a plain
/// single-container repo changed, so nothing about its prompt should.
pub fn project_confirm_prompt(members: &[compose::Member], repo_root: &Path) -> String {
    if let [only] = members {
        let c = Container {
            id: only.id.clone(),
            name: only.name.clone(),
            state: String::new(),
        };
        return confirm_prompt(&c, repo_root);
    }
    let service_width = members
        .iter()
        .map(|m| m.service.len())
        .max()
        .unwrap_or_default();
    // The name column is padded too, so the ids form a column the eye can scan
    // rather than a ragged edge. This prompt is the safety mechanism; it should
    // be easy to read under a mis-keystroke's worth of attention.
    let name_width = members
        .iter()
        .map(|m| m.name.len())
        .max()
        .unwrap_or_default();
    let mut out = format!(
        "stop {} containers for {}?\n",
        members.len(),
        repo_root.display()
    );
    for m in members {
        out.push_str(&format!(
            "  {:service_width$}  {:name_width$}  {}\n",
            m.service,
            m.name,
            m.id,
            service_width = service_width,
            name_width = name_width
        ));
    }
    // The question goes last so the answer is typed against it.
    out.push_str("[y/N]: ");
    out
}

/// What a cancel left behind. Counted, because "container left running" after
/// declining a six-container stop reads as though the other five went down.
fn cancel_message(count: usize) -> String {
    if count == 1 {
        return "cancelled; container left running.".to_string();
    }
    format!("cancelled; {count} containers left running.")
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
            // Everything that goes down together, so the confirmation can name
            // it all before the user commits to it.
            let targets = compose::stop_set(&c.id, &c.name)?;
            print!("{}", project_confirm_prompt(&targets, &repo_root));
            std::io::Write::flush(&mut std::io::stdout())?;
            let answer = read_answer(&mut std::io::stdin().lock())?;
            if !confirmed(&answer) {
                println!("{}", cancel_message(targets.len()));
                return Ok(());
            }
            println!("{}", stopping_message(&targets, &repo_root));
            let ids: Vec<String> = targets.iter().map(|m| m.id.clone()).collect();
            // docker's SIGTERM grace is 10s per container before SIGKILL, and
            // they are stopped in sequence, so the budget scales with the count
            // rather than assuming one.
            let budget = Duration::from_secs(30 * ids.len() as u64);
            let res = run(&stop_argv(&ids), budget, StderrMode::Capture)?;
            if res.timed_out {
                return Err(Error::DockerCommandFailed {
                    detail: format!("docker stop timed out after {}s", budget.as_secs()),
                });
            }
            classify_stop(&ids, &res)?;
            println!("stopped.");
            Ok(())
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

    // A single-container repo is unchanged by compose support, so its prompt
    // must be too — no count, no list, no new noise where nothing differs.
    #[test]
    fn one_container_keeps_the_original_prompt() {
        let c = crate::discover::Container {
            id: "abc123".to_string(),
            name: "herdr_devcontainer".to_string(),
            state: "running".to_string(),
        };
        let members = vec![crate::compose::Member {
            id: "abc123".to_string(),
            name: "herdr_devcontainer".to_string(),
            service: String::new(),
        }];
        assert_eq!(
            project_confirm_prompt(&members, std::path::Path::new("/r")),
            confirm_prompt(&c, std::path::Path::new("/r"))
        );
    }

    // The confirm is the only thing between a mis-keyed binding and six stopped
    // containers, so it names every one of them rather than a count. A user who
    // sees "postgres" listed can still say no.
    #[test]
    fn a_compose_project_prompt_names_every_container() {
        let members = vec![
            crate::compose::Member {
                id: "dc08b7aeca6f".to_string(),
                name: "dh_devcontainer-app-1".to_string(),
                service: "app".to_string(),
            },
            crate::compose::Member {
                id: "6cc33c601af0".to_string(),
                name: "dh_devcontainer-postgres-1".to_string(),
                service: "postgres".to_string(),
            },
        ];
        let p = project_confirm_prompt(&members, std::path::Path::new("/r"));
        assert!(p.contains("stop 2 containers"), "{p}");
        assert!(p.contains("/r"), "{p}");
        assert!(p.contains("[y/N]"), "{p}");
        for m in &members {
            assert!(p.contains(&m.id), "{p} is missing {}", m.id);
            assert!(p.contains(&m.name), "{p} is missing {}", m.name);
            assert!(p.contains(&m.service), "{p} is missing {}", m.service);
        }
        // The prompt must be the last thing on screen, so the answer is typed
        // against it rather than against a list line.
        assert!(p.trim_end_matches(' ').ends_with("[y/N]:"), "{p}");
    }

    // Cancelling a six-container stop that says "container left running" reads
    // as though five of them went down anyway.
    #[test]
    fn the_cancel_message_matches_how_many_were_at_stake() {
        assert_eq!(cancel_message(1), "cancelled; container left running.");
        assert_eq!(cancel_message(6), "cancelled; 6 containers left running.");
    }

    #[test]
    fn stop_argv_takes_every_id_in_order() {
        assert_eq!(
            stop_argv(&["a".to_string(), "b".to_string()]),
            vec!["docker", "stop", "a", "b"]
        );
    }

    fn res(code: i32, stdout: &str, stderr: &str) -> crate::run::RunResult {
        crate::run::RunResult {
            exit_code: Some(code),
            stdout: stdout.to_string(),
            stdout_incomplete: false,
            stderr: stderr.to_string(),
            timed_out: false,
        }
    }

    #[test]
    fn every_container_stopping_is_a_success() {
        let ids = ["a".to_string(), "b".to_string()];
        assert!(classify_stop(&ids, &res(0, "a\nb\n", "")).is_ok());
    }

    // A container that is already gone is the outcome the user wanted, so it
    // does not become an error just because docker had nothing to do.
    #[test]
    fn an_already_gone_container_is_not_a_failure() {
        let ids = ["a".to_string(), "b".to_string()];
        let r = res(
            1,
            "a\n",
            "Error response from daemon: No such container: b\n",
        );
        assert!(classify_stop(&ids, &r).is_ok());
    }

    // The failure that matters: some stopped, one did not, and the user is
    // walking away believing the project is down. The error has to name the
    // container still running — reporting "stopped." here would be the same
    // lie-by-omission the discovery fix was about.
    #[test]
    fn a_container_that_would_not_stop_is_named() {
        let ids = ["a".to_string(), "b".to_string(), "c".to_string()];
        let r = res(
            1,
            "a\nc\n",
            "Error response from daemon: cannot stop container b: permission denied\n",
        );
        let err = classify_stop(&ids, &r).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains('b'),
            "{msg} must name the container left running"
        );
        assert!(msg.contains("permission denied"), "{msg}");
        // Do not accuse the ones that did stop.
        assert!(!msg.contains("\"a\""), "{msg}");
    }

    #[test]
    fn stop_argv_targets_the_id() {
        assert_eq!(
            stop_argv(&["abc123".to_string()]),
            vec!["docker", "stop", "abc123"]
        );
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
        let msg = stopping_message(
            &[crate::compose::Member {
                id: "abc123".to_string(),
                name: "herdr_devcontainer".to_string(),
                service: String::new(),
            }],
            std::path::Path::new("/r"),
        );
        assert!(msg.contains("abc123"), "{msg}");
        assert!(msg.contains("herdr_devcontainer"), "{msg}");
        assert!(msg.contains("/r"), "{msg}");
    }

    // The line that stays on screen after the pane finishes is the record of
    // what the keystroke spent, so it names every container rather than a count.
    #[test]
    fn the_stop_message_names_every_container_of_a_project() {
        let msg = stopping_message(
            &[
                crate::compose::Member {
                    id: "a".to_string(),
                    name: "dh-app-1".to_string(),
                    service: "app".to_string(),
                },
                crate::compose::Member {
                    id: "b".to_string(),
                    name: "dh-postgres-1".to_string(),
                    service: "postgres".to_string(),
                },
            ],
            std::path::Path::new("/r"),
        );
        assert!(msg.contains("dh-app-1"), "{msg}");
        assert!(msg.contains("dh-postgres-1"), "{msg}");
        assert!(msg.contains('2'), "{msg}");
    }
}
