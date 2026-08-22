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


#[cfg(test)]
mod tests {
    use super::*;

    use std::process::Command;

    use crate::testing::{TempDir, git_init, git_init_bare, has_git};

    /// The message from an argument the resolver is expected to turn away.
    fn resolve_err(arg: &str) -> String {
        match Source::resolve(arg) {
            Ok(_) => panic!("expected {arg} to be rejected"),
            Err(err) => err.to_string(),
        }
    }

    /// One line of git output, for checking what a source actually built.
    fn git_says(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .expect("cannot run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn a_url_scheme_is_recognised_and_lowercased() {
        assert_eq!(url_scheme("https://example.com/x").as_deref(), Some("https"));
        assert_eq!(url_scheme("HTTPS://example.com/x").as_deref(), Some("https"));
        assert_eq!(url_scheme("git+ssh://example.com").as_deref(), Some("git+ssh"));
        assert_eq!(url_scheme("file:///tmp/x").as_deref(), Some("file"));
    }

    #[test]
    fn things_that_are_not_url_schemes() {
        assert_eq!(url_scheme("://example.com"), None, "empty scheme");
        assert_eq!(url_scheme("redis"), None);
        assert_eq!(url_scheme("./redis"), None);
        assert_eq!(url_scheme("my scheme://x"), None, "a space is not allowed");
        assert_eq!(url_scheme("git@github.com:o/r.git"), None);
    }

    #[test]
    fn the_scp_shorthand_is_recognised() {
        assert!(is_scp_style("git@github.com:goofansu/try.git"));
        assert!(is_scp_style("user@host:path"));
    }

    #[test]
    fn things_that_are_not_the_scp_shorthand() {
        assert!(!is_scp_style("git@github.com"), "no colon");
        assert!(!is_scp_style("@github.com:o/r"), "no user");
        assert!(!is_scp_style("git@:o/r"), "no host");
        assert!(!is_scp_style("git@github.com:"), "no path");
        assert!(
            !is_scp_style("https://user@github.com/o/r"),
            "a slash in the host means it is a URL, not the shorthand"
        );
    }

    /// Being explicit is what keeps the grammar unambiguous: `src` is the
    /// project src, and `./src` is the directory.
    #[test]
    fn a_bare_word_is_a_name_not_a_source() {
        for arg in ["redis", "src", "my-notes", "try.git", "..."] {
            assert!(!looks_like_source(arg), "{arg}");
        }
    }

    #[test]
    fn paths_and_urls_are_sources() {
        for arg in [
            ".",
            "..",
            "./src",
            "../sibling",
            "/etc",
            "~",
            "~/code",
            "https://github.com/goofansu/try.git",
            "git@github.com:goofansu/try.git",
        ] {
            assert!(looks_like_source(arg), "{arg}");
        }
    }

    #[test]
    fn url_paths_drop_the_host_and_anything_after_it() {
        assert_eq!(
            url_path_segments("https://github.com/goofansu/try.git"),
            ["goofansu", "try.git"]
        );
        assert_eq!(
            url_path_segments("https://github.com/goofansu/try/pull/123?tab=x#note"),
            ["goofansu", "try", "pull", "123"]
        );
        assert_eq!(
            url_path_segments("git@github.com:goofansu/try.git"),
            ["goofansu", "try.git"]
        );
        assert_eq!(
            url_path_segments("https://example.com//a//b/"),
            ["a", "b"],
            "empty segments are dropped"
        );
        assert!(url_path_segments("https://example.com").is_empty());
    }

    /// Including the owner keeps two people's forks apart.
    #[test]
    fn a_repository_url_is_named_after_its_owner_and_repository() {
        for url in [
            "https://github.com/goofansu/try.git",
            "https://github.com/goofansu/try",
            "http://github.com/goofansu/try/",
            "git@github.com:goofansu/try.git",
            "ssh://git@github.com/goofansu/try.git",
        ] {
            assert_eq!(name_from_repo_url(url).unwrap(), "goofansu-try", "{url}");
        }
    }

    #[test]
    fn a_nested_repository_url_keeps_only_the_last_two_segments() {
        assert_eq!(
            name_from_repo_url("https://gitlab.com/group/subgroup/thing.git").unwrap(),
            "subgroup-thing"
        );
    }

    #[test]
    fn a_repository_url_with_one_segment_is_named_after_it() {
        assert_eq!(
            name_from_repo_url("https://example.com/thing.git").unwrap(),
            "thing"
        );
    }

    #[test]
    fn a_url_with_no_name_in_it_is_rejected() {
        for url in ["https://example.com", "https://example.com/", "https://example.com/.git"] {
            let err = name_from_repo_url(url).unwrap_err().to_string();
            assert!(err.contains("cannot work out a project name"), "{url}: {err}");
        }
    }

    #[test]
    fn a_local_path_is_named_after_its_last_component() {
        assert_eq!(basename_name(Path::new("/code/try")).unwrap(), "try");
        assert_eq!(
            basename_name(Path::new("/code/my project")).unwrap(),
            "my-project"
        );
    }

    /// A bare repository at thing.git is the project thing.
    #[test]
    fn a_bare_repository_path_loses_its_git_suffix() {
        assert_eq!(basename_name(Path::new("/code/thing.git")).unwrap(), "thing");
    }

    #[test]
    fn a_path_with_no_last_component_is_rejected() {
        assert!(basename_name(Path::new("/")).is_err());
    }

    #[test]
    fn a_github_pull_request_url_is_understood() {
        let request = parse_request("https://github.com/goofansu/try/pull/123").unwrap();
        assert_eq!(request.repo_url, "https://github.com/goofansu/try");
        assert_eq!(request.remote_ref, "refs/pull/123/head");
        assert_eq!(request.branch, "pr-123");
        assert_eq!(request.name, "goofansu-try-pr-123");
    }

    #[test]
    fn a_pull_request_url_may_carry_a_tab_or_a_fragment() {
        for url in [
            "https://github.com/goofansu/try/pull/123/files",
            "https://github.com/goofansu/try/pull/123?w=1",
            "https://github.com/goofansu/try/pull/123#issuecomment-1",
        ] {
            let request = parse_request(url).expect(url);
            assert_eq!(request.name, "goofansu-try-pr-123", "{url}");
            assert_eq!(request.repo_url, "https://github.com/goofansu/try", "{url}");
        }
    }

    #[test]
    fn a_gitlab_merge_request_url_is_understood() {
        let request = parse_request("https://gitlab.com/group/thing/-/merge_requests/7").unwrap();
        assert_eq!(request.repo_url, "https://gitlab.com/group/thing");
        assert_eq!(request.remote_ref, "refs/merge-requests/7/head");
        assert_eq!(request.branch, "mr-7");
        assert_eq!(request.name, "group-thing-mr-7");
    }

    #[test]
    fn a_merge_request_in_a_subgroup_is_understood() {
        let request =
            parse_request("https://gitlab.com/group/sub/thing/-/merge_requests/7").unwrap();
        assert_eq!(request.repo_url, "https://gitlab.com/group/sub/thing");
        assert_eq!(request.name, "sub-thing-mr-7");
    }

    #[test]
    fn things_that_are_not_requests() {
        for url in [
            "https://github.com/goofansu/try.git",
            "https://github.com/goofansu/try/pull",
            "https://github.com/goofansu/try/pull/latest",
            "https://github.com/goofansu/try/pull/-1",
            "https://github.com/pull/123",
            "https://gitlab.com/group/thing/-/issues/7",
            "ssh://github.com/goofansu/try/pull/123",
            "git@github.com:goofansu/try.git",
        ] {
            assert!(parse_request(url).is_none(), "{url}");
        }
    }

    #[test]
    fn a_repository_url_resolves_to_a_clone() {
        let source = Source::resolve("https://github.com/goofansu/try.git").unwrap();
        assert!(matches!(source.0, Kind::Clone(_)));
        assert_eq!(source.name().unwrap(), "goofansu-try");

        let source = Source::resolve("git@github.com:goofansu/try.git").unwrap();
        assert!(matches!(source.0, Kind::Clone(_)));
        assert_eq!(source.name().unwrap(), "goofansu-try");
    }

    #[test]
    fn a_pull_request_url_resolves_to_a_request() {
        let source = Source::resolve("https://github.com/goofansu/try/pull/9").unwrap();
        assert!(matches!(source.0, Kind::Request(_)));
        assert_eq!(source.name().unwrap(), "goofansu-try-pr-9");
    }

    /// git accepts file:// URLs, but they mean a clone rather than the
    /// worktree or symlink a local path gets, which is not what someone
    /// pointing at a directory means.
    #[test]
    fn a_file_url_is_turned_away() {
        let err = resolve_err("file:///tmp/x");
        assert!(err.contains("point at the path directly"), "{err}");
    }

    #[test]
    fn a_path_that_is_not_there_says_so() {
        let err = resolve_err("/nowhere/at/all");
        assert!(err.contains("no such path"), "{err}");
    }

    #[test]
    fn a_file_is_not_a_source() {
        let dir = TempDir::new("not-a-dir");
        let file = dir.file("notes.txt", "hello");
        let err = resolve_err(file.to_str().unwrap());
        assert!(err.contains("is not a directory"), "{err}");
    }

    #[test]
    fn a_broken_symlink_is_not_a_source() {
        let dir = TempDir::new("broken");
        let link = dir.join("ghost");
        symlink("/nowhere/at/all", &link).unwrap();
        let err = resolve_err(link.to_str().unwrap());
        assert!(err.contains("is a broken symlink"), "{err}");
    }

    #[test]
    fn a_plain_directory_resolves_to_a_link() {
        if !has_git() {
            return;
        }
        let dir = TempDir::new("plain");
        let target = dir.dir("scratch");

        let source = Source::resolve(target.to_str().unwrap()).unwrap();
        assert!(matches!(source.0, Kind::Link(_)));
        assert_eq!(source.name().unwrap(), "scratch");

        let dest = dir.join("linked");
        source.create_at(&dest).unwrap();
        assert!(fs::symlink_metadata(&dest).unwrap().is_symlink());
        assert_eq!(fs::read_link(&dest).unwrap(), target);
    }

    #[test]
    fn a_repository_resolves_to_a_detached_worktree() {
        if !has_git() {
            return;
        }
        let dir = TempDir::new("worktree");
        let repo = dir.join("myrepo");
        git_init(&repo);

        let source = Source::resolve(repo.to_str().unwrap()).unwrap();
        assert!(matches!(source.0, Kind::Worktree(_)));
        assert_eq!(source.name().unwrap(), "myrepo");

        let dest = dir.join("checkout");
        source.create_at(&dest).unwrap();
        assert!(dest.join(".git").exists(), "the worktree is checked out");
        assert_eq!(
            git_says(&dest, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "HEAD",
            "the worktree is detached, so the repository keeps its branch"
        );
    }

    /// A worktree holds the whole repository, so pointing at a subdirectory
    /// still means the repository, and the name comes from its root.
    #[test]
    fn a_subdirectory_resolves_to_the_repository_that_holds_it() {
        if !has_git() {
            return;
        }
        let dir = TempDir::new("subdir");
        let repo = dir.join("myrepo");
        git_init(&repo);
        let inner = repo.join("src");
        fs::create_dir_all(&inner).unwrap();

        let source = Source::resolve(inner.to_str().unwrap()).unwrap();
        assert!(matches!(&source.0, Kind::Worktree(root) if root == &repo));
        assert_eq!(source.name().unwrap(), "myrepo");
    }

    #[test]
    fn a_bare_repository_is_its_own_root() {
        if !has_git() {
            return;
        }
        let dir = TempDir::new("bare");
        let repo = dir.join("myrepo.git");
        git_init_bare(&repo);

        let source = Source::resolve(repo.to_str().unwrap()).unwrap();
        assert!(matches!(&source.0, Kind::Worktree(root) if root == &repo));
        assert_eq!(source.name().unwrap(), "myrepo");
    }

    /// Deleting a project by hand leaves its worktree registered, and git then
    /// refuses to reuse the path. The next attempt has to succeed anyway.
    #[test]
    fn a_worktree_can_be_made_again_after_its_directory_was_deleted() {
        if !has_git() {
            return;
        }
        let dir = TempDir::new("reuse");
        let repo = dir.join("myrepo");
        git_init(&repo);
        let source = Source::resolve(repo.to_str().unwrap()).unwrap();

        let dest = dir.join("checkout");
        source.create_at(&dest).unwrap();
        fs::remove_dir_all(&dest).unwrap();

        source.create_at(&dest).unwrap();
        assert!(dest.join(".git").exists());
    }

    #[test]
    fn a_clone_copies_the_repository() {
        if !has_git() {
            return;
        }
        let dir = TempDir::new("clone");
        let repo = dir.join("myrepo");
        git_init(&repo);

        let source = Source(Kind::Clone(repo.to_str().unwrap().to_string()));
        let dest = dir.join("copy");
        source.create_at(&dest).unwrap();
        assert!(dest.join(".git").is_dir());
    }

    /// The end of the pull request path, with a local repository standing in
    /// for the forge: clone, fetch the request's head, switch to it.
    #[test]
    fn a_request_is_cloned_then_switched_to_its_branch() {
        if !has_git() {
            return;
        }
        let dir = TempDir::new("request");
        let repo = dir.join("myrepo");
        git_init(&repo);
        // The ref a forge would publish the request's head on.
        git_says(&repo, &["update-ref", "refs/pull/7/head", "HEAD"]);

        let source = Source(Kind::Request(Request {
            repo_url: repo.to_str().unwrap().to_string(),
            remote_ref: "refs/pull/7/head".to_string(),
            branch: "pr-7".to_string(),
            name: "myrepo-pr-7".to_string(),
        }));
        assert_eq!(source.name().unwrap(), "myrepo-pr-7");

        let dest = dir.join("checkout");
        source.create_at(&dest).unwrap();
        assert_eq!(
            git_says(&dest, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "pr-7"
        );
    }

    /// A half-made project must not take a name, so a failed attempt leaves
    /// nothing behind.
    #[test]
    fn a_failed_clone_leaves_nothing_behind() {
        if !has_git() {
            return;
        }
        let dir = TempDir::new("clone-fail");
        let missing = dir.join("nowhere.git");
        let source = Source(Kind::Clone(missing.to_str().unwrap().to_string()));

        let dest = dir.join("copy");
        assert!(source.create_at(&dest).is_err());
        assert!(!dest.exists());
    }

    #[test]
    fn discarding_removes_a_file_or_a_directory() {
        let dir = TempDir::new("discard");
        let source = Source(Kind::Link(PathBuf::from("/nowhere")));

        let file = dir.file("leftover", "x");
        source.discard(&file);
        assert!(!file.exists());

        let subdir = dir.dir("half-cloned");
        fs::write(subdir.join("inner"), "x").unwrap();
        source.discard(&subdir);
        assert!(!subdir.exists());

        source.discard(&dir.join("never-existed"));
    }
}
