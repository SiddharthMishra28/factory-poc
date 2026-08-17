//! Minimal MCP client support for agent tool calls.
//!
//! Servers are configured outside source control through `MCP_CONFIG` (a JSON
//! file) or `MCP_SERVERS_JSON`. Both stdio and Streamable HTTP transports use
//! JSON-RPC 2.0. Secrets in headers/environment values support `${NAME}`
//! expansion from the runner environment and are never placed in prompts.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const PROTOCOL_VERSION: &str = "2024-11-05";
const DEFAULT_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Deserialize)]
struct McpConfigFile {
    servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase")]
enum McpServerConfig {
    Stdio {
        name: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        timeout_seconds: Option<u64>,
    },
    Http {
        name: String,
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        timeout_seconds: Option<u64>,
    },
}

impl McpServerConfig {
    fn name(&self) -> &str {
        match self {
            Self::Stdio { name, .. } | Self::Http { name, .. } => name,
        }
    }

    fn timeout(&self) -> Duration {
        let seconds = match self {
            Self::Stdio { timeout_seconds, .. } | Self::Http { timeout_seconds, .. } => *timeout_seconds,
        }
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
        Duration::from_secs(seconds.clamp(1, 300))
    }
}

pub struct McpRegistry {
    servers: Vec<McpServerConfig>,
}

impl McpRegistry {
    pub fn from_env() -> Result<Self> {
        let raw = if let Ok(path) = std::env::var("MCP_CONFIG") {
            std::fs::read_to_string(&path).with_context(|| format!("cannot read MCP_CONFIG {path}"))?
        } else if let Ok(json) = std::env::var("MCP_SERVERS_JSON") {
            json
        } else {
            return Ok(Self { servers: Vec::new() });
        };
        let config: McpConfigFile = serde_json::from_str(&raw).context("invalid MCP configuration JSON")?;
        let mut names = BTreeMap::new();
        for server in &config.servers {
            if server.name().trim().is_empty() {
                bail!("MCP server name must not be empty");
            }
            if names.insert(server.name(), ()).is_some() {
                bail!("duplicate MCP server name: {}", server.name());
            }
        }
        Ok(Self { servers: config.servers })
    }

    pub fn prompt_description(&self) -> String {
        if self.servers.is_empty() {
            return "No MCP servers configured.".into();
        }
        let names = self
            .servers
            .iter()
            .map(|server| server.name())
            .collect::<Vec<_>>()
            .join(", ");
        format!("Configured servers: {names}. Call mcp_list_tools before mcp_call to inspect capabilities.")
    }

    pub fn list_tools(&self, server: &str) -> Result<Value> {
        self.request(server, "tools/list", json!({}))
    }

    pub fn call_tool(&self, server: &str, tool: &str, arguments: Value) -> Result<Value> {
        self.request(server, "tools/call", json!({ "name": tool, "arguments": arguments }))
    }

    fn request(&self, server: &str, method: &str, params: Value) -> Result<Value> {
        let config = self
            .servers
            .iter()
            .find(|candidate| candidate.name() == server)
            .ok_or_else(|| anyhow!("MCP server '{server}' is not configured"))?;
        match config {
            McpServerConfig::Stdio { command, args, env, .. } => {
                stdio_request(command, args, env, config.timeout(), method, params)
            }
            McpServerConfig::Http { url, headers, .. } => {
                http_request(url, headers, config.timeout(), method, params)
            }
        }
    }
}

fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "factory-agent", "version": env!("CARGO_PKG_VERSION") }
    })
}

fn expand_env(value: &str) -> Result<String> {
    let mut output = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| anyhow!("unclosed environment placeholder"))?;
        let name = &after[..end];
        if name.is_empty() {
            bail!("empty environment placeholder");
        }
        output.push_str(&std::env::var(name).map_err(|_| anyhow!("MCP environment variable {name} is not set"))?);
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn response_result(response: Value) -> Result<Value> {
    if let Some(error) = response.get("error") {
        bail!("MCP error: {}", error.to_string());
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("MCP response had no result"))
}

