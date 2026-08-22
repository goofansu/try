//! Where a new project's contents come from.
//!
//! A source is a remote repository, a pull request, a local repository or a
//! plain directory. Callers only ever ask three things of it: whether an
//! argument looks like one, what name it implies, and to fill a directory.
//! Everything else — URL shapes, host-specific pull request layouts, and every
//! git invocation — stays inside.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::paths;
use crate::store;

/// Where a new project's contents come from.
///
/// Opaque on purpose: which of the four kinds a source turned out to be is
/// this module's business, and callers never branch on it.
pub struct Source(Kind);

enum Kind {
    /// A remote repository to clone.
    Clone(String),
    /// A pull request to clone and check out.
    Request(Request),
    /// A local repository to add a detached worktree of.
    Worktree(PathBuf),
    /// A local directory to symlink.
    Link(PathBuf),
}

/// A GitHub pull request or GitLab merge request, read out of its web URL.
struct Request {
    /// The repository it belongs to.
    repo_url: String,
    /// The remote ref holding its head commit.
    remote_ref: String,
    /// The local branch to create from that ref.
    branch: String,
    /// The derived project name, e.g. "tobi-try-pr-139".
    name: String,
}

impl Source {
    pub fn resolve(arg: &str) -> Result<Self> {
        if let Some(scheme) = url_scheme(arg) {
            if scheme == "file" {
                bail!("point at the path directly rather than through a file:// URL");
            }
            return Ok(match parse_request(arg) {
                Some(request) => Source(Kind::Request(request)),
                None => Source(Kind::Clone(arg.to_string())),
            });
        }
        if is_scp_style(arg) {
            return Ok(Source(Kind::Clone(arg.to_string())));
        }

        let path = paths::expand(arg)?;
        let meta = fs::symlink_metadata(&path).map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => anyhow!("no such path: {arg}"),
            _ => anyhow!(err).context(format!("cannot read {arg}")),
        })?;
        if !fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
            let kind = if meta.is_symlink() { "a broken symlink" } else { "not a directory" };
            bail!("{arg} is {kind}");
        }

        match git::repo_root(&path)? {
            Some(root) => Ok(Source(Kind::Worktree(root))),
            None => Ok(Source(Kind::Link(path))),
        }
    }

    /// The name a source implies when none was typed.
    pub fn name(&self) -> Result<String> {
        match &self.0 {
            Kind::Clone(url) => name_from_repo_url(url),
            Kind::Request(request) => Ok(request.name.clone()),
            // A worktree holds the whole repository, so it is named after the
            // repository root rather than the subdirectory that was pointed at.
            Kind::Worktree(root) | Kind::Link(root) => basename_name(root),
        }
    }

    fn write_into(&self, path: &Path) -> Result<()> {
        match &self.0 {
            Kind::Clone(url) => git::run(
                &format!("git clone {url}"),
                &[OsStr::new("clone"), OsStr::new(url), path.as_os_str()],
            ),
            Kind::Request(request) => checkout(request, path),
            Kind::Worktree(root) => {
                // A project deleted by hand leaves its worktree registered, and
                // git then refuses to reuse that path: "missing but already
                // registered worktree". Clearing the registrations whose
                // directories are gone makes the path available again.
                git::prune_worktrees(root);
                git::run(
                    &format!("git worktree add {}", path.display()),
                    &[
                        OsStr::new("-C"),
                        root.as_os_str(),
                        OsStr::new("worktree"),
                        OsStr::new("add"),
                        OsStr::new("--detach"),
                        path.as_os_str(),
                    ],
                )
            }
            Kind::Link(target) => symlink(target, path)
                .with_context(|| format!("cannot link {} to {}", path.display(), target.display())),
        }
    }

    /// Fills `path` with this source, removing whatever a failure leaves behind
    /// so a half-made project never takes a name.
    pub fn create_at(&self, path: &Path) -> Result<()> {
        if let Err(err) = self.write_into(path) {
            self.discard(path);
            return Err(err);
        }
        Ok(())
    }

    /// Removes whatever a failed attempt left behind, so a half-made project never
    /// takes a name.
    fn discard(&self, path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_dir_all(path);
        if let Kind::Worktree(root) = &self.0 {
            // The worktree was registered against the repository, not this path.
            git::prune_worktrees(root);
        }
    }
}

/// Whether an argument is a source rather than a name.
///
/// Being explicit about paths is what keeps the grammar unambiguous: `src` is
/// always the project `src`, and `./src` is always the directory.
pub fn looks_like_source(arg: &str) -> bool {
    if arg == "." || arg == ".." {
        return true;
    }
    if arg.starts_with("./") || arg.starts_with("../") || arg.starts_with('/') || arg.starts_with('~')
    {
        return true;
    }
    url_scheme(arg).is_some() || is_scp_style(arg)
}

/// Clones the repository a request belongs to, then fetches the request's head
/// into a local branch and switches to it.
///
/// A branch is safe here in a way it is not for a worktree: the clone is fresh,
/// so nothing else has the branch checked out.
fn checkout(request: &Request, path: &Path) -> Result<()> {
    let refspec = format!("{}:{}", request.remote_ref, request.branch);
    git::run(
        &format!("git clone {}", request.repo_url),
        &[
            OsStr::new("clone"),
            OsStr::new(&request.repo_url),
            path.as_os_str(),
        ],
    )?;
    git::run(
        &format!("git fetch {}", request.remote_ref),
        &[
            OsStr::new("-C"),
            path.as_os_str(),
            OsStr::new("fetch"),
            OsStr::new("origin"),
            OsStr::new(&refspec),
        ],
    )?;
    git::run(
        &format!("git switch {}", request.branch),
        &[
            OsStr::new("-C"),
            path.as_os_str(),
            OsStr::new("switch"),
            OsStr::new(&request.branch),
        ],
    )
}

