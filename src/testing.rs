//! Helpers the unit tests share, compiled only under `cfg(test)`.
//!
//! Three things the tests keep needing: a throwaway directory that cleans
//! itself up, a lock around the process environment (which is global, while
//! tests are not), and a git repository to point a source at.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A directory under the system temporary directory, removed on drop.
///
/// The path is canonicalised, because macOS puts the temporary directory
/// behind a symlink and git reports the resolved form.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
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
        let path = std::env::temp_dir().join(format!("try-test-{label}-{unique}"));
        fs::create_dir_all(&path).expect("cannot create the temporary directory");
        let path = fs::canonicalize(&path).expect("cannot canonicalise the temporary directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path inside the directory. Nothing is created.
    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// A subdirectory, created.
    pub fn dir(&self, name: &str) -> PathBuf {
        let path = self.join(name);
        fs::create_dir_all(&path).expect("cannot create the subdirectory");
        path
    }

    /// A file with the given contents, created.
    pub fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.join(name);
        fs::write(&path, contents).expect("cannot write the file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Serialises the tests that read or write environment variables, and puts
/// back whatever was there before.
///
/// The environment belongs to the whole process, so a test that sets `HOME`
/// while another reads it would be reading someone else's value. Every test
/// that touches `HOME`, `TRY_PATH` or `TRY_FD` — including the ones that only
/// read them through the code under test — holds one of these.
pub struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(OsString, Option<OsString>)>,
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

impl EnvGuard {
    pub fn new() -> Self {
        // A test that panics while holding the lock poisons it; the environment
        // is put back by the drop that unwinding runs, so the next test can
        // still have it.
        let lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        Self {
            _lock: lock,
            saved: Vec::new(),
        }
    }

    pub fn set(&mut self, key: &str, value: impl AsRef<OsStr>) -> &mut Self {
        self.save(key);
        // SAFETY: every test that touches the environment holds this lock, so
        // no other test is reading it while this runs.
        unsafe { std::env::set_var(key, value) };
        self
    }

    pub fn remove(&mut self, key: &str) -> &mut Self {
        self.save(key);
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
        self
    }

    fn save(&mut self, key: &str) {
        self.saved
            .push((OsString::from(key), std::env::var_os(key)));
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // In reverse, so a key set twice ends up with the value it started on.
        for (key, value) in self.saved.drain(..).rev() {
            match value {
                // SAFETY: the lock is still held; it is released after this.
                Some(value) => unsafe { std::env::set_var(&key, value) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
    }
}

/// Stamps a modification time on a path, so tests of ordering do not have to
/// wait for the clock. Works on directories on every unix we build for.
pub fn set_mtime(path: &Path, secs: u64) {
    File::open(path)
        .expect("cannot open the path")
        .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .expect("cannot set the modification time");
}

/// Whether git is on PATH. The tests that shell out to it are skipped when it
/// is not, so the suite still runs in a sandbox without it.
pub fn has_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// A repository with one empty commit, so `git worktree add` has a commit to
/// detach at.
pub fn git_init(path: &Path) {
    fs::create_dir_all(path).expect("cannot create the repository directory");
    git(path, &["init", "-q", "-b", "main"]);
    git(
        path,
        &[
            "-c",
            "user.name=try tests",
            "-c",
            "user.email=tests@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "first",
        ],
    );
}

/// A bare repository, which has no working tree of its own.
pub fn git_init_bare(path: &Path) {
    fs::create_dir_all(path).expect("cannot create the repository directory");
    git(path, &["init", "-q", "--bare"]);
}

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        // Keep the developer's own git configuration out of the tests.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("cannot run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
