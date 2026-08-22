//! try — find, create and jump into project directories.
//!
//! Every project lives under the root (`$TRY_PATH`, or `~/try`) and is a single
//! directory named after itself. A name is an identity: it maps to exactly one
//! directory, so `try redis` always means the same place.
//!
//! A project can be empty, cloned from a remote repository, checked out from a
//! pull request, or backed by a local directory — but the name is chosen the
//! same way in every case, and an existing project is simply entered.
//!
//! A process cannot change its parent shell's directory, so the chosen path is
//! reported on the descriptor named by `TRY_FD` and the shell function printed
//! by `--init` performs the cd.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, IsTerminal, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, NaiveDate};
use clap::Parser;
use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

/// The picker is drawn inline rather than in the alternate screen, so it is
/// kept to one line per project and no taller than it needs to be.
const MAX_VISIBLE: usize = 10;

/// Rows the picker spends on itself rather than on projects: the query line
/// above the list and the help line below it.
const CHROME: usize = 2;

const AFTER_HELP: &str = "\
Examples:
  try
  try <name>
  try <name> <source>
  try https://<host>/<user>/<repo>.git
  try git@<host>:<user>/<repo>.git
  try https://<host>/<user>/<repo>/pull/<number>
  try ./<path>
  try <name> ./<path>
  try --init fish

See README.md for what each form does.";

#[derive(Parser)]
#[command(
    name = "try",
    version,
    about = "Find, create and jump into project directories.",
    after_help = AFTER_HELP
)]
struct Cli {
    /// Print the project root and exit
    #[arg(long)]
    path: bool,

    /// Print the shell function to eval, for fish, bash or zsh
    #[arg(long, value_name = "SHELL")]
    init: Option<String>,

    /// A project name, a source, or a name followed by a source
    #[arg(value_name = "NAME|SOURCE")]
    args: Vec<String>,
}

fn main() {
    if let Err(err) = run() {
        if err.downcast_ref::<Canceled>().is_some() {
            std::process::exit(130);
        }
        eprintln!("try: {err:#}");
        std::process::exit(1);
    }
}

/// Returned when the user dismisses the picker; `main` turns it into exit 130.
#[derive(Debug)]
struct Canceled;

impl fmt::Display for Canceled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("canceled")
    }
}

impl std::error::Error for Canceled {}

fn run() -> Result<()> {
    let cli = Cli::parse();

    if let Some(shell) = cli.init.as_deref() {
        return print_shell_init(shell);
    }

    let root = root()?;
    if cli.path {
        println!("{}", root.display());
        return Ok(());
    }

    match cli.args.as_slice() {
        [] => browse(&root),
        [only] if is_source(only) => enter(&root, None, Some(only)),
        [only] => enter(&root, Some(only), None),
        [first, second] => {
            if is_source(first) && !is_source(second) {
                bail!(
                    "the source goes last: try `try {second} {first}`\n\
                     \x20     (a name comes first, a URL or path second)"
                );
            }
            if !is_source(second) {
                bail!(
                    "expected a URL or a path as the second argument, but got {second:?}\n\
                     \x20     for a multi-word name, quote it: try \"{first} {second}\""
                );
            }
            enter(&root, Some(first), Some(second))
        }
        _ => bail!(
            "too many arguments: expected a name, a source, or a name and a source\n\
             \x20     for a multi-word name, quote it or join it with dashes"
        ),
    }
}

/// Handles bare `try`: pick from the projects that already exist. Creating
/// is what a name is for, so the picker only ever selects.
fn browse(root: &Path) -> Result<()> {
    let projects = list(root)?;
    if projects.is_empty() {
        bail!(
            "no projects in {} yet — create one with: try <name>",
            root.display()
        );
    }
    let today = Local::now().date_naive();
    let chosen = run_select(&items_for(&projects, today))?;
    output(&projects[chosen].path)
}

