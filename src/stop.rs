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

/// Stop `ids` in one call, then say nothing about whether it worked.
///
/// docker stops the listed containers in *parallel* — the argument order only
/// controls the order results print — so a call is one grace window regardless
/// of how many containers it names. Verification is deliberately not done here:
/// see `verify_stopped`, which asks docker what is running rather than reading
/// this call's output.
fn stop_call(ids: &[String]) -> Result<crate::run::RunResult, Error> {
    // docker's SIGTERM grace is 10s before SIGKILL, and the containers in one
    // call share that window, so the budget does not scale with the count.
    Ok(run(
        &stop_argv(ids),
        Duration::from_secs(STOP_TIMEOUT_SECS),
        StderrMode::Capture,
    )?)
}

const STOP_TIMEOUT_SECS: u64 = 30;

/// Confirm the stop by asking docker what is still running.
///
/// Not by parsing `docker stop`'s output: on timeout the CLI is killed and its
/// output is silent about what landed, and "no such container" handling meant
/// matching English prose to decide whether a container was gone. A container
/// that is not running is stopped, whichever way it got there.
fn verify_stopped(targets: &[compose::Member], stop_detail: String) -> Result<(), Error> {
    let alive = compose::still_running(targets)?;
    if alive.is_empty() {
        return Ok(());
    }
    Err(Error::ContainersNotStopped {
        // Names, not ids: this is what the user reads when six containers were
        // named in the prompt and one is still up. `docker stop` takes either.
        names: alive.iter().map(|m| m.name.clone()).collect(),
        detail: stop_detail,
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
    // Whether the first target is the dev container. Only then is there an
    // ordering worth paying a second grace window for; the orphan path below
    // returns survivors in docker's order, where "first" means nothing.
    let mut dev_leads = true;
    let targets = match discover::select_running(&containers, &repo_root)? {
        // Everything that goes down together, so the confirmation can name it
        // all before the user commits to it.
        Some(c) => compose::stop_set(&c.id, &c.name)?,
        // The dev container is not running — but its compose project may still
        // be. Saying "no running dev container" while postgres serves is the
        // same false absence this path exists to prevent.
        None => {
            dev_leads = false;
            let orphans = compose::orphaned_members(&containers)?;
            if !orphans.is_empty() {
                println!(
                    "the dev container for {} is not running, but {} of its compose services are:",
                    repo_root.display(),
                    orphans.len()
                );
            }
            orphans
        }
    };
    if targets.is_empty() {
        println!("no running dev container for {}", repo_root.display());
        return Ok(());
    }

    print!("{}", project_confirm_prompt(&targets, &repo_root));
    std::io::Write::flush(&mut std::io::stdout())?;
    let answer = read_answer(&mut std::io::stdin().lock())?;
    if !confirmed(&answer) {
        println!("{}", cancel_message(targets.len()));
        return Ok(());
    }
    println!("{}", stopping_message(&targets, &repo_root));

    // Two calls, not one argv: docker stops the containers named in a single
    // call in parallel, so passing them together would SIGTERM the database at
    // the same instant as the dev container still talking to it. The dev
    // container goes first and is waited on — that is the point of ordering,
    // and the direction Compose shuts a project down in.
    //
    // With no dev container to lead, there is nothing to order around: the
    // survivors go down together rather than paying a second grace window to
    // sequence one arbitrary service ahead of the others.
    let (first, rest) = if dev_leads {
        targets.split_at(1)
    } else {
        targets.split_at(0)
    };
    let mut detail = String::new();
    for phase in [first, rest] {
        if phase.is_empty() {
            continue;
        }
        let ids: Vec<String> = phase.iter().map(|m| m.id.clone()).collect();
        let res = stop_call(&ids)?;
        if res.timed_out {
            detail.push_str(&format!(
                "docker stop timed out after {STOP_TIMEOUT_SECS}s; "
            ));
        } else if res.exit_code != Some(0) {
            detail.push_str(&tail(res.stderr.trim(), 500));
            detail.push_str("; ");
        }
    }
    // Asked of docker, after the fact: a timeout kills the CLI without telling
    // us what landed, and a container that stopped is stopped whether or not
    // docker's output said so.
    verify_stopped(&targets, detail)?;
    println!("stopped.");
    Ok(())
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

    // What used to be inferred from `docker stop`'s prose is now asked of
    // docker directly, so that behavior is covered by the docker-gated
    // integration test rather than by string-matching unit tests. What belongs
    // here is the pure part: the error names the containers still running, so
    // a partial stop cannot be mistaken for a completed one.
    #[test]
    fn a_container_left_running_is_named_in_the_error() {
        let err = Error::ContainersNotStopped {
            names: vec!["dh_devcontainer-postgres-1".to_string()],
            detail: "cannot stop container: permission denied".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("dh_devcontainer-postgres-1"), "{msg}");
        assert!(msg.contains("permission denied"), "{msg}");
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