/// The lowercased `scheme` of a `scheme://...` argument.
fn url_scheme(arg: &str) -> Option<String> {
    let end = arg.find("://").filter(|&i| i > 0)?;
    let scheme = &arg[..end];
    scheme
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        .then(|| scheme.to_ascii_lowercase())
}

/// The `user@host:path` form git accepts as a shorthand for ssh.
fn is_scp_style(arg: &str) -> bool {
    let Some((user, rest)) = arg.split_once('@') else {
        return false;
    };
    let Some((host, path)) = rest.split_once(':') else {
        return false;
    };
    !user.is_empty() && !host.is_empty() && !host.contains('/') && !path.is_empty()
}

/// The path segments of a URL, with the host and any query or fragment removed.
fn url_path_segments(url: &str) -> Vec<&str> {
    let rest = if let Some(scheme) = url_scheme(url) {
        let after = &url[scheme.len() + 3..];
        after.split_once('/').map(|(_host, path)| path).unwrap_or("")
    } else if let Some((_user, after_at)) = url.split_once('@') {
        after_at.split_once(':').map(|(_host, path)| path).unwrap_or("")
    } else {
        url
    };
    rest.split(['?', '#'])
        .next()
        .unwrap_or("")
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// The `<user>-<repo>` name a repository URL implies. Including the owner keeps
/// two people's forks of the same project from colliding.
fn name_from_repo_url(url: &str) -> Result<String> {
    let mut segments = url_path_segments(url);
    if let Some(last) = segments.last_mut() {
        *last = last.strip_suffix(".git").unwrap_or(last);
    }
    if segments.is_empty() {
        bail!("cannot work out a project name from {url:?}");
    }
    let tail = &segments[segments.len().saturating_sub(2)..];
    store::clean_name(&tail.join("-")).with_context(|| format!("cannot work out a project name from {url:?}"))
}

/// The name a local path implies: its last component, with a `.git` suffix
/// dropped so a bare repository at `thing.git` becomes the project `thing`.
fn basename_name(path: &Path) -> Result<String> {
    let base = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("cannot work out a project name from {}", path.display()))?;
    store::clean_name(base.strip_suffix(".git").unwrap_or(&base))
}

/// Recognises the web URL of a GitHub pull request or a GitLab merge request.
///
/// These address a page rather than a repository, so git cannot clone them
/// directly: the repository is cloned and the request's head ref fetched.
fn parse_request(arg: &str) -> Option<Request> {
    let scheme = url_scheme(arg)?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = arg[scheme.len() + 3..].split(['?', '#']).next()?;
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();

    // github.com/<owner>/<repo>/pull/<number>
    // gitlab.com/<group>[/<subgroup>…]/<repo>/-/merge_requests/<number>
    //
    // The marker cannot come before the third segment, because a host, an
    // owner and a repository name have to precede it.
    let mut found = None;
    for i in 3..segments.len() {
        if segments[i] == "pull" {
            found = Some((i, i + 1, "pr", "refs/pull"));
            break;
        }
        if segments[i] == "-" && segments.get(i + 1) == Some(&"merge_requests") {
            found = Some((i, i + 2, "mr", "refs/merge-requests"));
            break;
        }
    }
    let (repo_end, number_at, label, ref_base) = found?;
    let number: u32 = segments.get(number_at)?.parse().ok()?;

    let repo_url = format!("{scheme}://{}", segments[..repo_end].join("/"));
    let repo_name = name_from_repo_url(&repo_url).ok()?;
    Some(Request {
        remote_ref: format!("{ref_base}/{number}/head"),
        branch: format!("{label}-{number}"),
        name: format!("{repo_name}-{label}-{number}"),
        repo_url,
    })
}

/// Every git invocation, kept behind one internal seam so the rest of this
/// module never spells out a command line.
mod git {
    use std::ffi::OsStr;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{Result, anyhow, bail};

    /// The root of the repository a path belongs to, or `None` when it is not in
    /// one. A bare repository is its own root.
    pub fn repo_root(path: &Path) -> Result<Option<PathBuf>> {
        if let Some(top) = line(path, "--show-toplevel")? {
            return Ok(Some(PathBuf::from(top)));
        }
        if line(path, "--is-bare-repository")?.as_deref() == Some("true") {
            return Ok(Some(path.to_path_buf()));
        }
        Ok(None)
    }

    /// One line of `git rev-parse`, or `None` when git says no.
    fn line(path: &Path, flag: &str) -> Result<Option<String>> {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("rev-parse")
            .arg(flag)
            .output()
            .map_err(|err| match err.kind() {
                io::ErrorKind::NotFound => anyhow!("git is required, but it is not on PATH"),
                _ => anyhow!(err).context("cannot run git"),
            })?;
        if !output.status.success() {
            return Ok(None);
        }
        let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!line.is_empty()).then_some(line))
    }

    /// Runs git with its output inherited, so its progress bar, credential prompts
    /// and SSH host-key questions all behave normally.
    pub fn run(what: &str, args: &[&OsStr]) -> Result<()> {
        let status = Command::new("git")
            .args(args)
            .status()
            .map_err(|err| match err.kind() {
                io::ErrorKind::NotFound => anyhow!("git is required, but it is not on PATH"),
                _ => anyhow!(err).context("cannot run git"),
            })?;
        match status.code() {
            Some(0) => Ok(()),
            Some(code) => bail!("{what} failed with exit code {code}"),
            None => bail!("{what} was killed by a signal"),
        }
    }

    /// Drops registrations for worktrees whose directories no longer exist. Best
    /// effort and silent: it is housekeeping, not something the user asked for.
    pub fn prune_worktrees(root: &Path) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["worktree", "prune"])
            .output();
    }
}