/// Handles every naming form. The name is settled first, and an existing
/// project of that name wins outright — the source is not even resolved.
fn enter(root: &Path, name: Option<&str>, source: Option<&str>) -> Result<()> {
    // A typed name always wins, so it is worth resolving the source only when
    // the name has to come from it.
    let (name, source) = match (name, source) {
        (Some(name), None) => (clean_name(name)?, None),
        (Some(name), Some(arg)) => (clean_name(name)?, Some(resolve(arg)?)),
        (None, Some(arg)) => {
            let source = resolve(arg)?;
            (derived_name(&source)?, Some(source))
        }
        (None, None) => unreachable!("run() never calls enter with neither"),
    };

    let path = root.join(&name);
    if fs::symlink_metadata(&path).is_ok() {
        return output(&path);
    }

    fs::create_dir_all(root).with_context(|| format!("cannot create {}", root.display()))?;
    let made = match &source {
        Some(source) => materialize(source, &path),
        None => fs::create_dir(&path).with_context(|| format!("cannot create {}", path.display())),
    };
    if let Err(err) = made {
        discard(&path, source.as_ref());
        return Err(err);
    }

    eprintln!("created {}", tildify(&path));
    output(&path)
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// What a project is filled from.
enum Source {
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

/// Whether an argument is a source rather than a name.
///
/// Being explicit about paths is what keeps the grammar unambiguous: `src` is
/// always the project `src`, and `./src` is always the directory.
fn is_source(arg: &str) -> bool {
    if arg == "." || arg == ".." {
        return true;
    }
    if arg.starts_with("./") || arg.starts_with("../") || arg.starts_with('/') || arg.starts_with('~')
    {
        return true;
    }
    url_scheme(arg).is_some() || is_scp_style(arg)
}

fn resolve(arg: &str) -> Result<Source> {
    if let Some(scheme) = url_scheme(arg) {
        if scheme == "file" {
            bail!("point at the path directly rather than through a file:// URL");
        }
        return Ok(match parse_request(arg) {
            Some(request) => Source::Request(request),
            None => Source::Clone(arg.to_string()),
        });
    }
    if is_scp_style(arg) {
        return Ok(Source::Clone(arg.to_string()));
    }

    let path = expand_home(arg)?;
    let meta = fs::symlink_metadata(&path).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => anyhow!("no such path: {arg}"),
        _ => anyhow!(err).context(format!("cannot read {arg}")),
    })?;
    if !fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
        let kind = if meta.is_symlink() { "a broken symlink" } else { "not a directory" };
        bail!("{arg} is {kind}");
    }

    match repo_root(&path)? {
        Some(root) => Ok(Source::Worktree(root)),
        None => Ok(Source::Link(path)),
    }
}

/// The name a source implies when none was typed.
fn derived_name(source: &Source) -> Result<String> {
    match source {
        Source::Clone(url) => name_from_repo_url(url),
        Source::Request(request) => Ok(request.name.clone()),
        // A worktree holds the whole repository, so it is named after the
        // repository root rather than the subdirectory that was pointed at.
        Source::Worktree(root) | Source::Link(root) => basename_name(root),
    }
}

fn materialize(source: &Source, path: &Path) -> Result<()> {
    match source {
        Source::Clone(url) => git(
            &format!("git clone {url}"),
            &[OsStr::new("clone"), OsStr::new(url), path.as_os_str()],
        ),
        Source::Request(request) => checkout(request, path),
        Source::Worktree(root) => {
            // A project deleted by hand leaves its worktree registered, and
            // git then refuses to reuse that path: "missing but already
            // registered worktree". Clearing the registrations whose
            // directories are gone makes the path available again.
            prune_worktrees(root);
            git(
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
        Source::Link(target) => symlink(target, path)
            .with_context(|| format!("cannot link {} to {}", path.display(), target.display())),
    }
}

/// Clones the repository a request belongs to, then fetches the request's head
/// into a local branch and switches to it.
///
/// A branch is safe here in a way it is not for a worktree: the clone is fresh,
/// so nothing else has the branch checked out.
fn checkout(request: &Request, path: &Path) -> Result<()> {
    let refspec = format!("{}:{}", request.remote_ref, request.branch);
    git(
        &format!("git clone {}", request.repo_url),
        &[
            OsStr::new("clone"),
            OsStr::new(&request.repo_url),
            path.as_os_str(),
        ],
    )?;
    git(
        &format!("git fetch {}", request.remote_ref),
        &[
            OsStr::new("-C"),
            path.as_os_str(),
            OsStr::new("fetch"),
            OsStr::new("origin"),
            OsStr::new(&refspec),
        ],
    )?;
    git(
        &format!("git switch {}", request.branch),
        &[
            OsStr::new("-C"),
            path.as_os_str(),
            OsStr::new("switch"),
            OsStr::new(&request.branch),
        ],
    )
}

/// Removes whatever a failed attempt left behind, so a half-made project never
/// takes a name.
fn discard(path: &Path, source: Option<&Source>) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_dir_all(path);
    if let Some(Source::Worktree(root)) = source {
        // The worktree was registered against the repository, not this path.
        prune_worktrees(root);
    }
}

/// Drops registrations for worktrees whose directories no longer exist. Best
/// effort and silent: it is housekeeping, not something the user asked for.
fn prune_worktrees(root: &Path) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "prune"])
        .output();
}

