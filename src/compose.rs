//! Compose-project awareness for stop.
//!
//! A compose-based dev container is one service of a project: stopping only it
//! leaves its database, cache, and gateway running, which is not what "stop the
//! dev container" means to anyone who asked for it. The Dev Container spec says
//! the same — `shutdownAction` defaults to `stopCompose` for compose configs.
//!
//! Membership comes from the container's own labels rather than from
//! `devcontainer.json`. Docker Compose writes `com.docker.compose.project` at
//! creation and finds its containers by it, so the labels are the authority
//! here; parsing the config would mean re-deriving a project name Compose
//! already computed, and this codebase does not re-implement `devcontainer.json`.

use std::time::Duration;

use crate::error::Error;
use crate::run::{run, StderrMode};
use crate::util::tail;

pub const PROJECT_LABEL: &str = "com.docker.compose.project";
pub const SERVICE_LABEL: &str = "com.docker.compose.service";
pub const ONEOFF_LABEL: &str = "com.docker.compose.oneoff";

pub fn project_argv(container_id: &str) -> Vec<String> {
    vec![
        "docker".to_string(),
        "inspect".to_string(),
        "--format".to_string(),
        format!("{{{{index .Config.Labels \"{PROJECT_LABEL}\"}}}}"),
        container_id.to_string(),
    ]
}

/// One running container of a compose project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub service: String,
}

pub fn members_argv(project: &str) -> Vec<String> {
    vec![
        "docker".to_string(),
        "ps".to_string(),
        "--filter".to_string(),
        format!("label={PROJECT_LABEL}={project}"),
        "--format".to_string(),
        format!(
            "{{{{.ID}}}}\t{{{{.Names}}}}\t{{{{.Label \"{SERVICE_LABEL}\"}}}}\t{{{{.Label \"{ONEOFF_LABEL}\"}}}}"
        ),
    ]
}

/// Parse the member listing, refusing rows we cannot read.
///
/// Discovery's rule applies with sharper stakes: dropping an unreadable row
/// here would leave that container running while the user is told the project
/// stopped.
///
/// One-off containers are excluded. `docker compose run` tags them
/// `oneoff=True` and `docker compose stop` leaves them alone, so stopping them
/// would kill a test run someone is watching in another pane. A row whose
/// oneoff label is *missing* is kept — an absent label is not a claim.
pub fn parse_members(stdout: &str) -> Result<Vec<Member>, Error> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').map(str::trim).collect();
        if fields.len() != 4 || fields[0].is_empty() || fields[1].is_empty() {
            return Err(Error::MalformedDockerOutput {
                line: line.to_string(),
            });
        }
        if fields[3].eq_ignore_ascii_case("true") {
            continue;
        }
        out.push(Member {
            id: fields[0].to_string(),
            name: fields[1].to_string(),
            service: fields[2].to_string(),
        });
    }
    Ok(out)
}

/// Order the stop so the dev container goes first.
///
/// It is the dependent — the thing holding connections to the database and
/// cache — so stopping it before them is the graceful direction, the same
/// reason Compose shuts a project down in reverse dependency order. Deeper
/// ordering via `com.docker.compose.depends_on` is deliberately not attempted:
/// in a dev container project every other service is a dependency of the dev
/// container, so one level is the whole ordering.
///
/// `dev_id` leads even when the listing does not contain it. The dev container
/// is always a member of its own project, so a listing without it means either
/// the two queries disagreed or it exited on its own — and in both cases
/// dropping the one container the user named is the wrong repair. `dev_name`
/// is carried onto that synthetic row because the confirmation prompt has to
/// be able to name every container it is about to stop.
pub fn order_for_stop(members: Vec<Member>, dev_id: &str, dev_name: &str) -> Vec<Member> {
    let (mut dev, rest): (Vec<Member>, Vec<Member>) =
        members.into_iter().partition(|m| m.id == dev_id);
    if dev.is_empty() {
        dev.push(Member {
            id: dev_id.to_string(),
            name: dev_name.to_string(),
            service: String::new(),
        });
    }
    dev.into_iter().chain(rest).collect()
}

