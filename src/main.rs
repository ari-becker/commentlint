//! commentlint: static analysis for code comments, powered by TypeSafe.ai.

mod config;
mod extract;
mod git;
mod languages;
mod typesafe;

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::thread;

use anyhow::{Context, Result};
use clap::Parser;

use config::{Loader, Resolved};
use extract::Comment;

/// Lint code comments with TypeSafe.ai.
///
/// With no paths, checks every file tracked by Git. Pass `.` to walk the
/// current directory instead, or pass explicit file names (as pre-commit does).
#[derive(Parser, Debug)]
#[command(name = "commentlint", version, about, long_about = None)]
struct Cli {
    /// Only check files that Git reports as changed (staged, unstaged, or untracked).
    #[arg(long)]
    only_changed: bool,

    /// Read text from stdin and evaluate the rules against it directly, without
    /// parsing any files.
    #[arg(long, conflicts_with_all = ["only_changed", "paths"])]
    from_stdin: bool,

    /// Print the comments that would be evaluated and exit without calling the API.
    #[arg(long)]
    list_comments: bool,

    /// Number of concurrent API requests.
    #[arg(long, short = 'j', default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=64))]
    jobs: u16,

    /// Print progress and skipped files to stderr.
    #[arg(long, short)]
    verbose: bool,

    /// Files or directories to check. A directory is walked recursively,
    /// honouring .gitignore.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,
}

/// One comment queued for evaluation together with its resolved rules.
struct Job {
    path: PathBuf,
    comment: Comment,
    config: Resolved,
}

/// One failed rule for one comment.
struct Finding {
    path: PathBuf,
    line: usize,
    column: usize,
    rule: String,
    probability: f64,
    threshold: f64,
    excerpt: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            // A closed pipe on stdout (for example `commentlint | head`) is
            // not an error worth reporting.
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
            {
                return ExitCode::SUCCESS;
            }
            eprintln!("commentlint: error: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Returns `Ok(true)` when no comment failed a rule.
fn run(cli: Cli) -> Result<bool> {
    let defaults = config::defaults()?;
    let mut loader = Loader::new(PathBuf::from("."), defaults);

    let jobs = if cli.from_stdin {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("failed to read stdin")?;
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(true);
        }
        let config = loader.for_dir(Path::new("."))?;
        vec![Job {
            path: PathBuf::from("<stdin>"),
            comment: Comment {
                line: 1,
                column: 1,
                text,
            },
            config,
        }]
    } else {
        let files = discover_files(&cli)?;
        collect_jobs(&files, &mut loader, cli.verbose)?
    };

    if cli.list_comments {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for j in &jobs {
            writeln!(
                out,
                "{}:{}:{}: {}",
                j.path.display(),
                j.comment.line,
                j.comment.column,
                oneline(&j.comment.text)
            )?;
        }
        return Ok(true);
    }
    if jobs.is_empty() {
        if cli.verbose {
            eprintln!("commentlint: no comments to evaluate");
        }
        return Ok(true);
    }

    let key = typesafe::load_key()?;
    let client = typesafe::Client::new(key);
    if cli.verbose {
        eprintln!(
            "commentlint: evaluating {} comment(s) with {} worker(s)",
            jobs.len(),
            cli.jobs
        );
    }

    let mut findings = evaluate_all(&client, jobs, cli.jobs as usize)?;
    findings.sort_by(|a, b| (&a.path, a.line, a.column, &a.rule).cmp(&(&b.path, b.line, b.column, &b.rule)));

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for f in &findings {
        writeln!(
            out,
            "{}:{}:{}: error: comment fails rule `{}` (p={:.2}, threshold {:.2})\n    {}",
            f.path.display(),
            f.line,
            f.column,
            f.rule,
            f.probability,
            f.threshold,
            f.excerpt
        )?;
    }
    if !findings.is_empty() {
        writeln!(out, "commentlint: {} issue(s) found", findings.len())?;
    }
    Ok(findings.is_empty())
}

/// Works out which files to parse from the command line and Git.
fn discover_files(cli: &Cli) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = Vec::new();
    if cli.paths.is_empty() {
        files = if cli.only_changed {
            git::changed_files().context("listing changed files with `git status`")?
        } else {
            git::tracked_files().context("listing Git-tracked files (pass `.` to walk the directory instead)")?
        };
    } else {
        for p in &cli.paths {
            if p.is_dir() {
                files.extend(walk_dir(p)?);
            } else {
                files.push(p.clone());
            }
        }
        if cli.only_changed {
            let changed: HashSet<PathBuf> = git::changed_files()?.iter().map(|p| normalize(p)).collect();
            files.retain(|f| changed.contains(&normalize(f)));
        }
    }

