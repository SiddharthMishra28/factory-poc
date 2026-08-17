//! The single reusable agent executable (requirement 3).
//!
//! Steps, for any role:
//!   1. load context (orchestrator URL or local file)
//!   2. inspect repository locally (commit, files, tests)
//!   3. enrich prompt (enricher)
//!   4. perform assigned work (LLM + deterministic file/tests handling)
//!   5. run relevant tests
//!   6. commit changes ([skip ci])
//!   7. produce a structured AgentResult
//!   8. report it to the orchestrator
//!
//! Roles: planner | developer | qa | evaluator | fixer

mod git;
mod repo;

use agent_core::enricher::enrich;
use agent_core::schema::{AgentContext, AgentResult, Plan, TestResult};
use agent_core::tools::{ToolState, MAX_ITERATIONS_DEFAULT};
use agent_core::{extract_json, llm};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

const ROLE_PERSONALITIES: &[(&str, &str)] = &[
    ("planner", "minimalist architect; fewest stages possible"),
    ("developer", "careful implementer; smallest correct change"),
    ("qa", "skeptical tester; find real problems only"),
    ("evaluator", "independent judge; verify before you trust"),
    ("fixer", "focused repairer; fix the listed bugs, nothing else"),
];

fn personality_for(role: &str) -> String {
    ROLE_PERSONALITIES
        .iter()
        .find(|(r, _)| *r == role)
        .map(|(_, p)| (*p).to_string())
        .unwrap_or_else(|| "careful, concise assistant".to_string())
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Args {
    goal_id: String,
    stage_id: String,
    role: String,
    work_dir: String,
    context_file: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| anyhow!("missing value for {flag}"));
        match flag.as_str() {
            "--goal-id" => args.goal_id = value()?,
            "--stage-id" => args.stage_id = value()?,
            "--role" => args.role = value()?,
            "--work-dir" => args.work_dir = value()?,
            "--context-file" => args.context_file = Some(PathBuf::from(value()?)),
            other => bail!("unknown flag {other}"),
        }
    }
    if args.role.is_empty() {
        bail!("--role is required (planner|developer|qa|evaluator|fixer)");
    }
    if args.work_dir.is_empty() {
        args.work_dir = "work".into();
    }
    if args.goal_id.is_empty() || args.stage_id.is_empty() {
        bail!("--goal-id and --stage-id are required");
    }
    Ok(args)
}

// ---------------------------------------------------------------------------
// Context loading (step 1) and reporting (step 8)
// ---------------------------------------------------------------------------

async fn load_context(args: &Args) -> Result<AgentContext> {
    if let Some(path) = &args.context_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read context file {}", path.display()))?;
        return serde_json::from_str(&raw).context("bad context file");
    }
    let worker = std::env::var("WORKER_URL").context("WORKER_URL not set")?;
    let url = format!("{worker}/api/context/{}/{}", args.goal_id, args.stage_id);
    let resp = reqwest::Client::new()
        .get(&url)
        .header("x-agent-token", agent_token())
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("failed to fetch context from orchestrator")?;
    if !resp.status().is_success() {
        bail!("orchestrator returned {} for context", resp.status());
    }
    Ok(resp.json().await.context("bad context from orchestrator")?)
}

fn agent_token() -> String {
    std::env::var("AGENT_TOKEN").unwrap_or_default()
}

