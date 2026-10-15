//! Turns a whoami tool's answer into a short, safe label such as `Acme` or `Acme, Beta, Gamma +2`.
//! Answer shapes differ per server and are undocumented, so registry pointers are tried first,
//! then a shallow look for a name-like field.

use anyhow::{Context, bail};
use rmcp::model::CallToolResult;
use serde_json::Value;

const MAX_ANSWER_BYTES: usize = 64 * 1024;
const MAX_SHOWN_CHARS: usize = 80;
const MAX_LISTED_NAMES: usize = 3;
/// Checked in this order; the first one holding a string names the account.
const NAME_FIELDS: &[&str] = &["name", "displayName", "slug", "login", "email"];

/// The label to show for the account behind `result`, or why none could be found.
pub fn shown_from(result: &CallToolResult, show: &[&str]) -> anyhow::Result<String> {
    let answer = answer_value(result)?;
    let described = match &answer {
        Value::String(text) => Some(text.clone()),
        _ => describe_with_pointers(&answer, show).or_else(|| summarize(&answer)),
    };
    let shown = described
        .map(|text| sanitize(&text))
        .filter(|text| !text.is_empty());
    shown.context("the answer names no account")
}

/// `structuredContent` when present, else the first text block as JSON, else that text itself.
fn answer_value(result: &CallToolResult) -> anyhow::Result<Value> {
    if let Some(structured) = &result.structured_content {
        let size = serde_json::to_string(structured)
            .context("measuring the structured answer")?
            .len();
        reject_oversized(size)?;
        return Ok(structured.clone());
    }
    let text = result
        .content
        .iter()
        .find_map(|block| block.as_text())
        .map(|text| text.text.as_str())
        .context("the answer holds no text")?;
    reject_oversized(text.len())?;
    let parsed = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()));
    Ok(parsed)
}

fn reject_oversized(size: usize) -> anyhow::Result<()> {
    if size > MAX_ANSWER_BYTES {
        bail!("the answer is {size} bytes, more than the {MAX_ANSWER_BYTES} plugbay reads");
    }
    Ok(())
}

fn describe_with_pointers(answer: &Value, show: &[&str]) -> Option<String> {
    show.iter()
        .filter_map(|pointer| answer.pointer(pointer))
        .find_map(describe)
}

fn describe(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => non_empty(text),
        Value::Number(number) => Some(number.to_string()),
        Value::Array(items) => list_names(items),
        Value::Object(_) => name_field(value),
        Value::Bool(_) | Value::Null => None,
    }
}

/// A name field of the answer itself, else the names in a top-level list or in its first list field.
fn summarize(answer: &Value) -> Option<String> {
    match answer {
        Value::Array(items) => list_names(items),
        Value::Object(fields) => {
            if let Some(name) = name_field(answer) {
                return Some(name);
            }
            let first_list = fields.values().find_map(Value::as_array)?;
            list_names(first_list)
        }
        other => describe(other),
    }
}

fn name_field(object: &Value) -> Option<String> {
    NAME_FIELDS
        .iter()
        .filter_map(|field| object.get(field)?.as_str())
        .find_map(non_empty)
}

/// Several orgs or teams: the first few names, then how many more.
fn list_names(items: &[Value]) -> Option<String> {
    let names: Vec<String> = items
        .iter()
        .filter_map(|item| match item {
            Value::String(text) => non_empty(text),
            Value::Object(_) => name_field(item),
            _ => None,
        })
        .collect();
    if names.is_empty() {
        return None;
    }
    let mut listed = names
        .iter()
        .take(MAX_LISTED_NAMES)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > MAX_LISTED_NAMES {
        listed.push_str(&format!(" +{}", names.len() - MAX_LISTED_NAMES));
    }
    Some(listed)
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Upstream text ends up in terminals and agent instructions: no control or direction-changing
/// characters, one line, bounded length.
fn sanitize(text: &str) -> String {
    let visible: String = text
        .chars()
        .filter(|character| !is_invisible_formatting(*character))
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let one_line = visible.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&one_line)
}

