use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub struct Workdir {
    pub path: String,
    pub outside_repo: bool,
}

pub fn map_workdir(repo_root: &Path, cwd: &Path, remote_root: &str) -> Workdir {
    match cwd.strip_prefix(repo_root) {
        Ok(rel) if rel.as_os_str().is_empty() => Workdir {
            path: remote_root.trim_end_matches('/').to_string(),
            outside_repo: false,
        },
        Ok(rel) => {
            let mut path = remote_root.trim_end_matches('/').to_string();
            for comp in rel.components() {
                path.push('/');
                path.push_str(&comp.as_os_str().to_string_lossy());
            }
            Workdir {
                path,
                outside_repo: false,
            }
        }
        Err(_) => Workdir {
            path: remote_root.trim_end_matches('/').to_string(),
            outside_repo: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn repo_root_maps_to_remote_root() {
        let wd = map_workdir(Path::new("/r"), Path::new("/r"), "/workspaces/p");
        assert_eq!(wd.path, "/workspaces/p");
        assert!(!wd.outside_repo);
    }

    #[test]
    fn subdirectory_is_joined_posix_style() {
        let wd = map_workdir(Path::new("/r"), Path::new("/r/sub/dir"), "/workspaces/p");
        assert_eq!(wd.path, "/workspaces/p/sub/dir");
    }

    #[test]
    fn trailing_slash_on_remote_root_does_not_double() {
        let wd = map_workdir(Path::new("/r"), Path::new("/r/sub"), "/workspaces/p/");
        assert_eq!(wd.path, "/workspaces/p/sub");
    }

    #[test]
    fn cwd_outside_the_repo_falls_back_to_remote_root() {
        let wd = map_workdir(Path::new("/r"), Path::new("/elsewhere/wt"), "/workspaces/p");
        assert_eq!(wd.path, "/workspaces/p");
        assert!(wd.outside_repo);
    }
}
