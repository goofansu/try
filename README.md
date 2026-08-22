# try

Find, create and jump into project directories.

Every project lives under one root and is a single directory named after
itself. A name is an identity: it maps to exactly one directory, so
`try redis` always means the same place. A project can be empty, cloned
from a remote repository, checked out from a pull request, or backed by a
local directory — the name is chosen the same way in every case.

## Usage

```
try                    Browse every project.
try <name>             Enter <root>/<name>, creating it if needed.
try <source>           The same, with the name derived from the source.
try <name> <source>    The same, but your name wins.
```

A bare word is **always** a name. `try src` means the project `src` even
when a directory called `src` is sitting in front of you. An argument counts as
a source only when it is a URL, or when it begins with `.` `..` `./` `../` `/`
or `~`.

An existing project is entered as it is. The source is not consulted, nothing
is fetched, and nothing is overwritten.

### Flags

```
--path            Print the project root and exit.
--init <shell>    Print the shell function to eval, for fish, bash or zsh.
-h, --help        Print usage.
-V, --version     Print the version.
```

## Sources

| Source | What happens |
| --- | --- |
| Remote URL | Cloned with the system `git`. Any host, any protocol it supports. |
| Pull request URL | The repository is cloned and the request checked out on a local branch. |
| Path to a git repository | A detached worktree of that repository. |
| Path to any other directory | Symlinked. |

A pull request means the web URL of a GitHub pull request or a GitLab merge
request. The branch created is `pr-<number>` or `mr-<number>` to match the
host's own terminology. Only those two URL layouts are recognised; anything
else is treated as an ordinary repository URL.

Pointing at a path *inside* a repository uses the whole repository, because git
cannot check out a subdirectory on its own. Bare repositories work as a source.
A path that does not exist is an error rather than a new directory.

The worktree is detached rather than on a named branch, so it never fails with
`branch is already checked out` — which is what you would hit whenever the
source repository is sitting on its main branch.

## Derived names

When you do not give a name, one is derived from the source.

| Source | Name |
| --- | --- |
| `https://<host>/<user>/<repo>.git` | `<user>-<repo>` |
| `git@<host>:<user>/<repo>.git` | `<user>-<repo>` |
| `https://<host>/<user>/<repo>/pull/<number>` | `<user>-<repo>-pr-<number>` |
| `https://<host>/<group>/<repo>/-/merge_requests/<number>` | `<group>-<repo>-mr-<number>` |
| a path inside a repository | the repository root's directory name |
| any other path | the last component of the path |

The owner is included so that two people's forks of the same project do not
collide. A `.git` suffix is dropped, so a bare repository at `thing.git`
becomes the project `thing`.

A name you type always wins, and it is cleaned before use: whitespace, slashes,
backslashes and colons become dashes, runs of dashes collapse, and leading or
trailing dashes and dots are dropped. Quote a name with spaces —
`try "my cool idea"` gives `my-cool-idea`. Two bare words are read as a
name and a source, so an unquoted multi-word name is an error rather than a
surprise.

## The picker

Bare `try` opens an inline picker over the projects that already exist. It
only ever selects; creating is what a name is for.

Projects are listed most recently modified first, at most ten rows at a time,
scrolling as you move past the end.

```
type to filter, then:

up    ctrl-p  ctrl-k    move up
down  ctrl-n  ctrl-j    move down
enter                   select
ctrl-u                  clear the filter
esc                     clear the filter, or cancel when it is empty
ctrl-c                  cancel
```

Filtering is fuzzy, and the characters your query actually matched are
highlighted. There is no `/` to enter a filter mode: any printable key starts
filtering, which is why only the control-prefixed forms navigate.

Cancelling exits with status 130.

## Shell integration

A process cannot change its parent shell's directory, so `try` reports the
chosen path and a shell function performs the `cd`.

```fish
# ~/.config/fish/config.fish
try --init fish | source
```

```bash
# ~/.bashrc or ~/.zshrc
eval "$(try --init bash)"    # or: try --init zsh
```

Each defines a function called `try`, which shadows the binary of the same
name. There is no recursion: fish's `command -s` and the POSIX `command`
builtin both skip functions and reach the binary.

The path travels on the file descriptor named by `TRY_FD`, which the function
sets to 3. That keeps it off stdout, so `--path` and `--help` still print and
still pipe. With no `TRY_FD` set the path falls back to stdout and the tool
says that the shell stayed put.

## Environment

| Variable | Meaning |
| --- | --- |
| `TRY_PATH` | The project root. Defaults to `~/try`. |
| `TRY_FD` | Descriptor the chosen directory is reported on. Set by the shell function. |

## Theme

The picker's colours are Catppuccin Mocha, pinned to the flavour's own values
rather than to ANSI colour names, so it looks the same under tmux, over ssh,
and in a terminal set to a different palette.

| Role | Value | Used for |
| --- | --- | --- |
| `text` | `#cdd6f4` | rows you are not on |
| `subtext0` | `#a6adc8` | the age column |
| `overlay0` | `#6c7086` | the prompt and the help line |
| `surface0` | `#313244` | the selection bar |
| `accent` | `#89b4fa` | the selected row's text |
| `yellow` | `#f9e2af` | matched characters |

The selection is carried three ways — a background bar, the accent colour, and
a pointer with bold weight — so it still reads with colour turned off.

## Install with Nix

Add the flake as an input:

```nix
# flake.nix
inputs.try = {
  url = "github:goofansu/try";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

Import the Home Manager module and enable it:

```nix
imports = [ try.homeModules.default ];

programs.try = {
  enable = true;
  # enableFishIntegration = true;  # default
  # enableBashIntegration = true;  # default false
  # enableZshIntegration = true;   # default false
};
```

The module installs the package and defines the `try` function itself, so there
is no need to eval `try --init` as well. It refers to the binary by its store
path rather than looking it up on `PATH`.

The module does not set `TRY_PATH`. To use a root other than `~/try`, export it
from your shell — `home.sessionVariables` reaches only shells started after the
generation is activated, so a long-running terminal or multiplexer keeps the old
value until it restarts.

Run it without installing:

```sh
nix run github:goofansu/try -- --help
```

Other outputs: `packages.<system>.try`, `overlays.default`, and a `devShell`
with the Rust toolchain.

## Building without Nix

```sh
cargo build --release
```

The binary is `target/release/try`.

It shells out to the system `git` for everything git-related, so `git` must be
on `PATH` for any source other than a plain directory.
