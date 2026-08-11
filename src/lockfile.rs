use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use sha2::{Digest, Sha256};

use crate::error::Error;

pub struct RepoLock(#[allow(dead_code)] Flock<File>);

pub fn lock_path(repo_root: &Path) -> Result<PathBuf, Error> {
    Ok(lock_path_in(&state_dir()?, repo_root))
}

/// Split from `lock_path` so the naming rule can be tested without reading the
/// ambient environment — tests run in parallel threads, and a sibling test that
/// sets `XDG_STATE_HOME` would otherwise race this one.
fn lock_path_in(state_dir: &Path, repo_root: &Path) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let digest = Sha256::digest(repo_root.as_os_str().as_bytes());
    state_dir.join("locks").join(format!("{digest:x}.lock"))
}

/// An empty or missing base would put the lock at a *relative* path, so two
/// panes started from different directories would take two different locks and
/// bring-up would silently stop being serialized. Fail loudly instead.
fn state_dir() -> Result<PathBuf, Error> {
    state_dir_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn state_dir_from(
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, Error> {
    let base = xdg
        .filter(|x| !x.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })
        .ok_or_else(|| {
            Error::Other(
                "neither XDG_STATE_HOME nor HOME is set; cannot place the lock file".into(),
            )
        })?;
    Ok(base.join("herdr-devcontainer"))
}

pub fn acquire(repo_root: &Path) -> Result<RepoLock, Error> {
    let path = lock_path(repo_root)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let open = || {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
    };
    match Flock::lock(open()?, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(RepoLock(lock)),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => {
            eprintln!("waiting for another bring-up of this repo to finish...");
            Flock::lock(open()?, FlockArg::LockExclusive)
                .map(RepoLock)
                .map_err(|(_, e)| Error::Io(std::io::Error::from(e)))
        }
        Err((_, e)) => Err(Error::Io(std::io::Error::from(e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::fcntl::{Flock, FlockArg};
    use std::path::Path;

    // An empty XDG_STATE_HOME is set-but-unusable: joining onto "" yields a
    // *relative* lock path, so two panes started from different directories
    // would take two different locks and bring-up would silently stop being
    // serialized. `config.rs` refuses the same shape on the config path.
    #[test]
    fn empty_env_vars_count_as_unset() {
        assert!(state_dir_from(Some("".into()), Some("".into())).is_err());
        let got = state_dir_from(Some("".into()), Some("/home/u".into())).unwrap();
        assert_eq!(got, Path::new("/home/u/.local/state/herdr-devcontainer"));
        assert!(got.is_absolute());
    }

    #[test]
    fn no_state_base_is_an_error_not_a_relative_path() {
        assert!(state_dir_from(None, None).is_err());
        let x = state_dir_from(Some("/x/state".into()), None).unwrap();
        assert_eq!(x, Path::new("/x/state/herdr-devcontainer"));
    }

    #[test]
    fn lock_paths_are_stable_and_distinct() {
        let base = Path::new("/state/herdr-devcontainer");
        let a1 = lock_path_in(base, Path::new("/repo/a"));
        let a2 = lock_path_in(base, Path::new("/repo/a"));
        let b = lock_path_in(base, Path::new("/repo/b"));
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert!(a1.to_string_lossy().ends_with(".lock"));
        assert!(a1.is_absolute(), "lock path must not be relative: {a1:?}");
    }

    #[test]
    fn acquired_lock_excludes_a_second_flock() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path()); // isolate this test's state dir
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();

        let guard = acquire(&repo).unwrap();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path(&repo).unwrap())
            .unwrap();
        let second = Flock::lock(file, FlockArg::LockExclusiveNonblock);
        assert!(second.is_err(), "second flock should be excluded");
        drop(guard);

        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path(&repo).unwrap())
            .unwrap();
        assert!(Flock::lock(file, FlockArg::LockExclusiveNonblock).is_ok());
    }
}
