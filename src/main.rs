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
#[cfg(test)]
mod testing;

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
/// project of that name wins outright: the source is not resolved at all, and
/// a note says so, because the alternative is landing in an old project that
/// looks as though it was just made from what was typed.
fn enter(store: &Store, name: Option<&str>, arg: Option<&str>) -> Result<()> {
    // A typed name settles the name on its own, so the source is left
    // unresolved until the project turns out not to exist.
    let (name, resolved) = match name {
        Some(name) => (store::clean_name(name)?, None),
        None => {
            let arg = arg.expect("run() never calls enter with neither");
            let source = Source::resolve(arg)?;
            (source.name()?, Some(source))
        }
    };

    if let Some(path) = store.find(&name) {
        // Whether the source is a valid one, an unrelated one or nonsense
        // makes no difference here, so the note does not depend on it either.
        if let Some(arg) = arg {
            eprintln!("{name} already exists, so {arg} was not used");
        }
        return shell::report(&path);
    }

    // Only now is the source worth the work: a name that came from one is
    // already resolved, and a typed name has left it until here.
    let source = match (resolved, arg) {
        (Some(source), _) => Some(source),
        (None, Some(arg)) => Some(Source::resolve(arg)?),
        (None, None) => None,
    };

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

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::SystemTime;

    use chrono::TimeZone;
    use clap::CommandFactory;

    /// A project last modified at noon on `date`, which is far enough from
    /// either end of the day that no timezone turns it into the day before.
    fn dated(name: &str, date: NaiveDate) -> Project {
        let noon = date
            .and_hms_opt(12, 0, 0)
            .and_then(|naive| Local.from_local_datetime(&naive).single())
            .expect("the date has a noon");
        Project {
            name: name.to_string(),
            path: PathBuf::from("/projects").join(name),
            mtime: Some(SystemTime::from(noon)),
        }
    }

    fn undated(name: &str) -> Project {
        Project {
            name: name.to_string(),
            path: PathBuf::from("/projects").join(name),
            mtime: None,
        }
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("a real date")
    }

    /// `age` counts whole days, so every bucket is stated as a day count away
    /// from one fixed "today".
    fn age_after(days: i64) -> String {
        let today = day(2026, 6, 15);
        let then = today - chrono::Duration::days(days);
        age(&dated("p", then), today)
    }

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn arguments_parse_into_their_fields() {
        let cli = Cli::try_parse_from(["try", "notes"]).unwrap();
        assert!(!cli.path);
        assert_eq!(cli.init, None);
        assert_eq!(cli.args, vec!["notes".to_string()]);

        let cli = Cli::try_parse_from(["try", "--path"]).unwrap();
        assert!(cli.path);

        let cli = Cli::try_parse_from(["try", "--init", "fish"]).unwrap();
        assert_eq!(cli.init.as_deref(), Some("fish"));

        let cli = Cli::try_parse_from(["try", "patch", "./"]).unwrap();
        assert_eq!(cli.args, vec!["patch".to_string(), "./".to_string()]);
    }

    #[test]
    fn init_wants_a_shell_name() {
        assert!(Cli::try_parse_from(["try", "--init"]).is_err());
    }

    #[test]
    fn cancelled_reads_as_cancelled() {
        assert_eq!(Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn an_undated_project_has_no_age() {
        assert_eq!(age(&undated("p"), day(2026, 6, 15)), "");
    }

    #[test]
    fn ages_within_the_month_are_counted_in_days() {
        assert_eq!(age_after(0), "today");
        assert_eq!(age_after(1), "yesterday");
        assert_eq!(age_after(2), "2 days ago");
        assert_eq!(age_after(29), "29 days ago");
    }

    #[test]
    fn ages_past_a_month_are_counted_in_months_then_years() {
        assert_eq!(age_after(30), "1 month ago");
        assert_eq!(age_after(59), "1 month ago");
        assert_eq!(age_after(60), "2 months ago");
        assert_eq!(age_after(364), "12 months ago");
        assert_eq!(age_after(365), "1 year ago");
        assert_eq!(age_after(400), "1 year ago");
        assert_eq!(age_after(730), "2 years ago");
    }

    /// A clock that is behind the filesystem, or a project touched a moment
    /// into tomorrow, must not read as a negative age.
    #[test]
    fn a_future_date_reads_as_today() {
        assert_eq!(age_after(-1), "today");
        assert_eq!(age_after(-500), "today");
    }

    #[test]
    fn plural_agrees_with_its_number() {
        assert_eq!(plural(1, "month"), "1 month ago");
        assert_eq!(plural(2, "month"), "2 months ago");
        assert_eq!(plural(0, "year"), "0 years ago");
    }

    #[test]
    fn rows_pair_each_name_with_its_age() {
        let today = day(2026, 6, 15);
        let projects = vec![
            dated("redis", today - chrono::Duration::days(1)),
            undated("notes"),
        ];
        let rows = rows(&projects, today);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "redis");
        assert_eq!(rows[0].hint, "yesterday");
        assert_eq!(rows[1].label, "notes");
        assert_eq!(rows[1].hint, "");
    }
}
