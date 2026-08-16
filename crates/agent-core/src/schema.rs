//! The generic agent schema. Every role (planner, developer, qa, evaluator,
//! fixer) is expressed with exactly the same structure. See
//! `schema/agent_context.yml` for the annotated YAML view of this schema.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub role: String,
    pub personality: String,
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
    pub id: String,
    pub objective: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Environment {
    pub work_dir: String,
    pub llm_provider: String,
    pub model: String,
    pub retry_limit: usize,
    pub attempt: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct History {
    pub summary: String,
    pub previous_results: Vec<String>,
}

/// Repository state is filled in by the agent runner itself (local truth),
/// never trusted from the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Repository {
    pub commit_hash: String,
    pub recent_commits: Vec<String>,
    pub work_dir_files: Vec<String>,
    /// Full contents of the (small) files the agent is allowed to touch.
    pub files: Vec<String>,
    /// Output of the last test run (empty if no tests ran yet).
    pub test_output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContext {
    pub goal: String,
    pub project: String,
    pub agent: Agent,
    pub stage: Stage,
    pub tasks: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub guardrails: Vec<String>,
    pub environment: Environment,
    pub history: History,
    pub repository: Repository,
    pub tools: Vec<String>,
    pub mcp: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Bug {
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub description: String,
}

/// Leniently parse a `bugs` field. Small models frequently emit bugs as
/// plain strings or drop required fields; this normalizes those into
/// well-formed `Bug`s instead of failing the whole stage.
pub fn parse_bugs(v: Option<&serde_json::Value>) -> Vec<Bug> {
    let Some(serde_json::Value::Array(items)) = v else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|i| match i {
            serde_json::Value::String(s) => Some(Bug {
                severity: "unknown".into(),
                location: "work/".into(),
                description: s.clone(),
            }),
            other => serde_json::from_value(other.clone()).ok(),
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
}

/// A single stage produced by the planner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStage {
    pub id: String,
    pub role: String,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub stages: Vec<PlanStage>,
}

/// The evaluator output. Only the evaluator may decide PASS/FAIL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalResult {
    pub decision: String, // "PASS" | "FAIL"
    pub bugs: Vec<Bug>,
    pub evidence: Vec<String>,
}

/// The structured result every agent produces (requirement 3).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentResult {
    pub stage_id: String,
    pub role: String,
    pub status: String, // "completed" | "failed"
    pub summary: String,
    pub commit: Option<String>,
    #[serde(default)]
    pub tests: Vec<TestResult>,
    #[serde(default)]
    pub bugs: Vec<Bug>,
    pub next_action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Vec<String>>,
}

pub const VALID_ROLES: &[&str] = &["planner", "developer", "qa", "evaluator", "fixer"];

impl AgentResult {
    pub fn failed(stage_id: &str, role: &str, summary: String) -> Self {
        Self {
            stage_id: stage_id.to_string(),
            role: role.to_string(),
            status: "failed".to_string(),
            summary,
            ..Default::default()
        }
    }
}