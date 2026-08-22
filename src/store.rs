//! The project root and the directories inside it.
//!
//! A name is an identity: it maps to exactly one directory under the root.
//! This module owns where the root is, what counts as a project, what order
//! projects come back in, and what a name is allowed to look like.

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use crate::paths;

/// The directory that holds every project.
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Opens the root named by `$TRY_PATH`, or `~/try` when it is unset. The
    /// directory is not required to exist yet.
    pub fn open() -> Result<Self> {
        let root = match std::env::var("TRY_PATH") {
            Ok(raw) if !raw.trim().is_empty() => paths::expand(raw.trim())?,
            _ => paths::home()?.join("try"),
        };
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The project of this name, if it exists. A symlink counts.
    pub fn find(&self, name: &str) -> Option<PathBuf> {
        let path = self.root.join(name);
        fs::symlink_metadata(&path).is_ok().then_some(path)
    }

    /// Makes the root and returns the path a new project of this name will
    /// occupy. The project directory itself is left to the caller to create,
    /// because how it is filled is the source's business.
    pub fn reserve(&self, name: &str) -> Result<PathBuf> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("cannot create {}", self.root.display()))?;
        Ok(self.root.join(name))
    }

    /// Creates an empty project and returns its path.
    pub fn create_empty(&self, name: &str) -> Result<PathBuf> {
        let path = self.reserve(name)?;
        fs::create_dir(&path).with_context(|| format!("cannot create {}", path.display()))?;
        Ok(path)
    }

    /// Every project under root, most recently modified first.
    pub fn list(&self) -> Result<Vec<Project>> {
            let root = &self.root;
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", root.display())),
        };

        let mut projects = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("cannot read {}", root.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            // Symlinked projects are followed; a broken one is still listed, so it
            // is visible rather than quietly missing.
            let target = fs::metadata(&path);
            let linked = fs::symlink_metadata(&path);
            let keep = match (&target, &linked) {
                (Ok(meta), _) => meta.is_dir(),
                (Err(_), Ok(meta)) => meta.is_symlink(),
                _ => false,
            };
            if !keep {
                continue;
            }
            projects.push(Project {
                name,
                path,
                mtime: target.ok().or_else(|| linked.ok()).and_then(|m| m.modified().ok()),
            });
        }

        projects.sort_by(|a, b| match (a.mtime, b.mtime) {
            (Some(x), Some(y)) if x != y => y.cmp(&x), // most recent first
            (Some(_), None) => Ordering::Less,         // undated last
            (None, Some(_)) => Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        Ok(projects)
    }
}

/// A single directory under the try root.
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub mtime: Option<SystemTime>,
}

