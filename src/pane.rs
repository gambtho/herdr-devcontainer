use std::path::Path;
use std::time::Duration;

use crate::discover::{self, Container};
use crate::error::Error;
use crate::exec::{self, Payload};
use crate::{config, context, detect, lockfile, preflight, shell, up, workdir};

/// Spec wrapper-flow step 4. `select_running` already refuses to choose between
/// several running matches; here we only need its error, not its selection.
pub fn check_unambiguous(containers: &[Container], repo_root: &Path) -> Result<(), Error> {
    discover::select_running(containers, repo_root).map(|_| ())
}

pub fn run_pane(shell: bool) -> Result<(), Error> {
    let cfg = config::load()?;
    for warning in &cfg.warnings {
        eprintln!("config: {warning}");
    }

    let ctx = context::load_context();
    let process_cwd = std::env::current_dir()?;
    let repo_root = context::resolve_repo_root(&ctx, &process_cwd)?;

    let repo_cfg = cfg.repo(&repo_root);
    let detection = detect::detect(&repo_root, &repo_cfg)?;

    let path_var = std::env::var("PATH").unwrap_or_default();
    let devcontainer_bin = preflight::find_devcontainer(&path_var)?;
    preflight::check_docker("docker")?;

    // Before taking the lock or starting anything: refuse an ambiguous repo.
    check_unambiguous(&discover::list(&repo_root)?, &repo_root)?;

    let up_result = {
        let _lock = lockfile::acquire(&repo_root)?;
        up::bring_up(
            &devcontainer_bin,
            &repo_root,
            detection.config_arg.as_deref(),
            Duration::from_secs(cfg.up_timeout_secs),
        )?
    };

    let cwd = context::pane_cwd(&ctx, &process_cwd);
    if let Some(unresolved) = &cwd.unresolved {
        eprintln!(
            "note: {unresolved} could not be resolved; starting from {} instead",
            cwd.path.display()
        );
    }
    let wd = workdir::map_workdir(&repo_root, &cwd.path, &up_result.remote_workspace_folder);
    if wd.outside_repo {
        eprintln!(
            "note: current directory is not under {}; starting at the container workspace root",
            repo_root.display()
        );
    }

    let payload = if shell {
        Payload::Shell
    } else {
        Payload::Command(cfg.command.clone())
    };
    // Configured value wins; otherwise ask the container. A probe that cannot
    // answer says so and says why, because the `sh` fallback silently
    // reinstates the missing rc files this exists to load.
    let exec_shell = match repo_cfg.shell.clone() {
        Some(configured) => configured,
        None => shell::probe(&up_result.container_id, up_result.remote_user.as_deref())
            .unwrap_or_else(|why| {
                eprintln!(
                    "note: could not read the container user's login shell ({why}); using {}, so rc files like ~/.zshrc will not be loaded",
                    shell::FALLBACK_SHELL
                );
                shell::FALLBACK_SHELL.to_string()
            }),
    };
    let argv = exec::exec_argv(&exec::ExecSpec {
        container_id: &up_result.container_id,
        remote_user: up_result.remote_user.as_deref(),
        workdir: &wd.path,
        shell: &exec_shell,
        env: &repo_cfg.env,
        payload: &payload,
    });
    // exec_into replaces the process; reaching the line below means it failed.
    Err(Error::Io(exec::exec_into(&argv)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::Container;
    use std::path::Path;

    fn c(id: &str, state: &str) -> Container {
        Container {
            id: id.to_string(),
            name: format!("{id}_name"),
            state: state.to_string(),
        }
    }

    // Spec wrapper-flow step 4: bring-up must not run when the repo has more
    // than one running dev container — `devcontainer up` would silently pick
    // one, which is exactly the case we refuse.
    #[test]
    fn ambiguous_repos_are_refused_before_bring_up() {
        let err = check_unambiguous(&[c("a", "running"), c("b", "running")], Path::new("/r"))
            .unwrap_err();
        assert!(matches!(err, Error::MultipleRunningContainers { .. }));
    }

    #[test]
    fn zero_or_one_running_proceeds() {
        assert!(check_unambiguous(&[], Path::new("/r")).is_ok());
        assert!(check_unambiguous(&[c("a", "running"), c("b", "exited")], Path::new("/r")).is_ok());
    }
}
