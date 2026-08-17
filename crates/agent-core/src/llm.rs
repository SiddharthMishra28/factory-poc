//! LLM wrapper over Rig.
//!
//! Providers (env `LLM_PROVIDER`):
//! * `zen`  – OpenCode Zen, FREE models only (default)
//! * `groq` – Groq free tier (fallback)
//! * `mock` – deterministic local stand-in for offline tests (never used in CI)
//!
//! Both Zen and Groq speak the OpenAI chat-completions protocol, so a single
//! Rig `openai::Client` with a custom base URL covers both.

use anyhow::{anyhow, bail, Context};
use rig::completion::Prompt;
use rig::providers::openai;
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const ZEN_BASE_URL: &str = "https://opencode.ai/zen/v1";
pub const GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
pub const NIM_INVOKE_URL: &str = "https://integrate.api.nvidia.com/v1/chat/completions";
// Zen model ids are unprefixed (the `opencode/` prefix is only for opencode
// config; the gateway rejects it with ModelError).
pub const DEFAULT_ZEN_MODEL: &str = "deepseek-v4-flash-free";
pub const DEFAULT_GROQ_MODEL: &str = "llama-3.3-70b-versatile";
pub const DEFAULT_NIM_MODEL: &str = "stepfun-ai/step-3.7-flash";
const NIM_RPM_LIMIT: u64 = 30;

static LAST_NIM_REQUEST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

pub fn provider() -> String {
    std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "zen".to_string())
}

fn env_or(name: &str) -> anyhow::Result<String> {
    std::env::var(name).map_err(|_| anyhow!("env var {name} is not set"))
}

/// Single completion call with a hard timeout so a stuck upstream never
/// blocks a CI job forever.
pub async fn complete(prompt: &str) -> anyhow::Result<String> {
    match provider().as_str() {
        "zen" => zen(prompt).await,
        "groq" => groq(prompt).await,
        "nim" => nim(prompt).await,
        "mock" => Ok(mock_complete(prompt)),
        other => bail!("unknown LLM_PROVIDER '{other}' (use zen|groq|nim|mock)"),
    }
}

async fn zen(prompt: &str) -> anyhow::Result<String> {
    let key = env_or("ZEN_API_KEY")?;
    let model = std::env::var("ZEN_MODEL").unwrap_or_else(|_| DEFAULT_ZEN_MODEL.into());
    call(&key, ZEN_BASE_URL, &model, prompt).await
}

async fn groq(prompt: &str) -> anyhow::Result<String> {
    let key = env_or("GROQ_API_KEY")?;
    let model = std::env::var("GROQ_MODEL").unwrap_or_else(|_| DEFAULT_GROQ_MODEL.into());
    call(&key, GROQ_BASE_URL, &model, prompt).await
}

