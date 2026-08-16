//! Thin shell-out wrapper around `git` (no heavy git libraries).

use anyhow::{anyhow, Context, Result};
use std::path::Path;

/// Run a git command, returning trimmed stdout.
pub fn run(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn git {args:?}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(anyhow!("git {} failed: {stderr}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn short_commit() -> String {
    run(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|_| "unknown".into())
}

pub fn recent_commits(n: usize) -> Vec<String> {
    run(&["log", "--oneline", &format!("-{n}")])
        .map(|s| s.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default()
}

pub fn has_changes() -> bool {
    run(&["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Stage everything and commit. Returns the new short hash. The `[skip ci]`
/// marker guarantees pushes never re-trigger this dispatch-only workflow.
pub fn commit_all(message: &str) -> Result<String> {
    ensure_node_modules_ignored()?;
    run(&["add", "-A"])?;
    if !has_changes() {
        return Err(anyhow!("nothing to commit"));
    }
    run(&["commit", "-m", message])?;
    Ok(short_commit())
}

/// An agent may run `npm install`, which creates `node_modules` under work/.
/// Belt and braces: make sure the repo ignores it before staging anything,
/// so a huge dependency tree never lands in the commit.
fn ensure_node_modules_ignored() -> Result<()> {
    let gi = Path::new(".gitignore");
    let existing = if gi.exists() {
        std::fs::read_to_string(gi)?
    } else {
        String::new()
    };
    if existing.lines().any(|l| l.trim() == "node_modules") {
        return Ok(());
    }
    let mut s = existing;
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("node_modules\n");
    std::fs::write(gi, s)?;
    Ok(())
}

/// Push the current branch. Errors are non-fatal (no remote in local tests).
pub fn try_push() -> Result<()> {
    run(&["push", "origin", "HEAD"])
        .map(|_| ())
        .map_err(|e| anyhow!("push skipped: {e}"))
}