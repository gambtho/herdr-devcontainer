use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use sha2::{Digest, Sha256};

use crate::error::Error;

pub struct RepoLock(#[allow(dead_code)] Flock<File>);

pub fn lock_path(repo_root: &Path) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let digest = Sha256::digest(repo_root.as_os_str().as_bytes());
    state_dir().join("locks").join(format!("{digest:x}.lock"))
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("herdr-devcontainer")
}

pub fn acquire(repo_root: &Path) -> Result<RepoLock, Error> {
    let path = lock_path(repo_root);
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

    #[test]
    fn lock_paths_are_stable_and_distinct() {
        let a1 = lock_path(Path::new("/repo/a"));
        let a2 = lock_path(Path::new("/repo/a"));
        let b = lock_path(Path::new("/repo/b"));
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert!(a1.to_string_lossy().ends_with(".lock"));
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
            .open(lock_path(&repo))
            .unwrap();
        let second = Flock::lock(file, FlockArg::LockExclusiveNonblock);
        assert!(second.is_err(), "second flock should be excluded");
        drop(guard);

        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path(&repo))
            .unwrap();
        assert!(Flock::lock(file, FlockArg::LockExclusiveNonblock).is_ok());
    }
}
