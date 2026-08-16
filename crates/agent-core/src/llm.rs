//! LLM wrapper over Rig.
//!
//! Providers (env `LLM_PROVIDER`):
//! * `zen`  – OpenCode Zen, FREE models only (default)
//! * `groq` – Groq free tier (fallback)
//! * `mock` – deterministic local stand-in for offline tests (never used in CI)
//!
//! Both Zen and Groq speak the OpenAI chat-completions protocol, so a single
//! Rig `openai::Client` with a custom base URL covers both.

use anyhow::{anyhow, bail};
use rig::completion::Prompt;
use rig::providers::openai;

pub const ZEN_BASE_URL: &str = "https://opencode.ai/zen/v1";
pub const GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
// Zen model ids are unprefixed (the `opencode/` prefix is only for opencode
// config; the gateway rejects it with ModelError).
pub const DEFAULT_ZEN_MODEL: &str = "deepseek-v4-flash-free";
pub const DEFAULT_GROQ_MODEL: &str = "llama-3.3-70b-versatile";

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
        "mock" => Ok(mock_complete(prompt)),
        other => bail!("unknown LLM_PROVIDER '{other}' (use zen|groq|mock)"),
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

fn mock_developer_files(prompt: &str) -> String {
    if prompt.to_lowercase().contains("bugs to fix") {
        if mock_seeded_bug_present() {
            // The fixer repairs the seeded defect.
            r#"{"files":[{"path":"work/calc.js","content":"\"use strict\";\n\nfunction add(a, b) {\n  return a + b;\n}\n\nfunction multiply(a, b) {\n  return a * b;\n}\n\nmodule.exports = { add, multiply };\n"}]}"#.to_string()
        } else {
            r#"{"files":[]}"#.to_string()
        }
    } else {
        // The developer misses the defect so the demo exercises the full
        // bug-loop: qa reports it, evaluator FAILs, the fixer repairs it.
        r#"{"files":[]}"#.to_string()
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
    } else if lower.contains("implement") && lower.contains("json only") {
        mock_developer_files(prompt)
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
}