/// Which of `ids` are still running, asked of docker rather than inferred.
///
/// The alternative was reading `docker stop`'s stdout and pattern-matching its
/// stderr for "no such container" — inferring the world from a CLI's prose, in
/// a place where being wrong means telling someone their database is down while
/// it is serving. This asks instead. It also covers the timeout case, where the
/// CLI was killed mid-flight and its output says nothing about what landed.
pub fn running_argv(ids: &[String]) -> Vec<String> {
    let mut argv = vec!["docker".to_string(), "ps".to_string()];
    // Same-type filters are OR'd, so this is one question about the whole set.
    for id in ids {
        argv.push("--filter".to_string());
        argv.push(format!("id={id}"));
    }
    argv.push("--format".to_string());
    argv.push("{{.ID}}".to_string());
    argv
}

pub fn parse_running_ids(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The subset of `members` that is still running.
pub fn still_running(members: &[Member]) -> Result<Vec<Member>, Error> {
    if members.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
    let out = docker_stdout(&running_argv(&ids))?;
    let alive = parse_running_ids(&out);
    Ok(members
        .iter()
        .filter(|m| alive.iter().any(|id| id == &m.id))
        .cloned()
        .collect())
}

/// Everything that should stop when the user stops `dev_id`, dev container
/// first.
///
/// A container outside a compose project yields just itself, so the plain
/// single-container path is unchanged.
///
/// A failed project lookup is *not* downgraded to "just this container".
/// Stopping one service of a project the user believes is fully down is the
/// failure this change exists to remove, so an unreadable project name or
/// member list is an error the user can see rather than a quietly smaller stop.
pub fn stop_set(dev_id: &str, dev_name: &str) -> Result<Vec<Member>, Error> {
    let alone = || {
        vec![Member {
            id: dev_id.to_string(),
            name: dev_name.to_string(),
            service: String::new(),
        }]
    };

    let Some(project) = project_of(dev_id)? else {
        return Ok(alone());
    };

    let members = parse_members(&docker_stdout(&members_argv(&project))?)?;
    if members.is_empty() {
        // Nothing in the project is running — the filter is by project, not by
        // this container, so this is not "the dev container vanished". Stopping
        // it anyway is idempotent and keeps the message about what the user
        // named.
        return Ok(alone());
    }
    Ok(order_for_stop(members, dev_id, dev_name))
}

/// Everything still running in the project of a dev container that is *not*
/// running itself.
///
/// The app container can exit on its own — a crash, an OOM kill, a stop from
/// another pane — while its database and cache keep running. Without this,
/// stop reports "no running dev container" and walks the user away from a live
/// project: the same false absence this discovery path exists to prevent, just
/// arrived at from the other side.
pub fn orphaned_members(candidates: &[crate::discover::Container]) -> Result<Vec<Member>, Error> {
    for c in candidates {
        let Some(project) = project_of(&c.id)? else {
            continue;
        };
        let members = parse_members(&docker_stdout(&members_argv(&project))?)?;
        if !members.is_empty() {
            return Ok(members);
        }
    }
    Ok(Vec::new())
}

/// The compose project of a container, or `None` if it has none.
///
/// A container that no longer exists reports none rather than failing. It can
/// be removed between discovery and here — a rebuild in another pane — and the
/// old single-container behavior treated a gone container as already stopped,
/// which is still the right answer.
fn project_of(container_id: &str) -> Result<Option<String>, Error> {
    let res = run(
        &project_argv(container_id),
        Duration::from_secs(5),
        StderrMode::Capture,
    )?;
    if res.exit_code != Some(0) {
        let stderr = res.stderr.to_lowercase();
        if stderr.contains("no such object") || stderr.contains("no such container") {
            return Ok(None);
        }
    }
    let out = check(res)?;
    Ok(parse_project(&out))
}

fn docker_stdout(argv: &[String]) -> Result<String, Error> {
    let res = run(argv, Duration::from_secs(5), StderrMode::Capture)?;
    check(res)
}

fn check(res: crate::run::RunResult) -> Result<String, Error> {
    if res.timed_out {
        return Err(Error::DockerCommandFailed {
            detail: "a docker query timed out after 5s".to_string(),
        });
    }
    if res.exit_code != Some(0) || res.stdout_incomplete {
        return Err(Error::DockerCommandFailed {
            detail: tail(res.stderr.trim(), 500),
        });
    }
    Ok(res.stdout)
}

/// The compose project a container belongs to, if any.
///
/// Docker's template prints an empty line when the label is missing, and
/// `<no value>` on older versions. Neither is a project name, and reading
/// either as one would send us looking for members of a project that does not
/// exist — so both mean "this is a plain single container".
pub fn parse_project(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() || trimmed == "<no value>" {
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: &str, name: &str, service: &str) -> Member {
        Member {
            id: id.to_string(),
            name: name.to_string(),
            service: service.to_string(),
        }
    }

    // `docker compose run` containers carry the project label too, and
    // `docker compose stop` deliberately leaves them alone. Sweeping them in
    // would kill a `docker compose run --rm app pytest` a developer is watching
    // in another pane — the opposite of the stopCompose parity this change
    // claims. Docker has no negated label filter, so the row is filtered here.
    #[test]
    fn a_oneoff_run_container_is_not_part_of_the_project_stop() {
        let parsed =
            parse_members("abc\tproj-app-1\tapp\tFalse\ndef\tproj-app-run-x\tapp\tTrue\n").unwrap();
        assert_eq!(parsed, vec![m("abc", "proj-app-1", "app")]);
    }

    // A missing oneoff label is not a claim that the container is one-off.
    // Keeping it matches how the rest of this codebase treats an unknown.
    #[test]
    fn a_row_without_the_oneoff_label_is_kept() {
        let parsed = parse_members("abc\tproj-app-1\tapp\t\n").unwrap();
        assert_eq!(parsed, vec![m("abc", "proj-app-1", "app")]);
    }

    // Whether a container stopped is a question about the world, not about
    // docker's prose. Same-type filters are OR'd, so one query covers the set.
    // The mirror of the bug this feature exists to fix. The app container can
    // exit on its own — a crash, an OOM kill, a `docker stop` from another pane
    // — while postgres and redis keep running. Reporting "no running dev
    // container" then walks the user away from a live database. Discovery
    // already lists exited containers (`docker ps -a`), and a stopped container
    // keeps its project label, so the evidence is in hand.
    #[test]
    fn an_exited_dev_container_still_names_its_project() {
        // Ordering is what is testable purely: the exited container is not a
        // stop target, so the survivors stand alone in the set.
        let survivors = vec![
            m("db1", "proj-db-1", "db"),
            m("r1", "proj-redis-1", "redis"),
        ];
        let ordered = order_for_stop(survivors.clone(), "gone", "");
        assert_eq!(
            ordered.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["gone", "db1", "r1"],
            "a dev container absent from the listing is still led with"
        );
    }

    // The synthetic row is the one container the user actually named. Rendering
    // it blank in the safety prompt hides exactly what they need to recognize —
    // and the name is already in hand at the call site.
    #[test]
    fn the_synthetic_dev_row_carries_its_name() {
        let ordered = order_for_stop(vec![m("db1", "proj-db-1", "db")], "app1", "proj-app-1");
        assert_eq!(ordered[0].id, "app1");
        assert_eq!(ordered[0].name, "proj-app-1");
    }

    #[test]
    fn running_argv_asks_about_exactly_these_containers() {
        let argv = running_argv(&["a".to_string(), "b".to_string()]);
        assert!(argv.contains(&"id=a".to_string()), "{argv:?}");
        assert!(argv.contains(&"id=b".to_string()), "{argv:?}");
        // Running only: `-a` would report a stopped container as still there.
        assert!(!argv.contains(&"-a".to_string()), "{argv:?}");
    }

    #[test]
    fn parse_running_ids_reads_one_id_per_line() {
        assert_eq!(parse_running_ids("a\nb\n"), vec!["a", "b"]);
        assert_eq!(parse_running_ids("\n"), Vec::<String>::new());
    }

    #[test]
    fn members_argv_lists_only_running_members_of_the_project() {
        let argv = members_argv("proj_devcontainer");
        assert!(argv.contains(&"label=com.docker.compose.project=proj_devcontainer".to_string()));
        // No `-a`: a container that is already stopped is not something to stop.
        assert!(!argv.contains(&"-a".to_string()), "{argv:?}");
    }

    #[test]
    fn parse_members_reads_id_name_and_service() {
        let parsed =
            parse_members("abc\tproj-app-1\tapp\tFalse\ndef\tproj-db-1\tdb\tFalse\n").unwrap();
        assert_eq!(
            parsed,
            vec![m("abc", "proj-app-1", "app"), m("def", "proj-db-1", "db")]
        );
    }

    // Same rule discovery follows: a row we cannot read must not quietly shrink
    // the set, because here that means silently leaving a container running
    // after telling the user everything stopped.
    #[test]
    fn parse_members_rejects_malformed_rows() {
        assert!(parse_members("abc\tonly-two\n").is_err());
        assert!(parse_members("\t\tapp\tFalse\n").is_err());
    }

    // Compose stops in reverse dependency order for a reason: the dev container
    // is the thing writing to the database, so it goes first. Full topological
    // ordering is not attempted — one level covers the real shape, where every
    // other service is a dependency of the dev container.
    #[test]
    fn the_dev_container_stops_before_its_dependencies() {
        let members = vec![
            m("db1", "proj-db-1", "db"),
            m("app1", "proj-app-1", "app"),
            m("redis1", "proj-redis-1", "redis"),
        ];
        let ordered = order_for_stop(members, "app1", "proj-app-1");
        assert_eq!(
            ordered.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["app1", "db1", "redis1"],
            "the dev container leads; the rest keep their listed order"
        );
    }

    // The dev container is always a member of its own project, so its absence
    // means the two queries disagreed — a container stopped or was removed
    // between them. Dropping it would stop the siblings and leave the one the
    // user actually named running.
    #[test]
    fn a_missing_dev_container_is_still_stopped_first() {
        let members = vec![m("db1", "proj-db-1", "db")];
        let ordered = order_for_stop(members, "app1", "proj-app-1");
        assert_eq!(
            ordered.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["app1", "db1"]
        );
    }

    #[test]
    fn project_argv_asks_for_just_the_project_label() {
        let argv = project_argv("abc123");
        assert_eq!(argv[0], "docker");
        assert!(argv.contains(&"inspect".to_string()));
        assert!(argv.contains(&"abc123".to_string()));
        assert!(
            argv.iter()
                .any(|a| a.contains("com.docker.compose.project")),
            "{argv:?}"
        );
    }

    // A container with no compose project is the plain single-container case,
    // which must keep behaving exactly as it did. Docker's template prints an
    // empty line for a missing key, and `<no value>` on older versions, so
    // neither may be mistaken for a project actually named that.
    #[test]
    fn a_container_outside_a_compose_project_has_no_project() {
        assert_eq!(parse_project(""), None);
        assert_eq!(parse_project("\n"), None);
        assert_eq!(parse_project("<no value>\n"), None);
        assert_eq!(parse_project("   \n"), None);
    }

    #[test]
    fn a_compose_container_reports_its_project() {
        assert_eq!(
            parse_project("double-holo-ui_devcontainer\n"),
            Some("double-holo-ui_devcontainer".to_string())
        );
    }
}
