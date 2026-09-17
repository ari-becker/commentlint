//! Thin wrappers around the `git` command line for file discovery.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Files tracked by Git under the current directory.
pub fn tracked_files() -> Result<Vec<PathBuf>> {
    let out = run(&["ls-files", "-z"])?;
    Ok(split_nul(&out))
}

/// Files that Git reports as modified, added, renamed, or untracked, relative
/// to the current directory. Deleted files are omitted.
pub fn changed_files() -> Result<Vec<PathBuf>> {
    let out = run(&[
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--no-renames",
        ".",
    ])?;
    let mut files = Vec::new();
    for entry in out.split(|b| *b == 0) {
        if entry.len() < 4 {
            continue;
        }
        let status = &entry[..2];
        let path = &entry[3..];
        // Skip deletions in either the index or the worktree, and ignored files.
        if status.contains(&b'D') || status == b"!!" {
            continue;
        }
        files.push(PathBuf::from(String::from_utf8_lossy(path).into_owned()));
    }
    // `git status` paths are relative to the repository root; convert them so
    // they are relative to the working directory like the other discovery modes.
    let prefix = run(&["rev-parse", "--show-prefix"])?;
    let prefix = String::from_utf8_lossy(&prefix).trim().to_string();
    if prefix.is_empty() {
        return Ok(files);
    }
    Ok(files
        .into_iter()
        .filter_map(|p| p.strip_prefix(&prefix).ok().map(|s| s.to_path_buf()))
        .collect())
}

fn run(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to run `git {}`; is git installed?", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn split_nul(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| PathBuf::from(String::from_utf8_lossy(s).into_owned()))
        .collect()
}
