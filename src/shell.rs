//! How the binary talks to the shell.
//!
//! A process cannot change its parent shell's directory, so the destination
//! is reported on a descriptor the shell function opened for it, and the
//! function itself is printed by `--init`. Both halves of that protocol live
//! here so neither can drift from the other.

use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::path::Path;

use anyhow::{Context, Result, bail};

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

pub fn init(shell: &str) -> Result<()> {
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

/// Reports the destination for the shell function to consume.
///
/// It goes to the descriptor named by TRY_FD when the shell function provides
/// one, which keeps it off stdout — otherwise `--path` would look like a
/// directory to jump to rather than something to print. With no TRY_FD there
/// is no shell function listening, so it falls back to stdout and says so.
pub fn report(path: &Path) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::fd::AsRawFd;

    use crate::testing::{EnvGuard, TempDir};

    #[test]
    fn every_supported_shell_has_an_init() {
        for shell in ["fish", "bash", "zsh"] {
            assert!(init(shell).is_ok(), "{shell}");
        }
    }

    /// `$SHELL` is a path, and people type `try --init $SHELL`.
    #[test]
    fn a_shell_may_be_given_as_a_path_or_in_capitals() {
        assert!(init("/usr/local/bin/fish").is_ok());
        assert!(init("/bin/BASH").is_ok());
        assert!(init("Zsh").is_ok());
    }

    #[test]
    fn an_unsupported_shell_says_which_ones_work() {
        let err = init("nushell").unwrap_err().to_string();
        assert!(err.contains("nushell"), "{err}");
        assert!(err.contains("fish, bash, zsh"), "{err}");
        assert!(init("").is_err());
    }

    /// Both halves of the protocol live in this file so they cannot drift: the
    /// function has to pass the descriptor the binary reads.
    #[test]
    fn both_shell_functions_speak_the_protocol() {
        for script in [FISH_INIT, POSIX_INIT] {
            assert!(script.contains("TRY_FD=3"), "{script}");
            assert!(script.contains("3>"), "{script}");
            assert!(script.contains("cd "), "{script}");
        }
    }

    /// Each function shadows the binary it calls, so it has to reach past
    /// itself: fish resolves the path first, POSIX shells use `command`.
    #[test]
    fn neither_shell_function_calls_itself() {
        assert!(FISH_INIT.contains("command -s try"));
        assert!(POSIX_INIT.contains("command try"));
    }

    #[test]
    fn the_posix_function_names_the_shell_it_was_asked_for() {
        assert!(POSIX_INIT.contains("{shell}"), "the placeholder is there");
        let bash = POSIX_INIT.replace("{shell}", "bash");
        assert!(bash.contains(r#"eval "$(try --init bash)""#), "{bash}");
        assert!(!bash.contains("{shell}"));
    }

    #[test]
    fn report_writes_the_path_to_the_descriptor() {
        let dir = TempDir::new("report");
        let path = dir.join("dest");
        let sink = File::create(&path).unwrap();

        let mut env = EnvGuard::new();
        env.set("TRY_FD", sink.as_raw_fd().to_string());
        report(Path::new("/home/ada/try/redis")).unwrap();
        drop(env);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "/home/ada/try/redis\n"
        );
        // The descriptor belongs to the caller, so report must not have closed
        // it: writing again still works.
        drop(sink);
    }

    /// Without the shell function there is no descriptor listening, so the
    /// path goes to stdout instead of being lost.
    #[test]
    fn report_falls_back_to_stdout_without_a_descriptor() {
        let mut env = EnvGuard::new();
        env.remove("TRY_FD");
        assert!(report(Path::new("/home/ada/try/redis")).is_ok());
    }

    #[test]
    fn report_falls_back_when_the_descriptor_is_unusable() {
        for raw in ["", "  ", "not-a-number", "-1", "1024"] {
            let mut env = EnvGuard::new();
            env.set("TRY_FD", raw);
            assert!(report(Path::new("/home/ada/try/redis")).is_ok(), "{raw:?}");
        }
    }

    #[test]
    fn a_descriptor_may_be_written_with_surrounding_space() {
        let dir = TempDir::new("report-space");
        let path = dir.join("dest");
        let sink = File::create(&path).unwrap();

        let mut env = EnvGuard::new();
        env.set("TRY_FD", format!(" {} ", sink.as_raw_fd()));
        report(Path::new("/tmp/x")).unwrap();
        drop(env);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "/tmp/x\n");
    }
}
