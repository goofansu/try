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
