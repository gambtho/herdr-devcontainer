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
        format!("{{{{.ID}}}}\t{{{{.Names}}}}\t{{{{.Label \"{SERVICE_LABEL}\"}}}}"),
    ]
}

/// Parse the member listing, refusing rows we cannot read.
///
/// Discovery's rule applies with sharper stakes: dropping an unreadable row
/// here would leave that container running while the user is told the project
/// stopped.
pub fn parse_members(stdout: &str) -> Result<Vec<Member>, Error> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').map(str::trim).collect();
        if fields.len() != 3 || fields[0].is_empty() || fields[1].is_empty() {
            return Err(Error::MalformedDockerOutput {
                line: line.to_string(),
            });
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
/// `dev_id` is put at the front even when the listing does not contain it. The
/// dev container is always a member of its own project, so a listing without it
/// means the two queries disagreed, and the fix is not to drop the one
/// container the user actually named.
pub fn order_for_stop(members: Vec<Member>, dev_id: &str) -> Vec<Member> {
    let (mut dev, rest): (Vec<Member>, Vec<Member>) =
        members.into_iter().partition(|m| m.id == dev_id);
    if dev.is_empty() {
        dev.push(Member {
            id: dev_id.to_string(),
            name: String::new(),
            service: String::new(),
        });
    }
    dev.into_iter().chain(rest).collect()
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

    let out = docker_stdout(&project_argv(dev_id))?;
    let Some(project) = parse_project(&out) else {
        return Ok(alone());
    };

    let members = parse_members(&docker_stdout(&members_argv(&project))?)?;
    if members.is_empty() {
        // The dev container is a member of its own project, so an empty listing
        // means it stopped between the two queries. Its own stop is idempotent.
        return Ok(alone());
    }
    Ok(order_for_stop(members, dev_id))
}

fn docker_stdout(argv: &[String]) -> Result<String, Error> {
    let res = run(argv, Duration::from_secs(5), StderrMode::Capture)?;
    if res.timed_out {
        return Err(Error::DockerCommandFailed {
            detail: format!("{} timed out after 5s", argv.join(" ")),
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

    #[test]
    fn members_argv_lists_only_running_members_of_the_project() {
        let argv = members_argv("proj_devcontainer");
        assert!(argv.contains(&"label=com.docker.compose.project=proj_devcontainer".to_string()));
        // No `-a`: a container that is already stopped is not something to stop.
        assert!(!argv.contains(&"-a".to_string()), "{argv:?}");
    }

    #[test]
    fn parse_members_reads_id_name_and_service() {
        let parsed = parse_members("abc\tproj-app-1\tapp\ndef\tproj-db-1\tdb\n").unwrap();
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
        assert!(parse_members("\t\tapp\n").is_err());
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
        let ordered = order_for_stop(members, "app1");
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
        let ordered = order_for_stop(members, "app1");
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