/// Turns user input into a directory-safe project name: separators become
/// dashes, runs of dashes collapse, and leading or trailing dashes and dots
/// are dropped.
pub fn clean_name(raw: &str) -> Result<String> {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.trim().chars() {
        if ch.is_whitespace() || matches!(ch, '/' | '\\' | ':' | '-') {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(ch);
        }
    }
    let name = out.trim_matches(|c| c == '-' || c == '.').to_string();
    if name.is_empty() {
        bail!("invalid project name {raw:?}");
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::symlink;

    use crate::testing::{EnvGuard, TempDir, set_mtime};

    fn store_at(dir: &TempDir) -> Store {
        Store {
            root: dir.path().to_path_buf(),
        }
    }

    #[test]
    fn the_root_defaults_to_try_under_home() {
        let mut env = EnvGuard::new();
        env.set("HOME", "/home/ada").remove("TRY_PATH");
        assert_eq!(Store::open().unwrap().root(), Path::new("/home/ada/try"));
    }

    #[test]
    fn try_path_names_the_root() {
        let mut env = EnvGuard::new();
        env.set("HOME", "/home/ada").set("TRY_PATH", "/srv/projects");
        assert_eq!(Store::open().unwrap().root(), Path::new("/srv/projects"));
    }

    #[test]
    fn try_path_is_expanded_and_trimmed() {
        let mut env = EnvGuard::new();
        env.set("HOME", "/home/ada").set("TRY_PATH", "  ~/work  ");
        assert_eq!(Store::open().unwrap().root(), Path::new("/home/ada/work"));
    }

    /// An empty or blank TRY_PATH is someone unsetting it clumsily, not a
    /// request to use the current directory.
    #[test]
    fn a_blank_try_path_falls_back_to_the_default() {
        let mut env = EnvGuard::new();
        env.set("HOME", "/home/ada").set("TRY_PATH", "   ");
        assert_eq!(Store::open().unwrap().root(), Path::new("/home/ada/try"));

        env.set("TRY_PATH", "");
        assert_eq!(Store::open().unwrap().root(), Path::new("/home/ada/try"));
    }

    /// The root is where projects will go; it does not have to be there yet.
    #[test]
    fn opening_does_not_require_the_root_to_exist() {
        let mut env = EnvGuard::new();
        env.set("HOME", "/home/ada")
            .set("TRY_PATH", "/nowhere/at/all");
        assert!(Store::open().is_ok());
    }

    #[test]
    fn find_returns_an_existing_project() {
        let dir = TempDir::new("find");
        let store = store_at(&dir);
        let made = dir.dir("redis");
        assert_eq!(store.find("redis"), Some(made));
    }

    #[test]
    fn find_returns_nothing_for_a_name_that_is_not_there() {
        let dir = TempDir::new("find-missing");
        assert_eq!(store_at(&dir).find("redis"), None);
    }

    /// A broken symlink is still a name that is taken, so entering it is
    /// better than silently cloning over it.
    #[test]
    fn find_counts_a_broken_symlink() {
        let dir = TempDir::new("find-broken");
        symlink("/nowhere/at/all", dir.join("ghost")).unwrap();
        assert_eq!(store_at(&dir).find("ghost"), Some(dir.join("ghost")));
    }

    #[test]
    fn reserve_makes_the_root_but_not_the_project() {
        let dir = TempDir::new("reserve");
        let root = dir.join("root");
        let store = Store { root: root.clone() };

        let path = store.reserve("redis").unwrap();

        assert_eq!(path, root.join("redis"));
        assert!(root.is_dir(), "the root is created");
        assert!(!path.exists(), "the project is left to the source");
    }

    #[test]
    fn create_empty_makes_the_project_directory() {
        let dir = TempDir::new("create");
        let store = Store {
            root: dir.join("root"),
        };
        let path = store.create_empty("notes").unwrap();
        assert!(path.is_dir());
        assert_eq!(path, dir.join("root").join("notes"));
    }

    #[test]
    fn create_empty_fails_when_the_name_is_taken() {
        let dir = TempDir::new("create-twice");
        let store = store_at(&dir);
        store.create_empty("notes").unwrap();
        assert!(store.create_empty("notes").is_err());
    }

    /// Nothing has been created yet, which is not an error: there are simply
    /// no projects.
    #[test]
    fn listing_a_root_that_is_not_there_finds_nothing() {
        let dir = TempDir::new("list-missing");
        let store = Store {
            root: dir.join("root"),
        };
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn listing_finds_directories_only() {
        let dir = TempDir::new("list-kinds");
        dir.dir("redis");
        dir.file("notes.txt", "not a project");
        dir.dir(".hidden");
        dir.file(".dotfile", "");

        let names: Vec<String> = store_at(&dir)
            .list()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["redis".to_string()]);
    }

    #[test]
    fn listing_follows_a_symlinked_project() {
        let dir = TempDir::new("list-link");
        let elsewhere = TempDir::new("list-link-target");
        symlink(elsewhere.path(), dir.join("linked")).unwrap();

        let projects = store_at(&dir).list().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "linked");
    }

    /// A broken link is listed rather than dropped, so it is visible as
    /// something to fix instead of quietly missing.
    #[test]
    fn listing_keeps_a_broken_symlink() {
        let dir = TempDir::new("list-broken");
        symlink("/nowhere/at/all", dir.join("ghost")).unwrap();
        symlink("/nowhere/either", dir.join("gone.txt")).unwrap();

        let mut names: Vec<String> = store_at(&dir)
            .list()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["ghost".to_string(), "gone.txt".to_string()]);
    }

    #[test]
    fn listing_puts_the_most_recent_first() {
        let dir = TempDir::new("list-order");
        for (name, mtime) in [("old", 1_000), ("newest", 3_000), ("middle", 2_000)] {
            set_mtime(&dir.dir(name), mtime);
        }

        let names: Vec<String> = store_at(&dir)
            .list()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["newest", "middle", "old"]);
    }

    /// Directory order is whatever the filesystem feels like, so projects
    /// touched at the same moment need a tiebreak of their own.
    #[test]
    fn projects_of_the_same_age_are_ordered_by_name() {
        let dir = TempDir::new("list-tie");
        for name in ["charlie", "alpha", "bravo"] {
            set_mtime(&dir.dir(name), 1_000);
        }

        let names: Vec<String> = store_at(&dir)
            .list()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn a_listed_project_carries_its_path_and_time() {
        let dir = TempDir::new("list-fields");
        let path = dir.dir("redis");
        set_mtime(&path, 1_234);

        let projects = store_at(&dir).list().unwrap();
        assert_eq!(projects[0].path, path);
        assert_eq!(
            projects[0].mtime,
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_234))
        );
    }

    #[test]
    fn a_plain_name_is_left_alone() {
        assert_eq!(clean_name("redis").unwrap(), "redis");
        assert_eq!(clean_name("My Project").unwrap(), "My-Project");
        assert_eq!(clean_name("try_2").unwrap(), "try_2");
        assert_eq!(clean_name("réseau").unwrap(), "réseau");
    }

    #[test]
    fn separators_become_dashes() {
        assert_eq!(clean_name("a/b").unwrap(), "a-b");
        assert_eq!(clean_name("a\\b").unwrap(), "a-b");
        assert_eq!(clean_name("a:b").unwrap(), "a-b");
        assert_eq!(clean_name("a b").unwrap(), "a-b");
        assert_eq!(clean_name("a\tb").unwrap(), "a-b");
    }

    #[test]
    fn runs_of_separators_collapse() {
        assert_eq!(clean_name("a // b").unwrap(), "a-b");
        assert_eq!(clean_name("a---b").unwrap(), "a-b");
    }

    #[test]
    fn leading_and_trailing_dashes_and_dots_are_dropped() {
        assert_eq!(clean_name("  redis  ").unwrap(), "redis");
        assert_eq!(clean_name("/redis/").unwrap(), "redis");
        assert_eq!(clean_name("--redis--").unwrap(), "redis");
        assert_eq!(clean_name(".redis").unwrap(), "redis");
        assert_eq!(clean_name("redis.git").unwrap(), "redis.git");
        assert_eq!(clean_name("redis.").unwrap(), "redis");
    }

    /// A name that cleans down to nothing would be a project directory with no
    /// name, or worse, the root itself.
    #[test]
    fn a_name_that_is_all_separators_is_rejected() {
        for raw in ["", "   ", "/", "///", "---", "...", "./", "../"] {
            let err = clean_name(raw).unwrap_err().to_string();
            assert!(err.contains("invalid project name"), "{raw:?}: {err}");
        }
    }
}
