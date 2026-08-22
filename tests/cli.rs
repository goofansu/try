//! The command as a user meets it: the argument grammar, the exit codes, and
//! the descriptor protocol the shell function relies on.
//!
//! `run` reads the real command line, so the only honest way to test it is to
//! run the real binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A directory under the system temporary directory, removed on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let unique = format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNT.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(format!("try-cli-{label}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        Self {
            path: fs::canonicalize(&path).unwrap(),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The whole world the binary runs in: a home directory and a project root
/// inside it, both throwaway.
struct World {
    home: TempDir,
}

impl World {
    fn new(label: &str) -> Self {
        Self {
            home: TempDir::new(label),
        }
    }

    fn root(&self) -> PathBuf {
        self.home.join("projects")
    }

    /// Runs the binary with TRY_PATH pointing at this world's root.
    fn run(&self, args: &[&str]) -> Run {
        self.command(args).env("TRY_PATH", self.root()).output()
    }

    fn command(&self, args: &[&str]) -> Cmd {
        let mut command = Command::new(env!("CARGO_BIN_EXE_try"));
        command
            .args(args)
            .env("HOME", self.home.path())
            .env_remove("TRY_PATH")
            .env_remove("TRY_FD");
        Cmd(command)
    }
}

struct Cmd(Command);

impl Cmd {
    fn env(mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        self.0.env(key, value);
        self
    }

    fn output(mut self) -> Run {
        Run(self.0.output().expect("cannot run the try binary"))
    }
}

struct Run(Output);

impl Run {
    fn ok(&self) -> &Self {
        assert!(
            self.0.status.success(),
            "expected success, got {:?}\nstderr: {}",
            self.0.status.code(),
            self.err()
        );
        self
    }

    fn failed(&self) -> &Self {
        assert_eq!(self.0.status.code(), Some(1), "stderr: {}", self.err());
        self
    }

    fn out(&self) -> String {
        String::from_utf8_lossy(&self.0.stdout).to_string()
    }

    fn err(&self) -> String {
        String::from_utf8_lossy(&self.0.stderr).to_string()
    }

    fn out_line(&self) -> String {
        self.out().trim_end().to_string()
    }
}

fn has_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[test]
fn path_prints_the_root() {
    let world = World::new("path");
    let run = world.run(&["--path"]);
    run.ok();
    assert_eq!(run.out_line(), world.root().display().to_string());
}

/// Without TRY_PATH the root is ~/try, and printing it must not create it.
#[test]
fn the_root_defaults_to_try_under_home() {
    let world = World::new("default-root");
    let run = world.command(&["--path"]).output();
    run.ok();
    assert_eq!(
        run.out_line(),
        world.home.join("try").display().to_string()
    );
    assert!(!world.home.join("try").exists());
}

#[test]
fn init_prints_a_shell_function() {
    let world = World::new("init");

    let fish = world.run(&["--init", "fish"]);
    fish.ok();
    assert!(fish.out().contains("function try"), "{}", fish.out());

    for shell in ["bash", "zsh"] {
        let run = world.run(&["--init", shell]);
        run.ok();
        assert!(run.out().contains("try() {"), "{}", run.out());
        assert!(
            run.out().contains(&format!("try --init {shell}")),
            "{}",
            run.out()
        );
    }
}

#[test]
fn init_refuses_a_shell_it_cannot_write() {
    let world = World::new("init-bad");
    let run = world.run(&["--init", "nushell"]);
    run.failed();
    assert!(run.err().starts_with("try: "), "{}", run.err());
    assert!(run.err().contains("fish, bash, zsh"), "{}", run.err());
}

#[test]
fn a_name_creates_a_project_and_reports_it() {
    let world = World::new("create");
    let run = world.run(&["notes"]);
    run.ok();

    let project = world.root().join("notes");
    assert!(project.is_dir(), "the project directory is created");
    assert_eq!(run.out_line(), project.display().to_string());
    assert!(run.err().contains("created"), "{}", run.err());
}

/// A name is an identity: the second time it is the same directory, and
/// nothing is created.
#[test]
fn the_same_name_enters_the_same_project() {
    let world = World::new("reenter");
    world.run(&["notes"]).ok();
    let again = world.run(&["notes"]);
    again.ok();

    assert_eq!(
        again.out_line(),
        world.root().join("notes").display().to_string()
    );
    assert_eq!(again.err(), "", "nothing to report: no source was given");
}

#[test]
fn a_typed_name_is_cleaned_up() {
    let world = World::new("clean");
    let run = world.run(&["My Notes"]);
    run.ok();
    assert!(world.root().join("My-Notes").is_dir());
    assert_eq!(
        run.out_line(),
        world.root().join("My-Notes").display().to_string()
    );
}

#[test]
fn a_name_that_cleans_down_to_nothing_is_refused() {
    let world = World::new("empty-name");
    let run = world.run(&["..."]);
    run.failed();
    assert!(run.err().contains("invalid project name"), "{}", run.err());
}

/// The name comes first and the source second. Getting it backwards is a
/// common slip, so it is named rather than reported as a bad name.
#[test]
fn a_source_typed_first_is_pointed_the_right_way_round() {
    let world = World::new("order");
    let run = world.run(&["./", "patch"]);
    run.failed();
    assert!(run.err().contains("the source goes last"), "{}", run.err());
    assert!(run.err().contains("try patch ./"), "{}", run.err());
}