async fn nim(prompt: &str) -> anyhow::Result<String> {
    let key = env_or("NVIDIA_API_KEY")?;
    let model = std::env::var("NIM_MODEL").unwrap_or_else(|_| DEFAULT_NIM_MODEL.into());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .context("cannot create NVIDIA NIM HTTP client")?;
    let attempts = [5u64, 15, 30];
    let mut last_err: Option<anyhow::Error> = None;
    for (i, wait) in attempts.iter().enumerate() {
        wait_for_nim_slot().await;
        match nim_once(&client, &key, &model, prompt).await {
            Ok(text) => return Ok(text),
            Err(e) => {
                last_err = Some(e);
                if i < attempts.len() - 1 {
                    eprintln!("NIM call failed, retrying in {wait}s: {last_err:?}");
                    tokio::time::sleep(Duration::from_secs(*wait)).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("NIM call failed")))
}

/// Enforces a two-second interval between NIM calls. CI also serializes NIM
/// jobs globally, keeping aggregate traffic at or below 30 requests/minute.
async fn wait_for_nim_slot() {
    let interval = Duration::from_secs(60 / NIM_RPM_LIMIT);
    let wait = {
        let lock = LAST_NIM_REQUEST.get_or_init(|| Mutex::new(None));
        let mut last = lock.lock().expect("NIM rate limiter mutex poisoned");
        let now = Instant::now();
        let wait = last
            .and_then(|previous| interval.checked_sub(now.saturating_duration_since(previous)));
        *last = Some(now + wait.unwrap_or_default());
        wait
    };
    if let Some(delay) = wait {
        tokio::time::sleep(delay).await;
    }
}

#[derive(Serialize)]
struct NimRequest<'a> {
    model: &'a str,
    messages: [NimMessage<'a>; 1],
    temperature: f32,
    top_p: f32,
    max_tokens: u32,
    seed: u32,
    stream: bool,
}

#[derive(Serialize)]
struct NimMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct NimResponse {
    choices: Vec<NimChoice>,
}

#[derive(Deserialize)]
struct NimChoice {
    message: NimResponseMessage,
}

#[derive(Deserialize)]
struct NimResponseMessage {
    content: Option<String>,
}

async fn nim_once(client: &reqwest::Client, api_key: &str, model: &str, prompt: &str) -> anyhow::Result<String> {
    let payload = NimRequest {
        model,
        messages: [NimMessage { role: "user", content: prompt }],
        temperature: 1.0,
        top_p: 0.95,
        max_tokens: 16_384,
        seed: 42,
        stream: false,
    };
    let response = client
        .post(NIM_INVOKE_URL)
        .bearer_auth(api_key)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&payload)
        .send()
        .await
        .context("NIM request failed")?;
    let status = response.status();
    let body = response.text().await.context("cannot read NIM response")?;
    if !status.is_success() {
        bail!("NIM returned {status}: {}", body.chars().take(500).collect::<String>());
    }
    let response: NimResponse = serde_json::from_str(&body).context("invalid NIM response JSON")?;
    response
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| anyhow!("NIM response had no completion text"))
}

async fn call(api_key: &str, base_url: &str, model: &str, prompt: &str) -> anyhow::Result<String> {
    // Free tiers are burst-limited: retry 429/5xx with backoff so a single
    // stage can absorb a transient throttle instead of failing the goal.
    let attempts = [5u64, 15, 30];
    let mut last_err: Option<anyhow::Error> = None;
    for (i, wait) in attempts.iter().enumerate() {
        match call_once(api_key, base_url, model, prompt).await {
            Ok(text) => return Ok(text),
            Err(e) => {
                last_err = Some(e);
                if i < attempts.len() - 1 {
                    eprintln!("LLM call failed, retrying in {wait}s: {last_err:?}");
                    tokio::time::sleep(std::time::Duration::from_secs(*wait)).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("LLM call failed")))
}

async fn call_once(api_key: &str, base_url: &str, model: &str, prompt: &str) -> anyhow::Result<String> {
    let client = openai::Client::from_url(api_key, base_url);
    let agent = client
        .agent(model)
        .temperature(0.2)
        .max_tokens(4096)
        .build();

    let text = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        agent.prompt(prompt.to_string()),
    )
    .await
    .map_err(|_| anyhow!("LLM call timed out after 180s"))?
    .map_err(|e| anyhow!("LLM call failed: {e}"))?;

    Ok(text)
}

// ---------------------------------------------------------------------------
// Mock provider: deterministic, offline. Used by `scripts/local-e2e.ps1`.
// It plays the seeded-defect scenario: the seeded bug is `return a - b`
// inside `work/calc.js`. The mock planner/developer/qa/evaluator all reason
// about that single fact, so the full bug-loop can be exercised for free.
// ---------------------------------------------------------------------------

fn mock_seeded_bug_present() -> bool {
    // Cargo tests run from the crate dir; the binary runs from the repo root.
    for candidate in ["work/calc.js", "../../work/calc.js", "../work/calc.js"] {
        if let Ok(src) = std::fs::read_to_string(candidate) {
            return src.contains("return a - b");
        }
    }
    false
}

fn mock_plan() -> &'static str {
    r#"{"stages":[
      {"id":"1","role":"developer","objective":"Implement the goal in work/","acceptance_criteria":["work/ tests pass"]},
      {"id":"2","role":"qa","objective":"Verify behavior with tests and inspection","acceptance_criteria":["report bugs if any"]},
      {"id":"3","role":"evaluator","objective":"Independently validate the result","acceptance_criteria":["PASS only if fully correct"]}
    ]}"#
}

