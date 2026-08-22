//! Path helpers shared by the store and by sources.
//!
//! Not a deep module — three short functions with no hidden machinery. It
//! exists only because the store, sources and the command all need the same
//! notion of "expand a leading ~" and would otherwise each carry a copy.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot determine home directory: HOME is unset"))
}
pub fn expand(raw: &str) -> Result<PathBuf> {
    let expanded = if raw == "~" {
        home()?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home()?.join(rest)
    } else {
        PathBuf::from(raw)
    };
    std::path::absolute(&expanded).with_context(|| format!("cannot resolve {expanded:?}"))
}

/// Shortens a path under the home directory for display. The prefix has to end
/// on a path boundary, so /Users/jamie is not shortened for /Users/james.
pub fn tildify(path: &Path) -> String {
    let Ok(home) = home() else {
        return path.display().to_string();
    };
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testing::EnvGuard;

    #[test]
    fn home_is_read_from_the_environment() {
        let mut env = EnvGuard::new();
        env.set("HOME", "/home/ada");
        assert_eq!(home().unwrap(), PathBuf::from("/home/ada"));
    }

    #[test]
    fn home_fails_when_it_is_unset() {
        let mut env = EnvGuard::new();
        env.remove("HOME");
        let err = home().unwrap_err().to_string();
        assert!(err.contains("HOME is unset"), "{err}");
    }

    /// An empty HOME would otherwise make every path relative to the root.
    #[test]
    fn home_fails_when_it_is_empty() {
        let mut env = EnvGuard::new();
        env.set("HOME", "");
        assert!(home().is_err());
    }

    #[test]
    fn expand_replaces_a_leading_tilde() {
        let mut env = EnvGuard::new();
        env.set("HOME", "/home/ada");
        assert_eq!(expand("~").unwrap(), PathBuf::from("/home/ada"));
        assert_eq!(
            expand("~/code/try").unwrap(),
            PathBuf::from("/home/ada/code/try")
        );
    }

    /// Only `~` and `~/…` are the home directory. `~ada` is another person's,
    /// and expanding it is the shell's job, not ours.
    #[test]
    fn expand_leaves_a_named_tilde_alone() {
        let mut env = EnvGuard::new();
        env.set("HOME", "/home/ada");
        let here = std::env::current_dir().unwrap();
        assert_eq!(expand("~ada").unwrap(), here.join("~ada"));
    }

    #[test]
    fn expand_keeps_an_absolute_path() {
        assert_eq!(expand("/var/log").unwrap(), PathBuf::from("/var/log"));
    }

    #[test]
    fn expand_makes_a_relative_path_absolute() {
        let here = std::env::current_dir().unwrap();
        assert_eq!(expand("src").unwrap(), here.join("src"));
        assert_eq!(expand("./src").unwrap(), here.join("src"));
    }

    #[test]
    fn expand_rejects_an_empty_path() {
        assert!(expand("").is_err());
    }

    #[test]
    fn tildify_shortens_the_home_directory_itself() {
        let mut env = EnvGuard::new();
        env.set("HOME", "/home/ada");
        assert_eq!(tildify(Path::new("/home/ada")), "~");
    }

    #[test]
    fn tildify_shortens_a_path_under_home() {
        let mut env = EnvGuard::new();
        env.set("HOME", "/home/ada");
        assert_eq!(tildify(Path::new("/home/ada/try/redis")), "~/try/redis");
    }

    /// The prefix has to end on a path boundary: /home/adamant is not inside
    /// /home/ada.
    #[test]
    fn tildify_respects_path_boundaries() {
        let mut env = EnvGuard::new();
        env.set("HOME", "/home/ada");
        assert_eq!(tildify(Path::new("/home/adamant")), "/home/adamant");
    }

    #[test]
    fn tildify_leaves_paths_outside_home_alone() {
        let mut env = EnvGuard::new();
        env.set("HOME", "/home/ada");
        assert_eq!(tildify(Path::new("/var/log")), "/var/log");
    }

    /// Display is all tildify is for, so no home directory is not an error.
    #[test]
    fn tildify_falls_back_to_the_whole_path_without_a_home() {
        let mut env = EnvGuard::new();
        env.remove("HOME");
        assert_eq!(tildify(Path::new("/var/log")), "/var/log");
    }
}