    let mut files: Vec<PathBuf> = files.iter().map(|p| normalize(p)).collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// Walks a directory recursively, honoring .gitignore and skipping hidden entries.
fn walk_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(dir).build() {
        let entry = entry?;
        if entry.file_type().is_some_and(|t| t.is_file()) {
            out.push(entry.into_path());
        }
    }
    Ok(out)
}

/// Strips a leading `./` so paths from different sources compare equal.
fn normalize(p: &Path) -> PathBuf {
    p.strip_prefix(".")
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| p.to_path_buf())
}

/// Parses every file and builds the evaluation queue.
fn collect_jobs(files: &[PathBuf], loader: &mut Loader, verbose: bool) -> Result<Vec<Job>> {
    let mut jobs = Vec::new();
    for path in files {
        let extracted = match extract::extract_from_file(path) {
            Ok(Some(x)) => x,
            Ok(None) => {
                if verbose {
                    eprintln!("commentlint: skipping {} (unsupported file type)", path.display());
                }
                continue;
            }
            Err(e) => {
                if path.exists() {
                    return Err(e);
                }
                // pre-commit can pass files that were deleted in the same commit.
                if verbose {
                    eprintln!("commentlint: skipping {} (missing)", path.display());
                }
                continue;
            }
        };
        let (lang, comments) = extracted;
        let config = loader.for_file(path)?;
        if config.rules.is_empty() {
            if verbose {
                eprintln!("commentlint: skipping {} (all rules disabled)", path.display());
            }
            continue;
        }
        let mut kept = 0usize;
        for comment in comments {
            if !config.should_evaluate(&comment.text) {
                continue;
            }
            kept += 1;
            jobs.push(Job {
                path: path.clone(),
                comment,
                config: config.clone(),
            });
        }
        if verbose {
            eprintln!("commentlint: {} ({}): {} comment(s)", path.display(), lang.name(), kept);
        }
    }
    Ok(jobs)
}

/// Sends every job to the API using a fixed pool of worker threads.
fn evaluate_all(client: &typesafe::Client, jobs: Vec<Job>, workers: usize) -> Result<Vec<Finding>> {
    let queue = Mutex::new(jobs.into_iter());
    let findings = Mutex::new(Vec::new());
    let failure: Mutex<Option<anyhow::Error>> = Mutex::new(None);

    thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    if failure.lock().unwrap().is_some() {
                        return;
                    }
                    let Some(job) = queue.lock().unwrap().next() else {
                        return;
                    };
                    match evaluate_one(client, &job) {
                        Ok(mut f) => findings.lock().unwrap().append(&mut f),
                        Err(e) => {
                            let mut slot = failure.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(e.context(format!(
                                    "evaluating {}:{}:{}",
                                    job.path.display(),
                                    job.comment.line,
                                    job.comment.column
                                )));
                            }
                            return;
                        }
                    }
                }
            });
        }
    });

    if let Some(e) = failure.into_inner().unwrap() {
        return Err(e);
    }
    Ok(findings.into_inner().unwrap())
}

fn evaluate_one(client: &typesafe::Client, job: &Job) -> Result<Vec<Finding>> {
    let answers = client.evaluate(&job.config.model, &job.comment.text, &job.config.rules)?;
    let mut out = Vec::new();
    for (id, rule) in &job.config.rules {
        let p = answers[id];
        let threshold = job.config.threshold_for(rule);
        if p < threshold {
            out.push(Finding {
                path: job.path.clone(),
                line: job.comment.line,
                column: job.comment.column,
                rule: id.clone(),
                probability: p,
                threshold,
                excerpt: oneline(&job.comment.text),
            });
        }
    }
    Ok(out)
}

/// Collapses a comment to one line, truncated for display.
fn oneline(text: &str) -> String {
    const MAX: usize = 120;
    let joined: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= MAX {
        return joined;
    }
    let cut: String = joined.chars().take(MAX - 1).collect();
    format!("{cut}…")
}
