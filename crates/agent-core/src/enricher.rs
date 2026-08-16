//! The Enricher: turns a structured [`AgentContext`] into one small, focused
//! prompt. Optimized for small/weak models with limited context windows:
//! only the 9 essential sections are emitted, everything else is truncated.

use crate::schema::AgentContext;

const MAX_HISTORY: usize = 2500;
const MAX_REPO: usize = 6000;
const MAX_GOAL: usize = 800;

/// Truncate to `max` characters, keeping the head (most relevant part).
pub fn truncate(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}\n... [TRUNCATED {} chars]", len - max)
    }
}

fn bullets(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        items
            .iter()
            .map(|i| format!("- {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn section(title: &str, body: &str) -> String {
    format!("{title}\n{body}\n")
}

/// Build the prompt. `extras` are runner-provided ground truth sections
/// (e.g. TEST RESULTS) inserted before EXPECTED OUTPUT.
pub fn enrich(ctx: &AgentContext, extras: &[(&str, String)], expected_output: &str) -> String {
    let mut out = String::new();

    let role = format!(
        "{} — {} (skills: {})",
        ctx.agent.role,
        ctx.agent.personality,
        ctx.agent.skills.join(", ")
    );
    out.push_str(&section("ROLE", &role));

    out.push_str(&section("GOAL", &truncate(&ctx.goal, MAX_GOAL)));

    let task = if ctx.stage.objective.is_empty() {
        ctx.tasks.join("; ")
    } else {
        ctx.stage.objective.clone()
    };
    out.push_str(&section("CURRENT TASK", &truncate(&task, 2000)));

    out.push_str(&section(
        "ACCEPTANCE CRITERIA",
        &bullets(&ctx.acceptance_criteria),
    ));

    let history = format!(
        "SUMMARY:\n{}\nPREVIOUS RESULTS:\n{}",
        truncate(&ctx.history.summary, 1200),
        bullets(&ctx.history.previous_results)
    );
    out.push_str(&section("RELEVANT HISTORY", &truncate(&history, MAX_HISTORY)));

    let repo = format!(
        "COMMIT: {}\nRECENT COMMITS:\n{}\nWORK DIRECTORY FILES:\n{}\nFILE CONTENTS:\n{}",
        ctx.repository.commit_hash,
        bullets(&ctx.repository.recent_commits),
        bullets(&ctx.repository.work_dir_files),
        truncate(&ctx.repository.files.join("\n\n=====\n\n"), 4000)
    );
    out.push_str(&section("REPOSITORY STATE", &truncate(&repo, MAX_REPO)));

    out.push_str(&section("TOOLS", &ctx.tools.join(", ")));

    out.push_str(&section("GUARDRAILS", &bullets(&ctx.guardrails)));

    for (title, body) in extras {
        out.push_str(&section(title, &truncate(body, 3000)));
    }

    out.push_str(&section("EXPECTED OUTPUT", expected_output));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::*;

    fn ctx() -> AgentContext {
        AgentContext {
            goal: "Make the calculator correct".into(),
            project: "factory-poc".into(),
            agent: Agent {
                role: "developer".into(),
                personality: "careful and concise".into(),
                skills: vec!["javascript".into()],
            },
            stage: Stage {
                id: "1".into(),
                objective: "Fix add()".into(),
            },
            tasks: vec!["write code".into()],
            acceptance_criteria: vec!["all tests pass".into()],
            guardrails: vec!["only touch work/".into()],
            environment: Environment {
                work_dir: "work".into(),
                llm_provider: "zen".into(),
                model: "m".into(),
                retry_limit: 3,
                attempt: 1,
            },
            history: History {
                summary: "n/a".into(),
                previous_results: vec![],
            },
            repository: Repository {
                commit_hash: "abc".into(),
                recent_commits: vec!["abc fix".into()],
                work_dir_files: vec!["work/calc.js".into()],
                files: vec!["function add(a,b){return a-b;}".into()],
                test_output: String::new(),
            },
            tools: vec!["git".into(), "node --test".into()],
            mcp: vec![],
        }
    }

    #[test]
    fn emits_all_nine_sections() {
        let prompt = enrich(&ctx(), &[], "RESPOND WITH JSON ONLY");
        for section in [
            "ROLE",
            "GOAL",
            "CURRENT TASK",
            "ACCEPTANCE CRITERIA",
            "RELEVANT HISTORY",
            "REPOSITORY STATE",
            "TOOLS",
            "GUARDRAILS",
            "EXPECTED OUTPUT",
        ] {
            assert!(prompt.contains(section), "missing section {section}");
        }
        assert!(prompt.contains("EXPECTED OUTPUT\nRESPOND WITH JSON ONLY"));
    }

    #[test]
    fn truncates_long_history() {
        let mut c = ctx();
        c.history.previous_results = vec!["x".repeat(5000)];
        let prompt = enrich(&c, &[], "");
        assert!(prompt.contains("[TRUNCATED"));
    }

    #[test]
    fn appends_extras_before_expected_output() {
        let prompt = enrich(
            &ctx(),
            &[("TEST RESULTS", "pass 1 fail 1".into())],
            "JSON ONLY",
        );
        let test_pos = prompt.find("TEST RESULTS").unwrap();
        let exp_pos = prompt.find("EXPECTED OUTPUT").unwrap();
        assert!(test_pos < exp_pos);
    }
}