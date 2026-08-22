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