/// The root of the repository a path belongs to, or `None` when it is not in
/// one. A bare repository is its own root.
fn repo_root(path: &Path) -> Result<Option<PathBuf>> {
    if let Some(top) = git_line(path, "--show-toplevel")? {
        return Ok(Some(PathBuf::from(top)));
    }
    if git_line(path, "--is-bare-repository")?.as_deref() == Some("true") {
        return Ok(Some(path.to_path_buf()));
    }
    Ok(None)
}

/// One line of `git rev-parse`, or `None` when git says no.
fn git_line(path: &Path, flag: &str) -> Result<Option<String>> {
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
fn git(what: &str, args: &[&OsStr]) -> Result<()> {
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

// ---------------------------------------------------------------------------
// URLs and names
// ---------------------------------------------------------------------------

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
    clean_name(&tail.join("-")).with_context(|| format!("cannot work out a project name from {url:?}"))
}

/// The name a local path implies: its last component, with a `.git` suffix
/// dropped so a bare repository at `thing.git` becomes the project `thing`.
fn basename_name(path: &Path) -> Result<String> {
    let base = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("cannot work out a project name from {}", path.display()))?;
    clean_name(base.strip_suffix(".git").unwrap_or(&base))
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

/// Turns user input into a directory-safe project name: separators become
/// dashes, runs of dashes collapse, and leading or trailing dashes and dots
/// are dropped.
fn clean_name(raw: &str) -> Result<String> {
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

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

/// A single directory under the try root.
struct Project {
    name: String,
    path: PathBuf,
    mtime: Option<SystemTime>,
}

/// The directory that holds all projects: `$TRY_PATH` if set, otherwise
/// `~/try`. It is not required to exist yet.
fn root() -> Result<PathBuf> {
    if let Ok(raw) = std::env::var("TRY_PATH") {
        let raw = raw.trim();
        if !raw.is_empty() {
            return expand_home(raw);
        }
    }
    Ok(home()?.join("try"))
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot determine home directory: HOME is unset"))
}

fn expand_home(raw: &str) -> Result<PathBuf> {
    let expanded = if raw == "~" {
        home()?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home()?.join(rest)
    } else {
        PathBuf::from(raw)
    };
    std::path::absolute(&expanded).with_context(|| format!("cannot resolve {expanded:?}"))
}

/// Every project under root, most recently modified first.
fn list(root: &Path) -> Result<Vec<Project>> {
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

// ---------------------------------------------------------------------------
// Reporting the destination
// ---------------------------------------------------------------------------

/// Reports the destination for the shell function to consume.
///
/// It goes to the descriptor named by TRY_FD when the shell function provides
/// one, which keeps it off stdout — otherwise `--path` would look like a
/// directory to jump to rather than something to print. With no TRY_FD there
/// is no shell function listening, so it falls back to stdout and says so.
fn output(path: &Path) -> Result<()> {
    match destination() {
        Some(file) => {
            let mut sink = &*file;
            writeln!(sink, "{}", path.display()).context("cannot report the chosen directory")?;
        }
        None => {
            println!("{}", path.display());
            if io::stdout().is_terminal() {
                eprintln!("note: shell integration is not active, so this shell stayed put.");
                eprintln!("      run `try --init fish` to see how to enable it.");
            }
        }
    }
    Ok(())
}

/// Resolves TRY_FD to an open file, or `None` when it is unusable. The handle
/// is never closed: the descriptor belongs to the caller.
fn destination() -> Option<ManuallyDrop<File>> {
    let raw = std::env::var("TRY_FD").ok()?;
    let fd: i32 = raw.trim().parse().ok()?;
    if fd < 0 {
        return None;
    }
    // SAFETY: the descriptor is owned by the caller and wrapped in a
    // ManuallyDrop, so this File never closes it.
    let file = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
    file.metadata().ok()?;
    Some(file)
}

/// Shortens a path under the home directory for display. The prefix has to end
/// on a path boundary, so /Users/jamie is not shortened for /Users/james.
fn tildify(path: &Path) -> String {
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

// ---------------------------------------------------------------------------
// Shell integration
// ---------------------------------------------------------------------------

/// Each function shadows the binary it calls, which is fine: fish's
/// `command -s` and the POSIX `command` builtin both skip functions, so there
/// is no recursion.
const FISH_INIT: &str = r#"# try shell integration — add to ~/.config/fish/config.fish:
#   try --init fish | source
function try --description "Find, create and jump into projects"
    set -l bin (command -s try)
    if test -z "$bin"
        echo "try: the try binary is not on PATH" >&2
        return 127
    end
    set -l dest (mktemp)
    env TRY_FD=3 $bin $argv 3>$dest
    set -l code $status
    set -l dir (cat $dest 2>/dev/null)
    rm -f $dest
    if test $code -ne 0
        return $code
    end
    if test -n "$dir" -a -d "$dir"
        cd $dir
    end
    return 0
end
"#;

const POSIX_INIT: &str = r#"# try shell integration — add to ~/.bashrc or ~/.zshrc:
#   eval "$(try --init {shell})"
try() {
  local dest dir code
  dest="$(mktemp)" || return $?
  TRY_FD=3 command try "$@" 3>"$dest"
  code=$?
  dir="$(cat "$dest" 2>/dev/null)"
  rm -f "$dest"
  if [ "$code" -ne 0 ]; then
    return "$code"
  fi
  if [ -n "$dir" ] && [ -d "$dir" ]; then
    cd "$dir" || return $?
  fi
  return 0
}
"#;

fn print_shell_init(shell: &str) -> Result<()> {
    let name = Path::new(shell)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match name.as_str() {
        "fish" => {
            print!("{FISH_INIT}");
            Ok(())
        }
        "bash" | "zsh" => {
            print!("{}", POSIX_INIT.replace("{shell}", &name));
            Ok(())
        }
        _ => bail!("cannot generate shell integration for {shell:?}; supported: fish, bash, zsh"),
    }
}

// ---------------------------------------------------------------------------
// Picker
// ---------------------------------------------------------------------------

/// One row of the picker.
struct Item {
    /// Main text, e.g. "tobi-try".
    label: String,
    /// Secondary text, e.g. "3 days ago".
    hint: String,
}

fn items_for(projects: &[Project], today: NaiveDate) -> Vec<Item> {
    projects
        .iter()
        .map(|p| Item {
            label: p.name.clone(),
            hint: age(p, today),
        })
        .collect()
}

fn age(project: &Project, today: NaiveDate) -> String {
    let Some(date) = project
        .mtime
        .map(|t| DateTime::<Local>::from(t).date_naive())
    else {
        return String::new();
    };
    let days = (today - date).num_days();
    match days {
        i64::MIN..=0 => "today".to_string(),
        1 => "yesterday".to_string(),
        2..=29 => format!("{days} days ago"),
        30..=364 => plural(days / 30, "month"),
        _ => plural(days / 365, "year"),
    }
}

fn plural(n: i64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{n} {unit}s ago")
    }
}

/// Shows the picker and returns the index of the chosen item.
///
/// The UI is drawn on stderr so that stdout stays free for ordinary output,
/// and input comes from /dev/tty, so both still work when the caller has
/// redirected either one.
fn run_select(items: &[Item]) -> Result<usize> {
    if !io::stderr().is_terminal() {
        bail!("no terminal available for the picker");
    }

    let visible = items.len().clamp(1, MAX_VISIBLE);
    let height = u16::try_from(visible + CHROME).unwrap_or(u16::MAX);

    enable_raw_mode().context("cannot put the terminal in raw mode")?;
    let backend = CrosstermBackend::new(io::stderr());
    let mut term = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .context("cannot start the picker");

    let result = match term.as_mut() {
        Ok(term) => select_loop(term, items),
        Err(_) => Err(anyhow!("cannot start the picker")),
    };

    if let Ok(term) = term.as_mut() {
        // `Terminal::clear` puts the cursor back where the last draw left it,
        // which is the bottom of the viewport — the cleared rows would then sit
        // above the next shell prompt as blank lines. Park the cursor on the
        // viewport's first row instead, so the prompt reclaims them.
        let top = term.get_frame().area().y;
        let _ = execute!(
            io::stderr(),
            MoveTo(0, top),
            Clear(ClearType::FromCursorDown),
            Show
        );
    }
    let _ = disable_raw_mode();
    result
}

fn select_loop(
    term: &mut Terminal<CrosstermBackend<io::Stderr>>,
    items: &[Item],
) -> Result<usize> {
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut query = String::new();
    let mut order = filter(items, &query, &mut matcher);
    let mut selected = 0usize;
    let mut offset = 0usize;
    let width = label_width(items);

    loop {
        let chrome = u16::try_from(CHROME).unwrap_or(u16::MAX);
        let rows = usize::from(term.get_frame().area().height.saturating_sub(chrome)).max(1);
        if selected < offset {
            offset = selected;
        } else if selected >= offset + rows {
            offset = selected + 1 - rows;
        }

        term.draw(|frame| {
            render(frame, &query, items, &order, selected, offset, width);
        })?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            KeyCode::Char('c') if ctrl => return Err(Canceled.into()),
            KeyCode::Esc => {
                if query.is_empty() {
                    return Err(Canceled.into());
                }
                query.clear();
                order = filter(items, &query, &mut matcher);
                selected = 0;
                offset = 0;
            }
            KeyCode::Enter => {
                if let Some(hit) = order.get(selected) {
                    return Ok(hit.index);
                }
            }
            // Only the control-prefixed forms navigate: with type-to-filter a
            // bare `j` or `n` has to reach the query like any other letter.
            // In raw mode crossterm reports Ctrl+J as Char('j'), not as Enter,
            // so binding it here does not shadow select.
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Char('p' | 'k') if ctrl => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = next(selected, order.len()),
            KeyCode::Char('n' | 'j') if ctrl => selected = next(selected, order.len()),
            KeyCode::Backspace => {
                query.pop();
                order = filter(items, &query, &mut matcher);
                selected = 0;
                offset = 0;
            }
            KeyCode::Char('u') if ctrl => {
                query.clear();
                order = filter(items, &query, &mut matcher);
                selected = 0;
                offset = 0;
            }
            KeyCode::Char(ch) if !ctrl && !alt => {
                query.push(ch);
                order = filter(items, &query, &mut matcher);
                selected = 0;
                offset = 0;
            }
            _ => {}
        }
    }
}

/// The row below `selected`, stopping at the end of the list.
fn next(selected: usize, len: usize) -> usize {
    if selected + 1 < len { selected + 1 } else { selected }
}

/// A row that survived the filter, with the character positions the query
/// matched so they can be picked out when the row is drawn.
struct Hit {
    index: usize,
    positions: Vec<u32>,
}

/// The visible rows, in order: everything when the query is empty, otherwise
/// the fuzzy matches ranked by score.
fn filter(items: &[Item], query: &str, matcher: &mut Matcher) -> Vec<Hit> {
    if query.is_empty() {
        return (0..items.len())
            .map(|index| Hit {
                index,
                positions: Vec::new(),
            })
            .collect();
    }

    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut positions = Vec::new();
    let mut scored: Vec<(Hit, u32)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            positions.clear();
            let score = pattern.indices(
                Utf32Str::new(&item.label, &mut buf),
                matcher,
                &mut positions,
            )?;
            positions.sort_unstable();
            positions.dedup();
            let hit = Hit {
                index,
                positions: positions.clone(),
            };
            Some((hit, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.index.cmp(&b.0.index)));
    scored.into_iter().map(|(hit, _)| hit).collect()
}

/// The column the hints line up on. Rows without a hint do not widen it.
fn label_width(items: &[Item]) -> usize {
    items
        .iter()
        .filter(|item| !item.hint.is_empty())
        .map(|item| item.label.chars().count())
        .max()
        .unwrap_or(0)
}

/// Catppuccin Mocha, with the role names and values herdr uses in
/// `~/repos/herdr/src/app/state.rs` (`Palette::catppuccin`).
///
/// Pinned to the flavour's own values rather than ANSI names, so the picker is
/// the same under tmux, over ssh, and in a terminal set to something else.
/// tmux passes these through: term.nix sets `terminal-features *:RGB`.
mod theme {
    use ratatui::style::Color;

    /// `text` — the rows you are not on
    pub const TEXT: Color = Color::Rgb(0xcd, 0xd6, 0xf4);
    /// `subtext0` — subdued text, i.e. the age column
    pub const SUBTEXT: Color = Color::Rgb(0xa6, 0xad, 0xc8);
    /// `overlay0` — muted chrome: the prompt and the help line
    pub const OVERLAY: Color = Color::Rgb(0x6c, 0x70, 0x86);
    /// `yellow` — the characters the query actually hit
    pub const YELLOW: Color = Color::Rgb(0xf9, 0xe2, 0xaf);
    /// `surface0` — herdr's surface for selected and focused items
    pub const SURFACE: Color = Color::Rgb(0x31, 0x32, 0x44);
    /// `accent` (blue) — the text of the row you are on
    pub const ACCENT: Color = Color::Rgb(0x89, 0xb4, 0xfa);
}

/// The palette, kept deliberately small.
///
/// The selection is a background bar in the theme's own selected-item surface,
/// with its text in the accent; weight and the pointer say the same thing
/// again, so the picker still reads with colour turned off. Matched characters
/// are the only other saturated colour on screen, because they are the only
/// thing colour tells you that position and weight cannot.
const DIM: Style = Style::new().fg(theme::OVERLAY);
const MUTED: Style = Style::new().fg(theme::SUBTEXT);
const BOLD: Style = Style::new()
    .fg(theme::TEXT)
    .add_modifier(Modifier::BOLD);
const TEXT: Style = Style::new().fg(theme::TEXT);
const MATCH: Style = Style::new()
    .fg(theme::YELLOW)
    .add_modifier(Modifier::BOLD);
const SELECTED: Style = Style::new().bg(theme::SURFACE);
const SELECTED_FG: Style = Style::new()
    .fg(theme::ACCENT)
    .add_modifier(Modifier::BOLD);

#[allow(clippy::too_many_arguments)]
fn render(
    frame: &mut Frame,
    query: &str,
    items: &[Item],
    order: &[Hit],
    selected: usize,
    offset: usize,
    width: usize,
) {
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // The top line is an input line and nothing else: the query sits where you
    // type it. There is no matched-out-of-total count, because the list is
    // right there — a count would spend a row saying what you can already see.
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" > ", DIM),
            Span::styled(query.to_string(), BOLD),
        ])),
        head,
    );

    let rows = usize::from(body.height);
    let lines: Vec<Line> = if order.is_empty() {
        vec![Line::from(Span::styled("   no matches", DIM))]
    } else {
        order
            .iter()
            .enumerate()
            .skip(offset)
            .take(rows)
            .map(|(position, hit)| {
                row(&items[hit.index], position == selected, width, &hit.positions)
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), body);

    // The bar is painted after the rows so it spans the full width rather than
    // stopping where the text does. Setting only a background patches it in
    // without disturbing the foregrounds already there.
    if !order.is_empty()
        && let Some(offset_row) = selected.checked_sub(offset)
        && let Ok(offset_row) = u16::try_from(offset_row)
        && offset_row < body.height
    {
        let bar = Rect {
            y: body.y + offset_row,
            height: 1,
            ..body
        };
        frame.buffer_mut().set_style(bar, SELECTED);
    }

    let help = " type to filter · ↑↓ move · enter select · esc clear/cancel";
    frame.render_widget(Paragraph::new(Line::from(Span::styled(help, DIM))), foot);
}

