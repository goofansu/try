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

use std::fmt;

use anyhow::{Result, bail};
use chrono::{DateTime, Local, NaiveDate};
use clap::Parser;

mod paths;
mod picker;
mod shell;
mod source;
mod store;

use source::Source;
use store::{Project, Store};

const AFTER_HELP: &str = "\
Examples:
  try                                           browse every project
  try notes                                     enter <root>/notes
  try https://github.com/goofansu/try.git       clone it as goofansu-try
  try git@github.com:goofansu/try.git           the same, over ssh
  try https://github.com/goofansu/try/pull/123  clone it as goofansu-try-pr-123
  try ./                                        worktree goofansu/try as try
  try ../                                       the same, from goofansu/try/src
  try patch ./                                  the same worktree, named patch
  try --init fish                               print the fish function to eval";

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
        if err.downcast_ref::<Cancelled>().is_some() {
            std::process::exit(130);
        }
        eprintln!("try: {err:#}");
        std::process::exit(1);
    }
}

/// Returned when the user dismisses the picker. The picker itself reports no
/// choice as `None`; turning that into an exit code is this layer's business.
#[derive(Debug)]
struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

fn run() -> Result<()> {
    let cli = Cli::parse();

    if let Some(shell) = cli.init.as_deref() {
        return shell::init(shell);
    }

    let store = Store::open()?;
    if cli.path {
        println!("{}", store.root().display());
        return Ok(());
    }

    match cli.args.as_slice() {
        [] => browse(&store),
        [only] if source::looks_like_source(only) => enter(&store, None, Some(only)),
        [only] => enter(&store, Some(only), None),
        [first, second] => {
            if source::looks_like_source(first) && !source::looks_like_source(second) {
                bail!(
                    "the source goes last: try `try {second} {first}`\n\
                     \x20     (a name comes first, a URL or path second)"
                );
            }
            if !source::looks_like_source(second) {
                bail!(
                    "expected a URL or a path as the second argument, but got {second:?}\n\
                     \x20     for a multi-word name, quote it: try \"{first} {second}\""
                );
            }
            enter(&store, Some(first), Some(second))
        }
        _ => bail!(
            "too many arguments: expected a name, a source, or a name and a source\n\
             \x20     for a multi-word name, quote it or join it with dashes"
        ),
    }
}

/// Handles bare `try`: pick from the projects that already exist. Creating is
/// what a name is for, so the picker only ever selects.
fn browse(store: &Store) -> Result<()> {
    let projects = store.list()?;
    if projects.is_empty() {
        bail!(
            "no projects in {} yet — create one with: try <name>",
            store.root().display()
        );
    }
    let today = Local::now().date_naive();
    match picker::choose(&rows(&projects, today))? {
        Some(chosen) => shell::report(&projects[chosen].path),
        None => Err(Cancelled.into()),
    }
}

/// Handles every naming form. The name is settled first, and an existing
/// project of that name wins outright — the source is not even consulted.
fn enter(store: &Store, name: Option<&str>, arg: Option<&str>) -> Result<()> {
    // A typed name always wins, so the source is worth resolving only when the
    // name has to come from it.
    let (name, source) = match (name, arg) {
        (Some(name), None) => (store::clean_name(name)?, None),
        (Some(name), Some(arg)) => (store::clean_name(name)?, Some(Source::resolve(arg)?)),
        (None, Some(arg)) => {
            let source = Source::resolve(arg)?;
            (source.name()?, Some(source))
        }
        (None, None) => unreachable!("run() never calls enter with neither"),
    };

    if let Some(path) = store.find(&name) {
        return shell::report(&path);
    }

    let path = match &source {
        Some(source) => {
            let path = store.reserve(&name)?;
            source.create_at(&path)?;
            path
        }
        None => store.create_empty(&name)?,
    };

    eprintln!("created {}", paths::tildify(&path));
    shell::report(&path)
}

/// Turns projects into picker rows.
fn rows(projects: &[Project], today: NaiveDate) -> Vec<picker::Item> {
    projects
        .iter()
        .map(|p| picker::Item {
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
