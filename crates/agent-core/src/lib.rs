//! agent-core: the generic, role-agnostic core of the factory POC.
//!
//! * [`schema`]  – the generic agent schema (works for any role)
//! * [`enricher`] – turns a structured context into a small, focused prompt
//! * [`llm`]     – thin wrapper over Rig (OpenCode Zen free / Groq free / local mock)

pub mod enricher;
pub mod llm;
pub mod schema;

/// Extract the first balanced JSON object out of a model response.
/// Small models often wrap JSON in prose or markdown fences; this tolerates that.
pub fn extract_json(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(text[start..=end].to_string())
}

#[cfg(test)]
mod tests {
    use super::extract_json;

    #[test]
    fn extracts_json_from_fenced_prose() {
        let s = "Sure! Here you go:\n```json\n{\"decision\":\"PASS\"}\n```\nHope this helps!";
        assert_eq!(extract_json(s).unwrap(), "{\"decision\":\"PASS\"}");
    }

    #[test]
    fn handles_plain_json() {
        assert_eq!(extract_json("{\"a\":1}").unwrap(), "{\"a\":1}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(extract_json("no json here").is_none());
    }
}