fn row(item: &Item, selected: bool, width: usize, positions: &[u32]) -> Line<'static> {
    let (marker, base) = if selected {
        (" > ", SELECTED_FG)
    } else {
        ("   ", TEXT)
    };

    let mut spans = vec![Span::styled(marker, base)];
    spans.extend(highlight(&item.label, positions, base));
    if !item.hint.is_empty() {
        let pad = width.saturating_sub(item.label.chars().count());
        spans.push(Span::raw(" ".repeat(pad + 2)));
        spans.push(Span::styled(item.hint.clone(), MUTED));
    }
    Line::from(spans)
}

/// Splits a label into runs of matched and unmatched characters, so the part
/// the query actually hit stands out.
fn highlight(label: &str, positions: &[u32], base: Style) -> Vec<Span<'static>> {
    if positions.is_empty() {
        return vec![Span::styled(label.to_string(), base)];
    }

    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_matched = false;
    for (i, ch) in label.chars().enumerate() {
        let matched = positions.binary_search(&(i as u32)).is_ok();
        if matched != run_matched && !run.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut run),
                if run_matched { base.patch(MATCH) } else { base },
            ));
        }
        run_matched = matched;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(
            run,
            if run_matched { base.patch(MATCH) } else { base },
        ));
    }
    spans
}
