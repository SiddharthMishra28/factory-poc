//! opencode-style tool execution for the developer/fixer agent loop.
//!
//! The agent iterates: model emits a tool call → runner executes it →
//! result is appended to the transcript → repeat until the model emits a
//! `finish`. Tools are confined to the work directory; shell commands run
//! with a scrubbed environment (no tokens/secrets) and redacted output.

use crate::mcp::McpRegistry;
use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

pub const MAX_ITERATIONS_DEFAULT: usize = 15;
const MAX_READ_BYTES: u64 = 60 * 1024;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_GREP_MATCHES: usize = 100;
const MAX_FILES_WRITTEN: usize = 40;
const MAX_BYTES_WRITTEN: u64 = 4 * 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

/// Env vars matching these names are never passed to agent-run commands.
static SECRET_VAR_RE: &str = "(?i)(token|secret|key|password|pat|auth|credential)";
/// Known credential shapes redacted from command output.
static SECRET_VALUE_RE: &str = r"(?i)ghp_[A-Za-z0-9]{20,}|sk-[A-Za-z0-9]{16,}|xox[baprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}";

pub struct ToolOutcome {
    pub ok: bool,
    pub output: String,
}

pub struct ToolState {
    work_root: PathBuf,
    written: Vec<String>,
    written_bytes: u64,
    mcp: McpRegistry,
}

/// Short human-readable description of every tool, embedded in the prompt.
pub fn tool_spec() -> &'static str {
    r#"TOOLS — call exactly ONE per response:
- {"tool":{"name":"list_dir","args":{"path":""}}}            list a directory (empty path = work root)
- {"tool":{"name":"read_file","args":{"path":"work/x"}}}     print a file (capped at 60 KB)
- {"tool":{"name":"write_file","args":{"path":"work/x","content":"..."}}}  create/overwrite a file (COMPLETE contents)
- {"tool":{"name":"edit_file","args":{"path":"work/x","old":"...","new":"..."}}}  replace the first occurrence of `old`
- {"tool":{"name":"glob","args":{"pattern":"work/**/*.js"}}} list files matching a glob
- {"tool":{"name":"grep","args":{"pattern":"...","path":"work/x"}}}  search file contents (regex)
- {"tool":{"name":"run_command","args":{"command":"node --test work","cwd":"work"}}}  run a shell command (npm install, node --test, git status); secrets are NOT available
- {"tool":{"name":"git_status","args":{}}}                  git status --porcelain
- {"tool":{"name":"mcp_list_tools","args":{"server":"name"}}}  list a configured MCP server's tools
- {"tool":{"name":"mcp_call","args":{"server":"name","tool":"name","arguments":{}}}}  invoke a configured MCP tool
When the task is fully implemented, respond with EXACTLY:
{"finish":{"summary":"what you did","files":["work/..."]}}"#
}

fn secret_var_re() -> Regex {
    Regex::new(SECRET_VAR_RE).expect("static secret var regex")
}

fn secret_value_re() -> Regex {
    Regex::new(SECRET_VALUE_RE).expect("static secret value regex")
}

impl ToolState {
    pub fn new(work_dir: &Path) -> Result<Self> {
        let root = work_dir
            .canonicalize()
            .with_context(|| format!("work dir {} does not exist", work_dir.display()))?;
        Ok(Self {
            work_root: root,
            written: Vec::new(),
            written_bytes: 0,
            mcp: McpRegistry::from_env()?,
        })
    }

    pub fn tool_spec(&self) -> String {
        format!("{}\nMCP: {}", tool_spec(), self.mcp.prompt_description())
    }

    pub fn written_files(&self) -> &[String] {
        &self.written
    }

