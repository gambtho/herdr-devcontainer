use std::path::{Path, PathBuf};
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

fn ps_argv_for(label_filter: String) -> Vec<String> {
    vec![
        "docker".to_string(),
        "ps".to_string(),
        "-a".to_string(),
        "--filter".to_string(),
        label_filter,
        "--format".to_string(),
        "{{.ID}}\t{{.Names}}\t{{.State}}".to_string(),
    ]
}

pub fn ps_argv(repo_root: &Path) -> Vec<String> {
    ps_argv_for(format!(
        "label=devcontainer.local_folder={}",
        repo_root.display()
    ))
}

/// The second discovery key. `devcontainer.local_folder` is written by whoever
/// *created* the container, and VS Code on Windows writes it as a UNC path
/// (`\\wsl.localhost\Ubuntu\home\u\repo`) that no POSIX repo root can equal.
/// `devcontainer.config_file` on that same container is POSIX, because the CLI
/// resolves it from inside WSL — so this stays an exact-match lookup instead of
/// guessing at the host's path rendering.
pub fn ps_argv_for_config_file(config_file: &Path) -> Vec<String> {
    ps_argv_for(format!(
        "label=devcontainer.config_file={}",
        config_file.display()
    ))
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

/// Both lookups can return the same container — a container the plugin created
/// matches on `local_folder` *and* `config_file`. Collapsing by id keeps that
/// from looking like two running containers, which `select_running` would
/// (correctly, given what it was told) refuse to choose between.
pub fn dedupe_by_id(containers: Vec<Container>) -> Vec<Container> {
    let mut seen = std::collections::HashSet::new();
    containers
        .into_iter()
        .filter(|c| seen.insert(c.id.clone()))
        .collect()
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

/// Every container that could belong to `repo_root`, looked up under both
/// identity labels.
///
/// Two `docker ps` calls rather than one: docker ANDs repeated `--filter label`
/// arguments, so a single call asking for both labels would match only
/// containers carrying both values — the opposite of what is needed. Results
/// are unioned and collapsed by id.
pub fn list(repo_root: &Path, config_files: &[PathBuf]) -> Result<Vec<Container>, Error> {
    let mut argvs = vec![ps_argv(repo_root)];
    argvs.extend(config_files.iter().map(|p| ps_argv_for_config_file(p)));

    let mut all = Vec::new();
    for argv in &argvs {
        let res = run(argv, Duration::from_secs(5), StderrMode::Capture)?;
        if res.exit_code != Some(0) {
            return Err(Error::DockerCommandFailed {
                detail: tail(res.stderr.trim(), 500),
            });
        }
        all.extend(parse_ps(&res.stdout)?);
    }
    Ok(dedupe_by_id(all))
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

    // VS Code on Windows writes `local_folder` as a UNC path
    // (`\\wsl.localhost\Ubuntu\home\u\repo`) while we compute a POSIX root, so
    // that label alone cannot find a container VS Code created. `config_file`
    // on the same container is POSIX, because the CLI writes it from inside
    // WSL, which makes it a second exact-match key rather than a path guess.
    #[test]
    fn ps_argv_for_config_file_filters_by_the_config_file_label() {
        let argv = ps_argv_for_config_file(Path::new("/r/.devcontainer/devcontainer.json"));
        assert!(argv
            .contains(&"label=devcontainer.config_file=/r/.devcontainer/devcontainer.json"
                .to_string()));
        assert!(argv.contains(&"-a".to_string()));
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

    // A plugin-created container carries a matching value under *both* labels,
    // so the two lookups return the same row twice. Left un-deduped that reads
    // as two running containers, and `select_running` would refuse to choose —
    // turning the fix into a hard error on the containers that already worked.
    #[test]
    fn dedupe_keeps_the_first_of_each_id() {
        let merged = dedupe_by_id(vec![
            c("a", "an", "running"),
            c("b", "bn", "exited"),
            c("a", "an", "running"),
        ]);
        assert_eq!(merged, vec![c("a", "an", "running"), c("b", "bn", "exited")]);
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