fn is_invisible_formatting(character: char) -> bool {
    matches!(
        character,
        '\u{061C}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_SHOWN_CHARS {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(MAX_SHOWN_CHARS - 1).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use rmcp::model::ContentBlock;
    use serde_json::json;

    use super::*;

    #[test]
    fn vercel_teams_are_found_under_result() {
        let answer = json!({ "result": { "teams": [{ "name": "Acme", "slug": "acme" }], "pagination": { "count": 1 } } });
        let shown = describe_with_pointers(&answer, &["/result/teams", "/teams"]);
        assert_eq!(shown.as_deref(), Some("Acme"));
    }

    fn text_answer(text: &str) -> CallToolResult {
        CallToolResult::success(vec![ContentBlock::text(text)])
    }

    #[test]
    fn pointer_picks_the_workspace_name() {
        let answer =
            text_answer(r#"{"id":"f655","name":"Initech","url":"https://linear.app/initech"}"#);
        assert_eq!(shown_from(&answer, &["/name"]).unwrap(), "Initech");
    }

    #[test]
    fn structured_content_wins_over_text() {
        let mut answer = text_answer(r#"{"login":"from-text"}"#);
        answer.structured_content = Some(json!({ "login": "from-structure" }));
        assert_eq!(shown_from(&answer, &["/login"]).unwrap(), "from-structure");
    }

    #[test]
    fn later_pointers_are_tried_when_earlier_ones_miss() {
        let answer = text_answer(r#"{"business_profile":{"name":"Acme Ltd"}}"#);
        let shown = shown_from(&answer, &["/display_name", "/business_profile/name"]).unwrap();
        assert_eq!(shown, "Acme Ltd");
    }

    #[test]
    fn name_like_field_is_the_fallback() {
        let answer = text_answer(r#"{"id":7,"displayName":"Mihai","email":"m@example.test"}"#);
        assert_eq!(shown_from(&answer, &["/missing"]).unwrap(), "Mihai");
    }

    #[test]
    fn several_organisations_are_listed_with_a_count() {
        let answer = text_answer(
            r#"[{"name":"Acme"},{"name":"Beta"},{"slug":"gamma"},{"name":"Delta"},{"name":"Echo"}]"#,
        );
        assert_eq!(shown_from(&answer, &[]).unwrap(), "Acme, Beta, gamma +2");
    }

    #[test]
    fn first_list_field_is_summarised() {
        let answer = text_answer(r#"{"teams":[{"name":"Acme"}],"total":1}"#);
        assert_eq!(shown_from(&answer, &[]).unwrap(), "Acme");
    }

    #[test]
    fn pointer_to_a_list_lists_its_names() {
        let answer = text_answer(r#"{"teams":[{"name":"Acme"},{"name":"Beta"}]}"#);
        assert_eq!(shown_from(&answer, &["/teams"]).unwrap(), "Acme, Beta");
    }

    #[test]
    fn plain_text_is_used_as_is() {
        let answer = text_answer("Logged in to   Acme\nworkspace");
        assert_eq!(
            shown_from(&answer, &["/name"]).unwrap(),
            "Logged in to Acme workspace"
        );
    }

    #[test]
    fn answers_without_a_name_are_an_error() {
        let answer = text_answer(r#"{"id":7,"active":true}"#);
        assert!(shown_from(&answer, &["/name"]).is_err());
        assert!(shown_from(&CallToolResult::success(Vec::new()), &[]).is_err());
    }

    #[test]
    fn oversized_answers_are_refused() {
        let answer = text_answer(&"x".repeat(MAX_ANSWER_BYTES + 1));
        assert!(shown_from(&answer, &[]).is_err());
    }

    #[test]
    fn control_and_bidi_characters_are_stripped() {
        let shown = sanitize("Ac\u{202E}me\u{0007}\tCorp\u{200B}");
        assert_eq!(shown, "Acme Corp");
    }

    #[test]
    fn long_names_are_truncated() {
        let shown = sanitize(&"a".repeat(200));
        assert_eq!(shown.chars().count(), MAX_SHOWN_CHARS);
        assert!(shown.ends_with('…'));
    }
}