    /// Resolve a repo-relative or work-relative path, confined to work dir.
    /// The prompt tells agents paths are repo-root-relative ("work/calc.js"),
    /// so a leading work-dir segment is stripped before joining.
    fn resolve(&self, rel: &str) -> Result<PathBuf> {
        let p = Path::new(rel);
        if p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
            bail!("path escapes work dir: {rel}");
        }
        let root_name = self
            .work_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("work");
        let trimmed = p.strip_prefix(root_name).unwrap_or(p);
        let full = self.work_root.join(trimmed);
        if !full.starts_with(&self.work_root) {
            bail!("path escapes work dir: {rel}");
        }
        Ok(full)
    }

    fn rel(&self, full: &Path) -> String {
        full.strip_prefix(&self.work_root)
            .unwrap_or(full)
            .to_string_lossy()
            .replace('\\', "/")
    }

    pub fn execute(&mut self, name: &str, args: &Value) -> ToolOutcome {
        let out = (|| -> Result<String> {
            match name {
                "list_dir" => self.list_dir(args),
                "read_file" => self.read_file(args),
                "write_file" => self.write_file(args),
                "edit_file" => self.edit_file(args),
                "glob" => self.glob(args),
                "grep" => self.grep(args),
                "run_command" => self.run_command(args),
                "git_status" => self.git_status(),
                "mcp_list_tools" => self.mcp_list_tools(args),
                "mcp_call" => self.mcp_call(args),
                other => bail!("unknown tool '{other}'"),
            }
        })();
        match out {
            Ok(output) => ToolOutcome { ok: true, output },
            Err(e) => ToolOutcome {
                ok: false,
                output: format!("ERROR: {e:#}"),
            },
        }
    }

    // ----------------------------------------------------------------- tools

    fn list_dir(&self, args: &Value) -> Result<String> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        let dir = self.resolve(path)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("cannot list {}", self.rel(&dir)))?
        {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "node_modules" || name == ".git" {
                continue;
            }
            let kind = if entry.file_type()?.is_dir() {
                format!("{name}/")
            } else {
                name
            };
            entries.push(kind);
        }
        entries.sort();
        if entries.is_empty() {
            Ok("(empty)".into())
        } else {
            Ok(entries.join("\n"))
        }
    }

    fn read_file(&self, args: &Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("read_file: `path` is required"))?;
        let full = self.resolve(path)?;
        let meta = std::fs::metadata(&full).with_context(|| format!("cannot stat {path}"))?;
        if meta.len() > MAX_READ_BYTES {
            return Ok(format!("[{path} is {} bytes; too large to read]", meta.len()));
        }
        let bytes = std::fs::read(&full).with_context(|| format!("cannot read {path}"))?;
        let text = String::from_utf8(bytes)
            .map_err(|_| anyhow!("{path} is not valid UTF-8 text"))?;
        Ok(text)
    }

    fn write_file(&mut self, args: &Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("write_file: `path` is required"))?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("write_file: `content` is required"))?;
        if self.written.len() >= MAX_FILES_WRITTEN {
            bail!("file budget exceeded ({MAX_FILES_WRITTEN} files per stage)");
        }
        if self.written_bytes + content.len() as u64 > MAX_BYTES_WRITTEN {
            bail!("write budget exceeded ({MAX_BYTES_WRITTEN} bytes per stage)");
        }
        let full = self.resolve(path)?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create dir for {path}"))?;
        }
        std::fs::write(&full, content).with_context(|| format!("cannot write {path}"))?;
        let bytes = content.len();
        self.written_bytes += bytes as u64;
        if !self.written.iter().any(|w| w == path) {
            self.written.push(path.to_string());
        }
        Ok(format!("wrote {path} ({bytes} bytes)"))
    }

    fn edit_file(&mut self, args: &Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("edit_file: `path` is required"))?;
        let old = args
            .get("old")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("edit_file: `old` is required"))?;
        let new = args
            .get("new")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("edit_file: `new` is required"))?;
        let full = self.resolve(path)?;
        let text = std::fs::read_to_string(&full).with_context(|| format!("cannot read {path}"))?;
        let Some(pos) = text.find(old) else {
            bail!("edit_file: pattern `old` not found in {path}");
        };
        let mut out = String::with_capacity(text.len() + new.len());
        out.push_str(&text[..pos]);
        out.push_str(new);
        out.push_str(&text[pos + old.len()..]);
        std::fs::write(&full, &out).with_context(|| format!("cannot write {path}"))?;
        if !self.written.iter().any(|w| w == path) {
            self.written.push(path.to_string());
        }
        Ok(format!("edited {path}: replaced {} chars with {} chars", old.len(), new.len()))
    }

    fn glob(&self, args: &Value) -> Result<String> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("glob: `pattern` is required"))?;
        let re = glob_to_regex(pattern)?;
        let mut matches = Vec::new();
        for rel in list_files(&self.work_root, 4)? {
            let rel = self.work_root.join(&rel);
            let rel_s = self.rel(&rel);
            if re.is_match(&rel_s) {
                matches.push(rel_s);
            }
        }
        if matches.is_empty() {
            Ok("(no matches)".into())
        } else {
            Ok(matches.join("\n"))
        }
    }

    fn grep(&self, args: &Value) -> Result<String> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("grep: `pattern` is required"))?;
        let re = Regex::new(pattern).map_err(|e| anyhow!("bad regex `{pattern}`: {e}"))?;
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        let dir = if path.is_empty() {
            self.work_root.clone()
        } else {
            self.resolve(path)?
        };
        let mut hits = Vec::new();
        for rel in list_files(&dir, 4)? {
            let full = dir.join(&rel);
            let rel_s = self.rel(&full);
            let Ok(text) = std::fs::read_to_string(&full) else { continue };
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    hits.push(format!("{rel_s}:{}: {}", i + 1, line.trim_end()));
                    if hits.len() >= MAX_GREP_MATCHES {
                        return Ok(format!("{}\n[truncated at {MAX_GREP_MATCHES} matches]", hits.join("\n")));
                    }
                }
            }
        }
        if hits.is_empty() {
            Ok("(no matches)".into())
        } else {
            Ok(hits.join("\n"))
        }
    }

    fn run_command(&self, args: &Value) -> Result<String> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("run_command: `command` is required"))?;
        if command.chars().count() > 2000 {
            bail!("command too long");
        }
        let mut dir = self.work_root.clone();
        if let Some(cwd) = args.get("cwd").and_then(Value::as_str) {
            if !cwd.is_empty() {
                dir = self.resolve(cwd)?;
            }
        }
        let (shell, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };
        let mut cmd = std::process::Command::new(shell);
        cmd.arg(flag).arg(command).current_dir(&dir);
        for (k, v) in std::env::vars() {
            if secret_var_re().is_match(&k) {
                continue;
            }
            cmd.env(k, v);
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("cannot spawn: {command}"))?;
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait()? {
                let mut stdout = String::new();
                let mut stderr = String::new();
                use std::io::Read;
                if let Some(mut so) = child.stdout.take() {
                    so.read_to_string(&mut stdout)?;
                }
                if let Some(mut se) = child.stderr.take() {
                    se.read_to_string(&mut stderr)?;
                }
                let mut output = format!("exit={status}\n{stdout}\n{stderr}");
                output = secret_value_re().replace_all(&output, "***REDACTED***").to_string();
                if output.chars().count() > MAX_OUTPUT_BYTES {
                    output = truncate_tail(&output, MAX_OUTPUT_BYTES);
                }
                return Ok(output);
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(format!("ERROR: command timed out after {}s: {command}", COMMAND_TIMEOUT.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn git_status(&self) -> Result<String> {
        let repo_root = self
            .work_root
            .ancestors()
            .find(|p| p.join(".git").exists())
            .ok_or_else(|| anyhow!("no git repository found"))?;
        let out = std::process::Command::new("git")
            .arg("status")
            .arg("--porcelain")
            .current_dir(repo_root)
            .output()
            .with_context(|| "cannot run git status")?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if text.trim().is_empty() {
            Ok("(clean)".into())
        } else {
            Ok(truncate_tail(&text, MAX_OUTPUT_BYTES))
        }
    }

    fn mcp_list_tools(&self, args: &Value) -> Result<String> {
        let server = args
            .get("server")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("mcp_list_tools: `server` is required"))?;
        serde_json::to_string_pretty(&self.mcp.list_tools(server)?)
            .context("cannot serialize MCP tools response")
    }

    fn mcp_call(&self, args: &Value) -> Result<String> {
        let server = args
            .get("server")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("mcp_call: `server` is required"))?;
        let tool = args
            .get("tool")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("mcp_call: `tool` is required"))?;
        let arguments = args
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        serde_json::to_string_pretty(&self.mcp.call_tool(server, tool, arguments)?)
            .context("cannot serialize MCP tool response")
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Recursively list files under `root` (repo-relative paths), depth-limited,
/// skipping node_modules and .git.
fn list_files(root: &Path, depth: usize) -> Result<Vec<String>> {
    let mut out = Vec::new();
    walk(root, root, depth, &mut out);
    Ok(out)
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().map(|n| n == "node_modules" || n == ".git").unwrap_or(false) {
                continue;
            }
            walk(root, &path, depth - 1, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Convert a glob (`*`, `**`, `?`) to an anchored regex.
fn glob_to_regex(pattern: &str) -> Result<Regex> {
    let mut re = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    re.push_str(".*");
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            c => re.push(c),
        }
    }
    Regex::new(&format!("^{re}$")).map_err(|e| anyhow!("bad glob `{pattern}`: {e}"))
}

fn truncate_tail(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let tail: String = s.chars().rev().take(max).collect::<Vec<_>>().iter().rev().collect();
        format!("[truncated, showing tail]\n{tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sandbox(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("factory-tools-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn confines_paths_to_work_dir() {
        let dir = sandbox("confine");
        std::fs::write(dir.join("a.txt"), "hi").unwrap();
        let mut st = ToolState::new(&dir).unwrap();
        let r = st.execute("read_file", &json!({"path": "../a.txt"}));
        assert!(!r.ok);
        assert!(r.output.contains("escapes"));
        let r = st.execute("read_file", &json!({"path": "C:\\a.txt"}));
        assert!(!r.ok);
        let r = st.execute("list_dir", &json!({}));
        assert!(r.ok);
        assert!(r.output.contains("a.txt"));
    }

    #[test]
    fn write_and_edit_roundtrip() {
        let dir = sandbox("edit");
        let mut st = ToolState::new(&dir).unwrap();
        let r = st.execute("write_file", &json!({"path": "work/lib/a.js", "content": "let x = 1;\nlet y = 2;\n"}));
        assert!(r.ok, "{}", r.output);
        let r = st.execute("edit_file", &json!({"path": "work/lib/a.js", "old": "1", "new": "42"}));
        assert!(r.ok, "{}", r.output);
        let r = st.execute("read_file", &json!({"path": "work/lib/a.js"}));
        assert!(r.ok);
        assert!(r.output.contains("42"));
        let r = st.execute("edit_file", &json!({"path": "work/lib/a.js", "old": "nope", "new": "x"}));
        assert!(!r.ok);
    }

    #[test]
    fn glob_and_grep() {
        let dir = sandbox("glob");
        std::fs::create_dir_all(dir.join("work/app")).unwrap();
        std::fs::create_dir_all(dir.join("work/lib")).unwrap();
        std::fs::write(dir.join("work/app/page.js"), "export function add(a, b) {\n  return a + b;\n}\n").unwrap();
        std::fs::write(dir.join("work/lib/util.js"), "export const x = 1;\n").unwrap();
        let mut st = ToolState::new(&dir).unwrap();
        let r = st.execute("glob", &json!({"pattern": "work/**/*.js"}));
        assert!(r.ok, "{}", r.output);
        assert!(r.output.contains("app/page.js") && r.output.contains("lib/util.js"));
        let r = st.execute("grep", &json!({"pattern": "return a", "path": "work"}));
        assert!(r.ok, "{}", r.output);
        assert!(r.output.contains("page.js:2"));
    }

    #[test]
    fn scrubs_secret_env_and_redacts_values() {
        let dir = sandbox("scrub");
        std::fs::write(dir.join("p.txt"), "x").unwrap();
        let mut st = ToolState::new(&dir).unwrap();
        std::env::set_var("MY_TEST_TOKEN_XYZ", "ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        // env var with a secret name must not be visible to the command
        let r = st.execute("run_command", &json!({"command": "echo \"$MY_TEST_TOKEN_XYZ\""}));
        assert!(r.ok, "{}", r.output);
        assert!(!r.output.contains("ghp_"));
        // values shaped like credentials are redacted from output
        let r = st.execute("run_command", &json!({"command": "echo ghp_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}));
        assert!(r.ok, "{}", r.output);
        assert!(r.output.contains("REDACTED"));
        std::env::remove_var("MY_TEST_TOKEN_XYZ");
    }
}
