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
        let parsed =
            parse_ps("abc123\tfoo_devcontainer\tRunning\ndef456\tbar\texited\n\n").unwrap();
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
