# try

Find, create and jump into project directories.

## Run via Nix

```sh
nix run github:goofansu/try -- --help
```

## Install via Nix

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
  # enableBashIntegration = false;  # defaults to home.shell.enableBashIntegration
  # enableFishIntegration = false;  # defaults to home.shell.enableFishIntegration
  # enableZshIntegration = false;   # defaults to home.shell.enableZshIntegration
};
```

The module installs the package and defines the `try` function itself, so there
is no need to eval `try --init` as well. It refers to the binary by its store
path rather than looking it up on `PATH`.

The module does not set `TRY_PATH`. To use a root other than `~/try`, export it
from your shell — `home.sessionVariables` reaches only shells started after the
generation is activated, so a long-running terminal or multiplexer keeps the old
value until it restarts.

Other outputs: `packages.<system>.try`, `overlays.default`, and a `devShell`
with the Rust toolchain.

`programs.try.package` has no default, because nixpkgs already has an unrelated
package named `try`. The module above sets it for you; importing
`modules/home-manager.nix` on its own means setting it yourself.

## Usage

```
$ try --help
Find, create and jump into project directories.

Usage: try [OPTIONS] [NAME|SOURCE]...

Arguments:
  [NAME|SOURCE]...  A project name, a source, or a name followed by a source

Options:
      --path          Print the project root and exit
      --init <SHELL>  Print the shell function to eval, for fish, bash or zsh
  -h, --help          Print help
  -V, --version       Print version

Examples:
  try                                           browse every project
  try notes                                     enter <root>/notes
  try https://github.com/goofansu/try.git       clone it as goofansu-try
  try git@github.com:goofansu/try.git           the same, over ssh
  try https://github.com/goofansu/try/pull/123  clone it as goofansu-try-pr-123
  try ./                                        worktree goofansu/try as try
  try ../                                       the same, from goofansu/try/src
  try patch ./                                  the same worktree, named patch
  try --init fish                               print the fish function to eval
```

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