/// Mock for the tool-loop protocol: first response is a tool call, and once
/// tool results are in the transcript the agent finishes. The fixer repairs
/// the seeded defect; the developer inspects and then finishes without
/// changing anything, so the demo exercises the full bug-loop.
fn mock_tool_loop(prompt: &str) -> String {
    let lower = prompt.to_lowercase();
    let has_results = lower.contains("- call:");
    let is_fixer = lower.contains("bugs to fix");
    if has_results {
        r#"{"finish":{"summary":"task complete","files":[]}}"#.to_string()
    } else if is_fixer {
        r#"{"tool":{"name":"write_file","args":{"path":"work/calc.js","content":"\"use strict\";\n\nfunction add(a, b) {\n  return a + b;\n}\n\nfunction multiply(a, b) {\n  return a * b;\n}\n\nmodule.exports = { add, multiply };\n"}}}"#.to_string()
    } else {
        r#"{"tool":{"name":"list_dir","args":{"path":"work"}}}"#.to_string()
    }
}

fn mock_qa_report() -> String {
    if mock_seeded_bug_present() {
        r#"{"summary":"add() returns the wrong result","tests":[{"name":"node --test work/","passed":false}],"bugs":[{"severity":"high","location":"work/calc.js:4","description":"add(2,3) returns -1 instead of 5: uses subtraction"}]}"#.to_string()
    } else {
        r#"{"summary":"no bugs found","tests":[{"name":"node --test work/","passed":true}],"bugs":[]}"#.to_string()
    }
}

fn mock_eval() -> &'static str {
    if mock_seeded_bug_present() {
        r#"{"decision":"FAIL","bugs":[{"severity":"high","location":"work/calc.js:4","description":"add() subtracts instead of adding"}],"evidence":["node --test work/ fails","code inspection: add uses '-'"]}"#
    } else {
        r#"{"decision":"PASS","bugs":[],"evidence":["node --test work/ passes","code inspection: add uses '+'"]}"#
    }
}

fn mock_complete(prompt: &str) -> String {
    let lower = prompt.to_lowercase();
    if lower.contains("planner") && lower.contains("plan the minimum") {
        mock_plan().to_string()
    } else if lower.contains("evaluator") && lower.contains("independently verify") {
        mock_eval().to_string()
    } else if lower.contains("quality assurance") || lower.contains("qa agent") {
        mock_qa_report().to_string()
    } else if lower.contains("call exactly one") {
        mock_tool_loop(prompt)
    } else if lower.contains("implement") && lower.contains("json only") {
        // legacy one-shot developer prompt (kept for safety)
        r#"{"files":[]}"#.to_string()
    } else {
        r#"{"status":"completed","summary":"mock fallback","next_action":""}"#.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_defaults_to_zen() {
        std::env::remove_var("LLM_PROVIDER");
        assert_eq!(provider(), "zen");
    }

    #[test]
    fn mock_roundtrip() {
        std::env::set_var("LLM_PROVIDER", "mock");
        assert!(mock_seeded_bug_present());
        let plan = mock_complete("You are the planner. PLAN THE MINIMUM stages.");
        assert!(plan.contains("\"evaluator\""));
        let eval = mock_complete("You are the evaluator. Independently verify.");
        assert!(eval.contains("FAIL"));
    }

    #[test]
    fn nim_payload_matches_documented_defaults() {
        let payload = NimRequest {
            model: DEFAULT_NIM_MODEL,
            messages: [NimMessage { role: "user", content: "hello" }],
            temperature: 1.0,
            top_p: 0.95,
            max_tokens: 16_384,
            seed: 42,
            stream: false,
        };
        let value = serde_json::to_value(payload).unwrap();
        assert_eq!(value["model"], DEFAULT_NIM_MODEL);
        assert_eq!(value["max_tokens"], 16_384);
        assert_eq!(value["stream"], false);
    }
}