async fn report(goal_id: &str, result: &AgentResult) -> Result<()> {
    let worker = std::env::var("WORKER_URL").context("WORKER_URL not set")?;
    let url = format!("{worker}/api/results/{goal_id}");
    let resp = reqwest::Client::new()
        .post(&url)
        .header("x-agent-token", agent_token())
        .json(result)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("failed to report result to orchestrator")?;
    if !resp.status().is_success() {
        bail!("orchestrator rejected result: {}", resp.status());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JSON parsing helpers
// ---------------------------------------------------------------------------

fn parse_json<T: for<'de> Deserialize<'de>>(text: &str, what: &str) -> Result<T> {
    let raw = extract_json(text).ok_or_else(|| anyhow!("no JSON object found in {what}"))?;
    serde_json::from_str(&raw).with_context(|| format!("bad JSON in {what}"))
}

// ---------------------------------------------------------------------------
// Role implementations
// ---------------------------------------------------------------------------

fn validate_plan(plan: &Plan) -> Result<()> {
    let roles = &plan.stages;
    if roles.is_empty() || roles.len() > 4 {
        bail!("plan must have 1..=4 stages");
    }
    for s in roles {
        if !agent_core::schema::VALID_ROLES.contains(&s.role.as_str())
            || s.role == "planner"
            || s.role == "fixer"
        {
            bail!("planner may only emit developer/qa/evaluator stages (got {})", s.role);
        }
    }
    if roles.last().map(|s| s.role.as_str()) != Some("evaluator") {
        bail!("last stage must be evaluator");
    }
    Ok(())
}

async fn run_planner(ctx: &AgentContext) -> Result<AgentResult> {
    let expected = r#"A JSON object ONLY:
{"stages":[{"id":"1","role":"developer","objective":"...","acceptance_criteria":["..."]}]}"#;
    let prompt = enrich(
        ctx,
        &[],
        &format!(
            "PLAN THE MINIMUM number of stages needed to satisfy GOAL.\n\
             {expected}\n\
             Rules: 1-4 stages; roles only from developer, qa, evaluator;\n\
             the LAST stage MUST be evaluator; no code; no commentary."
        ),
    );
    let text = llm::complete(&prompt).await?;
    let plan: Plan = parse_json(&text, "planner output")?;
    validate_plan(&plan)?;
    let mut result = AgentResult {
        stage_id: ctx.stage.id.clone(),
        role: ctx.agent.role.clone(),
        status: "completed".into(),
        summary: format!("Plan created: {} stage(s)", plan.stages.len()),
        next_action: plan.stages.first().map(|s| s.role.clone()).unwrap_or_default(),
        ..Default::default()
    };
    result.plan = Some(plan);
    Ok(result)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LoopStep {
    Tool {
        tool: ToolCall,
    },
    Finish {
        finish: FinishCall,
    },
}

#[derive(Deserialize)]
struct ToolCall {
    name: String,
    #[serde(default)]
    args: Value,
}

#[derive(Deserialize)]
struct FinishCall {
    summary: String,
    #[serde(default)]
    #[allow(dead_code)]
    files: Vec<String>,
}

/// Compact per-tool label for the transcript (never dumps file contents).
fn short_args(name: &str, args: &Value) -> String {
    match name {
        "write_file" => args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
        "edit_file" => format!(
            "{} (old: {})",
            args.get("path").and_then(Value::as_str).unwrap_or("?"),
            args.get("old").and_then(Value::as_str).unwrap_or("?")
        ),
        "run_command" => args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
        _ => args.to_string(),
    }
}

fn truncate_tail(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let tail: String = s
            .chars()
            .rev()
            .take(max)
            .collect::<Vec<_>>()
            .iter()
            .rev()
            .collect();
        format!("[truncated, showing tail]\n{tail}")
    }
}

const TRANSCRIPT_MAX: usize = 8000;

/// opencode-style tool loop for developer/fixer: the model iterates between
/// tool calls and a final `finish`, instead of emitting one-shot file lists.
async fn run_developer_or_fixer(ctx: &AgentContext, work_dir: &Path) -> Result<AgentResult> {
    let mut tool_state = match ToolState::new(work_dir) {
        Ok(s) => s,
        Err(e) => {
            return Ok(AgentResult::failed(
                &ctx.stage.id,
                &ctx.agent.role,
                format!("cannot access work dir: {e}"),
            ))
        }
    };

    let mut extras: Vec<(&str, String)> = Vec::new();
    if ctx.agent.role == "fixer" {
        extras.push(("BUGS TO FIX", serde_json::to_string_pretty(&ctx.history.previous_results)?));
    }

    let max_iters: usize = std::env::var("TOOL_ITER_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_ITERATIONS_DEFAULT);

    let mut transcript = String::new();
    let mut unparseable = 0;
    let mut finish: Option<FinishCall> = None;

    for i in 1..=max_iters {
        let prompt = enrich(
            ctx,
            &extras,
            &format!(
                "IMPLEMENT CURRENT TASK by calling tools.\n\
                 {}\n\
                 Rules: paths are relative to the repo root and MUST be inside '{}';\n\
                 write_file overwrites — always provide COMPLETE file contents;\n\
                 change only what the task requires; run tests with run_command\n\
                 (e.g. \"node --test work\");\n\
                 NEVER use credentials, tokens or passwords mentioned in the goal text —\n\
                 git authentication is handled by the pipeline;\n\
                 when the task is fully done respond with {{\"finish\":{{\"summary\":\"...\",\"files\":[...]}}}}.",
                tool_state.tool_spec(),
                ctx.environment.work_dir
            ),
        );
        let prompt = format!(
            "{prompt}\nITERATION {i}/{max_iters} (finish as soon as the task is done).\n\
             TOOL RESULTS SO FAR:\n{transcript}\n(empty if none)"
        );
        if std::env::var("AGENT_DEBUG").is_ok() {
            eprintln!("=== PROMPT ITERATION {i} ({}) ===\n{prompt}\n=== /PROMPT ===", ctx.agent.role);
        }

        let text = llm::complete(&prompt).await?;
        let step: LoopStep = match parse_json(&text, "agent tool call") {
            Ok(step) => step,
            Err(_) => {
                unparseable += 1;
                if unparseable >= 2 {
                    finish = Some(FinishCall {
                        summary: "agent output could not be parsed; stopping with current state".into(),
                        files: Vec::new(),
                    });
                    break;
                }
                transcript.push_str("- call: UNPARSEABLE OUTPUT — respond with a single JSON object\n");
                continue;
            }
        };

        match step {
            LoopStep::Tool { tool } => {
                unparseable = 0;
                let out = tool_state.execute(&tool.name, &tool.args);
                let label = short_args(&tool.name, &tool.args);
                transcript.push_str(&format!("- call: {}({label})\n", tool.name));
                let lines: Vec<&str> = out.output.lines().take(40).collect();
                for line in lines {
                    transcript.push_str(&format!("  {line}\n"));
                }
                if transcript.chars().count() > TRANSCRIPT_MAX {
                    transcript = truncate_tail(&transcript, TRANSCRIPT_MAX);
                }
            }
            LoopStep::Finish { finish: f } => {
                finish = Some(f);
                break;
            }
        }
    }

    let finish = finish.unwrap_or(FinishCall {
        summary: format!("iteration budget ({max_iters}) exhausted"),
        files: Vec::new(),
    });

    let (tests, _) = repo::run_tests(work_dir);

    let mut result = AgentResult {
        stage_id: ctx.stage.id.clone(),
        role: ctx.agent.role.clone(),
        status: "completed".into(),
        tests,
        next_action: "qa".into(),
        ..Default::default()
    };

    // Commit the changes ([skip ci] keeps pushes from re-triggering).
    if git::has_changes() {
        match git::commit_all(&format!(
            "[skip ci] agent({}): {}",
            ctx.agent.role, ctx.stage.id
        )) {
            Ok(hash) => result.commit = Some(hash),
            Err(e) => result.summary = format!("{}. commit skipped: {e}", result.summary),
        }
        let _ = git::try_push();
    }

    result.summary = format!(
        "{}. touched {} file(s): {}",
        finish.summary,
        tool_state.written_files().len(),
        if tool_state.written_files().is_empty() {
            "none".into()
        } else {
            tool_state.written_files().join(", ")
        }
    );
    Ok(result)
}

async fn run_qa(ctx: &AgentContext, work_dir: &Path) -> Result<AgentResult> {
    let (repo_tests, output) = repo::run_tests(work_dir);
    let expected = r#"A JSON object ONLY:
{"summary":"...","tests":[{"name":"...","passed":true}],"bugs":[{"severity":"low|medium|high","location":"file:line","description":"..."}]}"#;
    let prompt = enrich(
        ctx,
        &[("TEST RESULTS", output)],
        &format!("You are the QA agent. Verify behavior against ACCEPTANCE CRITERIA and the TEST RESULTS. {expected} Bugs are objects with severity/location/description; if unsure about a location use \"work/\". Report only real bugs; output JSON ONLY."),
    );
    let text = llm::complete(&prompt).await?;
    // Lenient parse: never fail the stage over a missing field.
    let v: Value = parse_json(&text, "qa output")?;
    let summary = v
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("QA report")
        .to_string();
    let mut tests: Vec<TestResult> = v
        .get("tests")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| serde_json::from_value::<TestResult>(t.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    if tests.is_empty() {
        tests = repo_tests;
    }
    let bugs = agent_core::schema::parse_bugs(v.get("bugs"));
    let next_action = if bugs.is_empty() { "evaluator" } else { "fixer" };
    let result = AgentResult {
        stage_id: ctx.stage.id.clone(),
        role: ctx.agent.role.clone(),
        status: "completed".into(),
        summary,
        tests,
        bugs,
        next_action: next_action.into(),
        ..Default::default()
    };
    Ok(result)
}

async fn run_evaluator(ctx: &AgentContext, work_dir: &Path) -> Result<AgentResult> {
    let (tests, output) = repo::run_tests(work_dir);
    let expected = r#"A JSON object ONLY:
{"decision":"PASS|FAIL","bugs":[],"evidence":["..."]}"#;
    let prompt = enrich(
        ctx,
        &[("TEST RESULTS", output)],
        &format!(
            "You are the evaluator. INDEPENDENTLY verify: acceptance criteria, application behavior, tests, obvious edge cases.\n\
             {expected}\n\
             FAIL if anything is wrong; list concrete bugs as objects with\n\
             severity/location/description (location defaults to \"work/\" if unsure).\n\
             Output JSON ONLY."
        ),
    );
    let text = llm::complete(&prompt).await?;
    // Lenient parse: bugs/evidence may be malformed from small models; the
    // decision is the only field that must be exact.
    let v: Value = parse_json(&text, "evaluator output")?;
    let decision = v
        .get("decision")
        .and_then(Value::as_str)
        .map(|d| d.trim().to_uppercase())
        .unwrap_or_default();
    if decision != "PASS" && decision != "FAIL" {
        bail!(
            "evaluator decision must be PASS or FAIL (got {:?})",
            v.get("decision")
        );
    }
    let bugs = agent_core::schema::parse_bugs(v.get("bugs"));
    let evidence: Vec<String> = v
        .get("evidence")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let mut result = AgentResult {
        stage_id: ctx.stage.id.clone(),
        role: ctx.agent.role.clone(),
        status: "completed".into(),
        summary: format!("Evaluator decision: {decision}"),
        tests,
        bugs: bugs.clone(),
        next_action: "".into(),
        ..Default::default()
    };
    result.decision = Some(decision);
    result.evidence = Some(evidence);
    Ok(result)
}

async fn run_role(_args: &Args, ctx: &AgentContext, work_dir: &Path) -> Result<AgentResult> {
    match ctx.agent.role.as_str() {
        "planner" => run_planner(ctx).await,
        "developer" | "fixer" => run_developer_or_fixer(ctx, work_dir).await,
        "qa" => run_qa(ctx, work_dir).await,
        "evaluator" => run_evaluator(ctx, work_dir).await,
        other => bail!("unsupported role {other}"),
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let mut ctx = load_context(&args).await?;

    ctx.agent.role = args.role.clone();
    if ctx.agent.personality.is_empty() {
        ctx.agent.personality = personality_for(&args.role);
    }
    ctx.stage.id = args.stage_id.clone();
    if ctx.stage.objective.is_empty() {
        ctx.stage.objective = format!("Run the {role} stage", role = args.role);
    }
    let work_dir = PathBuf::from(&args.work_dir);

    let (_, _) = repo::inspect(&work_dir, &mut ctx.repository);

    let result = match run_role(&args, &ctx, &work_dir).await {
        Ok(r) => r,
        Err(e) => AgentResult::failed(&args.stage_id, &args.role, format!("{e:#}")),
    };

    // Always persist the result locally (logs + audit trail), even when
    // reporting to the orchestrator fails.
    let pretty = serde_json::to_string_pretty(&result)?;
    std::fs::write("result.json", &pretty)?;
    println!("=== AGENT RESULT ===");
    println!("{pretty}");

    if std::env::var("WORKER_URL").is_ok() {
        if let Err(e) = report(&args.goal_id, &result).await {
            eprintln!("WARNING: could not report result: {e}");
        }
    } else {
        eprintln!("WORKER_URL not set; result saved to result.json only");
    }

    Ok(())
}
