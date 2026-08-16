//! Repository inspection + test running. This is the runner's own ground
//! truth; the orchestrator-provided repo metadata is never trusted.

use agent_core::schema::{Repository, TestResult};
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 8 * 1024;
const MAX_FILES: usize = 20;

/// Recursively list files under `dir` (relative paths), depth-limited.
pub fn list_files(dir: &Path, depth: usize) -> Vec<String> {
    let mut out = Vec::new();
    walk(dir, dir, depth, &mut out);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().map(|n| n == "node_modules" || n == ".git").unwrap_or(false) {
                continue;
            }
            walk(root, &path, depth - 1, out);
        } else {
            if let Ok(rel) = path.strip_prefix(root) {
                if out.len() < MAX_FILES {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
}

/// Read file contents (capped) into a single "===== path =====" joined blob.
pub fn file_contents(dir: &Path) -> String {
    let mut parts = Vec::new();
    for rel in list_files(dir, 3) {
        let full: PathBuf = dir.join(&rel);
        let meta = std::fs::metadata(&full).ok();
        let big = meta.map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(false);
        if big {
            parts.push(format!("===== {rel} =====\n[file too large, skipped]"));
            continue;
        }
        match std::fs::read_to_string(&full) {
            Ok(text) => parts.push(format!("===== {rel} =====\n{text}")),
            Err(_) => parts.push(format!("===== {rel} =====\n[binary or unreadable]")),
        }
    }
    parts.join("\n")
}

/// Run `node --test <dir>` and capture output. Reports one TestResult.
pub fn run_tests(dir: &Path) -> (Vec<TestResult>, String) {
    let out = std::process::Command::new("node")
        .arg("--test")
        .arg(dir)
        .output();
    let (passed, output) = match out {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).to_string();
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            (o.status.success(), format!("{stdout}\n{stderr}"))
        }
        Err(e) => (false, format!("failed to run node --test: {e}")),
    };
    let name = format!("node --test {}", dir.display());
    (vec![TestResult { name, passed }], output)
}

/// Fill the repository section with local truth. Returns the local test
/// results alongside (they are also embedded in `repo.test_output`).
pub fn inspect(work_dir: &Path, repo: &mut Repository) -> (Vec<TestResult>, String) {
    repo.commit_hash = crate::git::short_commit();
    repo.recent_commits = crate::git::recent_commits(5);
    repo.work_dir_files = list_files(work_dir, 3);
    repo.files = vec![file_contents(work_dir)];
    let (tests, output) = run_tests(work_dir);
    let mut truncated = output.clone();
    truncated.truncate(3000);
    repo.test_output = truncated;
    (tests, output)
}