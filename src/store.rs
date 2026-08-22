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