#[test]
fn two_names_suggest_quoting() {
    let world = World::new("two-names");
    let run = world.run(&["my", "notes"]);
    run.failed();
    assert!(
        run.err().contains("expected a URL or a path"),
        "{}",
        run.err()
    );
    assert!(run.err().contains(r#"try "my notes""#), "{}", run.err());
}

#[test]
fn three_arguments_are_too_many() {
    let world = World::new("three");
    let run = world.run(&["a", "b", "c"]);
    run.failed();
    assert!(run.err().contains("too many arguments"), "{}", run.err());
}

/// Browsing an empty root has nothing to show, and says what to do instead.
#[test]
fn browsing_with_no_projects_says_how_to_make_one() {
    let world = World::new("browse-empty");
    let run = world.run(&[]);
    run.failed();
    assert!(run.err().contains("no projects in"), "{}", run.err());
    assert!(run.err().contains("try <name>"), "{}", run.err());
}

/// The picker needs a terminal, and the test harness pipes stderr.
#[test]
fn browsing_without_a_terminal_says_so() {
    let world = World::new("browse-piped");
    world.run(&["notes"]).ok();
    let run = world.run(&[]);
    run.failed();
    assert!(run.err().contains("no terminal"), "{}", run.err());
}

/// The other half of the shell function: with TRY_FD set, the path goes to
/// that descriptor and stdout stays empty.
#[test]
fn the_path_is_reported_on_the_named_descriptor() {
    let world = World::new("fd");
    let dest = world.home.join("dest");

    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "exec 3>{dest}; TRY_FD=3 {bin} notes",
            dest = dest.display(),
            bin = env!("CARGO_BIN_EXE_try"),
        ))
        .env("HOME", world.home.path())
        .env("TRY_PATH", world.root())
        .output()
        .expect("cannot run the shell");

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        fs::read_to_string(&dest).unwrap().trim_end(),
        world.root().join("notes").display().to_string()
    );
}

#[test]
fn a_local_directory_becomes_a_project_of_its_own_name() {
    if !has_git() {
        return;
    }
    let world = World::new("link");
    let target = world.home.join("scratch");
    fs::create_dir_all(&target).unwrap();

    let run = world.run(&[target.to_str().unwrap()]);
    run.ok();

    let project = world.root().join("scratch");
    assert_eq!(fs::read_link(&project).unwrap(), target);
    assert_eq!(run.out_line(), project.display().to_string());
}

#[test]
fn a_typed_name_wins_over_the_one_the_source_implies() {
    if !has_git() {
        return;
    }
    let world = World::new("named-link");
    let target = world.home.join("scratch");
    fs::create_dir_all(&target).unwrap();

    world
        .run(&["patch", target.to_str().unwrap()])
        .ok();
    assert!(world.root().join("patch").exists());
    assert!(!world.root().join("scratch").exists());
}

/// An existing project of that name wins outright, so the source is not used
/// and the project is left exactly as it was.
#[test]
fn an_existing_project_is_entered_rather_than_rebuilt() {
    let world = World::new("existing");
    world.run(&["notes"]).ok();
    let target = world.home.join("scratch");
    fs::create_dir_all(&target).unwrap();

    let run = world.run(&["notes", target.to_str().unwrap()]);
    run.ok();

    let project = world.root().join("notes");
    assert_eq!(run.out_line(), project.display().to_string());
    assert!(project.is_dir(), "still the directory it was");
    assert!(
        !fs::symlink_metadata(&project).unwrap().is_symlink(),
        "the source did not replace it"
    );
    assert!(!run.err().contains("created"), "{}", run.err());
}

/// Landing in an old project silently would make it look as though it had just
/// been made from what was typed, so the unused source is named.
#[test]
fn entering_an_existing_project_says_the_source_went_unused() {
    let world = World::new("unused");
    world.run(&["notes"]).ok();
    let target = world.home.join("scratch");
    fs::create_dir_all(&target).unwrap();

    let err = world.run(&["notes", target.to_str().unwrap()]).ok().err();
    assert!(err.contains("notes already exists"), "{err}");
    assert!(err.contains(target.to_str().unwrap()), "{err}");
    assert!(err.contains("was not used"), "{err}");
}

/// The source is not resolved at all when the name is taken, so one that could
/// not possibly work is not an error either. The name is what was asked for.
#[test]
fn a_source_is_not_resolved_when_the_name_is_taken() {
    let world = World::new("unresolved");
    world.run(&["notes"]).ok();

    let run = world.run(&["notes", "/nowhere/at/all"]);
    run.ok();
    assert_eq!(
        run.out_line(),
        world.root().join("notes").display().to_string()
    );
    assert!(run.err().contains("was not used"), "{}", run.err());
    assert!(!run.err().contains("no such path"), "{}", run.err());
}

#[test]
fn a_source_that_is_not_there_is_reported() {
    let world = World::new("missing-source");
    let run = world.run(&["/nowhere/at/all"]);
    run.failed();
    assert!(run.err().contains("no such path"), "{}", run.err());
    assert!(!world.root().join("all").exists());
}