fn http_request(
    url: &str,
    headers: &BTreeMap<String, String>,
    timeout: Duration,
    method: &str,
    params: Value,
) -> Result<Value> {
    let client = reqwest::blocking::Client::builder().timeout(timeout).build()?;
    let mut request = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        .json(&rpc(1, "initialize", initialize_params()));
    for (name, value) in headers {
        request = request.header(name, expand_env(value)?);
    }
    let initialized = request.send().context("MCP initialize request failed")?;
    let session_id = initialized
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let init_status = initialized.status();
    let init_body = initialized.text()?;
    if !init_status.is_success() {
        bail!("MCP initialize returned {init_status}: {}", init_body.chars().take(500).collect::<String>());
    }
    parse_http_response(&init_body).context("invalid MCP initialize response")?;

    let mut notification = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    for (name, value) in headers {
        notification = notification.header(name, expand_env(value)?);
    }
    if let Some(session_id) = &session_id {
        notification = notification.header("mcp-session-id", session_id);
    }
    let notification_response = notification.send().context("MCP initialized notification failed")?;
    if !notification_response.status().is_success() {
        bail!("MCP initialized notification returned {}", notification_response.status());
    }

    let mut request = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        .json(&rpc(2, method, params));
    for (name, value) in headers {
        request = request.header(name, expand_env(value)?);
    }
    if let Some(session_id) = &session_id {
        request = request.header("mcp-session-id", session_id);
    }
    let response = request.send().with_context(|| format!("MCP {method} request failed"))?;
    let status = response.status();
    let body = response.text()?;
    if !status.is_success() {
        bail!("MCP {method} returned {status}: {}", body.chars().take(500).collect::<String>());
    }
    parse_http_response(&body)
}

fn parse_http_response(body: &str) -> Result<Value> {
    if let Ok(json) = serde_json::from_str::<Value>(body) {
        return response_result(json);
    }
    for line in body.lines().filter_map(|line| line.strip_prefix("data: ")) {
        if let Ok(json) = serde_json::from_str::<Value>(line) {
            return response_result(json);
        }
    }
    bail!("MCP response was neither JSON nor an SSE JSON event")
}

fn stdio_request(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    timeout: Duration,
    method: &str,
    params: Value,
) -> Result<Value> {
    let mut child = Command::new(command)
        .args(args)
        .env_clear()
        .envs(std::env::vars().filter(|(key, _)| !is_secret_name(key)))
        .envs(env.iter().map(|(key, value)| Ok((key, expand_env(value)?))).collect::<Result<Vec<_>>>()?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot start MCP server {command}"))?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("MCP stdin unavailable"))?;
    writeln!(stdin, "{}", rpc(1, "initialize", initialize_params()))?;
    writeln!(stdin, "{}", json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
    writeln!(stdin, "{}", rpc(2, method, params))?;
    drop(stdin);

    let stdout = child.stdout.take().ok_or_else(|| anyhow!("MCP stdout unavailable"))?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().flatten() {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if value.get("id").and_then(Value::as_u64) == Some(2) {
                    let _ = sender.send(value);
                    return;
                }
            }
        }
    });
    let response = receiver
        .recv_timeout(timeout)
        .map_err(|_| anyhow!("MCP {method} timed out after {}s", timeout.as_secs()))?;
    let _ = child.kill();
    let _ = child.wait();
    response_result(response)
}

fn is_secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["token", "secret", "key", "password", "pat", "auth", "credential"]
        .iter()
        .any(|needle| name.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_and_sse_responses() {
        assert_eq!(parse_http_response(r#"{"jsonrpc":"2.0","id":2,"result":{"ok":true}}"#).unwrap()["ok"], true);
        assert_eq!(parse_http_response("event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n").unwrap()["ok"], true);
    }

    #[test]
    fn expands_environment_placeholders() {
        std::env::set_var("FACTORY_MCP_TEST", "value");
        assert_eq!(expand_env("Bearer ${FACTORY_MCP_TEST}").unwrap(), "Bearer value");
        std::env::remove_var("FACTORY_MCP_TEST");
    }
}
