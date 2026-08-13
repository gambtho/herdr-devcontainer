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
///
/// The lookups are sequential, so the same container can be reported with
/// different states: one can start or stop between the two `docker ps` calls.
/// A disagreement resolves toward `running`, because the other direction would
/// report an absence we already hold evidence against.
pub fn dedupe_by_id(containers: Vec<Container>) -> Vec<Container> {
    let mut order: Vec<String> = Vec::new();
    let mut by_id: std::collections::HashMap<String, Container> = std::collections::HashMap::new();
    for c in containers {
        match by_id.get_mut(&c.id) {
            Some(existing) => {
                if c.state == "running" {
                    existing.state = c.state;
                }
            }
            None => {
                order.push(c.id.clone());
                by_id.insert(c.id.clone(), c);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
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

const PS_TIMEOUT_SECS: u64 = 5;

/// Classify one `docker ps` outcome.
///
/// A timeout is reported as a timeout: `run` kills the process group, so the
/// exit code is `None` and the generic branch below would otherwise render it
/// as a bare "docker command failed" with an empty detail. `shell::probe`
/// distinguishes the two the same way.
///
/// A truncated capture is an error even at exit 0. Truncation on a line
/// boundary parses cleanly, so the short list would otherwise be indexed as the
/// complete set of containers — absence manufactured from a read failure.
fn check_ps_result(res: &crate::run::RunResult) -> Result<(), Error> {
    if res.timed_out {
        return Err(Error::DockerCommandFailed {
            detail: format!("docker ps timed out after {PS_TIMEOUT_SECS}s"),
        });
    }
    if res.exit_code != Some(0) {
        return Err(Error::DockerCommandFailed {
            detail: tail(res.stderr.trim(), 500),
        });
    }
    if res.stdout_incomplete {
        return Err(Error::DockerCommandFailed {
            detail: "docker ps output was incomplete (its stdout could not be read to the end)"
                .to_string(),
        });
    }
    Ok(())
}

/// Every `docker ps` this lookup needs: the `local_folder` key, plus one
/// `config_file` key per candidate config path.
///
/// Split out from `list` so the union is provable without a docker daemon.
/// Folded into `list`, the wiring could be deleted with every unit test still
/// green — the pieces were each tested alone, which is not the same as testing
/// that they are connected.
fn list_argvs(repo_root: &Path, config_files: &[PathBuf]) -> Vec<Vec<String>> {
    let mut argvs = vec![ps_argv(repo_root)];
    argvs.extend(config_files.iter().map(|p| ps_argv_for_config_file(p)));
    argvs
}

/// Every container that could belong to `repo_root`, looked up under both
/// identity labels.
///
/// Two `docker ps` calls rather than one: docker ANDs repeated `--filter label`
/// arguments, so a single call asking for both labels would match only
/// containers carrying both values — the opposite of what is needed. Results
/// are unioned and collapsed by id.
pub fn list(repo_root: &Path, config_files: &[PathBuf]) -> Result<Vec<Container>, Error> {
    let mut all = Vec::new();
    for argv in &list_argvs(repo_root, config_files) {
        let res = run(
            argv,
            Duration::from_secs(PS_TIMEOUT_SECS),
            StderrMode::Capture,
        )?;
        check_ps_result(&res)?;
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

    // A truncated listing that happens to break on a line boundary parses
    // perfectly and is simply short — the container we wanted may be in the
    // part we never read. Exit code 0 does not make it complete.
    #[test]
    fn a_truncated_listing_is_an_error_not_a_shorter_answer() {
        let res = crate::run::RunResult {
            exit_code: Some(0),
            stdout: "abc123\tfoo\trunning\n".to_string(),
            stdout_incomplete: true,
            stderr: String::new(),
            timed_out: false,
        };
        let err = check_ps_result(&res).unwrap_err();
        assert!(err.to_string().contains("incomplete"), "{err}");
    }

    // A killed process reports no exit code, so the generic branch renders a
    // timeout as "docker command failed" with an empty detail — nothing the
    // user can act on. This path now runs up to three times per invocation, so
    // the uninformative version costs three timeouts to reach.
    #[test]
    fn a_timed_out_ps_says_so_rather_than_failing_blankly() {
        let res = crate::run::RunResult {
            exit_code: None,
            stdout: String::new(),
            stdout_incomplete: false,
            stderr: String::new(),
            timed_out: true,
        };
        let err = check_ps_result(&res).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("timed out"), "{msg}");
        assert!(msg.contains('5'), "{msg} should name the budget");
    }

    #[test]
    fn a_failed_ps_carries_its_stderr() {
        let res = crate::run::RunResult {
            exit_code: Some(1),
            stdout: String::new(),
            stdout_incomplete: false,
            stderr: "permission denied while trying to connect".to_string(),
            timed_out: false,
        };
        let err = check_ps_result(&res).unwrap_err();
        assert!(err.to_string().contains("permission denied"));
    }

    // The fix's actual decision is "one `docker ps` per config file, unioned
    // with the local_folder lookup". Tested here rather than only inside the
    // docker-gated integration test, because deleting the wiring left every
    // other unit test green — each piece was proven in isolation while nothing
    // proved they were connected.
    #[test]
    fn list_queries_the_folder_label_and_every_config_file() {
        let argvs = list_argvs(
            Path::new("/r"),
            &[
                PathBuf::from("/r/.devcontainer/devcontainer.json"),
                PathBuf::from("/r/.devcontainer.json"),
            ],
        );
        assert_eq!(argvs.len(), 3, "one folder lookup plus one per config file");
        assert!(argvs[0].contains(&"label=devcontainer.local_folder=/r".to_string()));
        assert!(argvs[1].contains(
            &"label=devcontainer.config_file=/r/.devcontainer/devcontainer.json".to_string()
        ));
        assert!(
            argvs[2].contains(&"label=devcontainer.config_file=/r/.devcontainer.json".to_string())
        );
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
        assert!(argv.contains(
            &"label=devcontainer.config_file=/r/.devcontainer/devcontainer.json".to_string()
        ));
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
    // The two lookups are sequential, not atomic, so one container can appear
    // in both with different states — another pane's `devcontainer up` can
    // finish between them. Keeping the first-seen row would retain the stale
    // `exited` one and report "no running dev container" for a container that
    // started while we were asking. Resolve the disagreement toward running:
    // the alternative is asserting an absence we have evidence against.
    #[test]
    fn dedupe_prefers_a_running_row_over_a_stale_one() {
        let merged = dedupe_by_id(vec![c("a", "an", "exited"), c("a", "an", "running")]);
        assert_eq!(merged, vec![c("a", "an", "running")]);
        // ...and the order of the disagreement must not matter.
        let merged = dedupe_by_id(vec![c("a", "an", "running"), c("a", "an", "exited")]);
        assert_eq!(merged, vec![c("a", "an", "running")]);
    }

    #[test]
    fn dedupe_keeps_the_first_of_each_id() {
        let merged = dedupe_by_id(vec![
            c("a", "an", "running"),
            c("b", "bn", "exited"),
            c("a", "an", "running"),
        ]);
        assert_eq!(
            merged,
            vec![c("a", "an", "running"), c("b", "bn", "exited")]
        );
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
