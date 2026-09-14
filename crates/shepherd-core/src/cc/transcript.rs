//! Transcript JSONL parsing. One line per JSON object; we extract only what
//! the panel shows: tool activity labels, the AI-generated title, token
//! usage, the first user prompt (title fallback), and assistant text (final
//! outcome). Line shapes verified against Claude Code 2.1.270 transcripts.

use serde_json::Value;

/// The parts of one transcript line Shepherd cares about.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// An assistant message: any tool uses, trailing text, and the usage
    /// snapshot (context = input + cache read/creation; output accumulates).
    Assistant {
        tools: Vec<ToolUse>,
        text: Option<String>,
        context_tokens: u64,
        output_tokens: u64,
    },
    /// A real user prompt (string content, not a tool result).
    UserPrompt(String),
    /// `{"type":"ai-title",...}`: Claude's own summary of the session.
    AiTitle(String),
    /// Everything else (attachment, mode, last-prompt, file-history, ...).
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolUse {
    pub name: String,
    /// Panel label, e.g. "Bash pnpm test" or "Edit src/main.rs +12 -3".
    pub label: String,
    /// Edit/Write target, for the run's files-touched list.
    pub file: Option<String>,
}

/// Primary-argument keys we surface in a tool label, in preference order.
const LABEL_KEYS: &[&str] = &["command", "file_path", "query", "url", "pattern", "path"];

/// Label length cap; transcripts regularly contain kilobyte-long commands.
const LABEL_MAX: usize = 60;

pub fn parse_line(raw: &str) -> Line {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return Line::Other;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("ai-title") => match v.get("aiTitle").and_then(Value::as_str) {
            Some(t) => Line::AiTitle(t.to_string()),
            None => Line::Other,
        },
        // sidechain lines are subagent internals; skip the noise
        Some("assistant")
            if !v
                .get("isSidechain")
                .and_then(Value::as_bool)
                .unwrap_or(false) =>
        {
            parse_assistant(&v)
        }
        Some("user") if !is_tool_result(&v) && !is_meta(&v) => {
            match v.pointer("/message/content").and_then(Value::as_str) {
                Some(text) => Line::UserPrompt(text.to_string()),
                None => Line::Other,
            }
        }
        _ => Line::Other,
    }
}

fn parse_assistant(v: &Value) -> Line {
    let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else {
        return Line::Other;
    };
    let mut tools = Vec::new();
    let mut text = None;
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                if let Some(name) = block.get("name").and_then(Value::as_str) {
                    tools.push(tool_label(name, block.get("input").unwrap_or(&Value::Null)));
                }
            }
            Some("text") => {
                let t = block.get("text").and_then(Value::as_str).unwrap_or("");
                if !t.trim().is_empty() {
                    text = Some(t.to_string());
                }
            }
            _ => {}
        }
    }
    let usage = v.pointer("/message/usage");
    let num = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let context_tokens =
        num("input_tokens") + num("cache_read_input_tokens") + num("cache_creation_input_tokens");
    let output_tokens = num("output_tokens");
    if tools.is_empty() && text.is_none() && context_tokens == 0 && output_tokens == 0 {
        return Line::Other;
    }
    Line::Assistant {
        tools,
        text,
        context_tokens,
        output_tokens,
    }
}

/// Label + file for a tool use, shared by transcript activity and hook
/// permission cards.
pub fn tool_label(name: &str, input: &Value) -> ToolUse {
    let name = name.to_string();
    let input = input.clone();
    let arg = LABEL_KEYS
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str));
    let label = match (name.as_str(), arg) {
        // Edit/Write show a +added -removed diff hint like the prototype
        ("Edit" | "Write", Some(file)) => {
            let (add, del) = diff_hint(&input);
            format!("Edit {file} +{add} -{del}")
        }
        (name, Some(arg)) => format!("{name} {}", truncate(arg)),
        (name, None) => name.to_string(),
    };
    let file = matches!(name.as_str(), "Edit" | "Write")
        .then(|| {
            input
                .get("file_path")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .flatten();
    ToolUse { name, label, file }
}

/// Added/removed line counts for Edit-style inputs (new_string/old_string).
fn diff_hint(input: &Value) -> (u64, u64) {
    let lines = |k: &str| {
        input
            .get(k)
            .and_then(Value::as_str)
            .map(|s| s.split('\n').count() as u64)
            .unwrap_or(0)
    };
    let del = lines("old_string");
    let add = lines("new_string").max(1);
    (add, del)
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= LABEL_MAX {
        s.to_string()
    } else {
        let cut: String = s.chars().take(LABEL_MAX - 1).collect();
        format!("{cut}\u{2026}")
    }
}

fn is_tool_result(v: &Value) -> bool {
    v.pointer("/message/content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type") == Some(&Value::from("tool_result")))
        })
}

fn is_meta(v: &Value) -> bool {
    v.get("isMeta").and_then(Value::as_bool).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // fixtures copied from a real 2.1.270 transcript, trimmed to the fields
    // the parser reads

    fn assistant_line(content: &str, usage: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-09-14T07:27:26.793Z","message":{{"role":"assistant","model":"glm-5.3","content":{content},"usage":{usage}}},"sessionId":"s1","cwd":"/w"}}"#
        )
    }

    const USAGE: &str = r#"{"input_tokens":632,"cache_creation_input_tokens":0,"cache_read_input_tokens":64704,"output_tokens":489}"#;

    #[test]
    fn parses_tool_use_label_and_file() {
        let line = assistant_line(
            r#"[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"pnpm test auth/","description":"run tests"}}]"#,
            USAGE,
        );
        match parse_line(&line) {
            Line::Assistant {
                tools,
                text,
                context_tokens,
                output_tokens,
            } => {
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0].name, "Bash");
                assert_eq!(tools[0].label, "Bash pnpm test auth/");
                assert!(tools[0].file.is_none());
                assert!(text.is_none());
                assert_eq!(context_tokens, 632 + 64_704);
                assert_eq!(output_tokens, 489);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn edit_shows_diff_hint_and_file() {
        let line = assistant_line(
            r#"[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"src/auth/tokens.ts","old_string":"a\nb\nc","new_string":"a\nb\nc\nd\ne"}}]"#,
            USAGE,
        );
        match parse_line(&line) {
            Line::Assistant { tools, .. } => {
                assert_eq!(tools[0].label, "Edit src/auth/tokens.ts +5 -3");
                assert_eq!(tools[0].file.as_deref(), Some("src/auth/tokens.ts"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn long_command_is_truncated() {
        let long = "x".repeat(80);
        let line = assistant_line(
            &format!(
                r#"[{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"{long}"}}}}]"#
            ),
            USAGE,
        );
        match parse_line(&line) {
            Line::Assistant { tools, .. } => {
                // "Bash " prefix + truncated arg capped at LABEL_MAX chars
                assert_eq!(tools[0].label.chars().count(), "Bash ".len() + LABEL_MAX);
                assert!(tools[0].label.ends_with('\u{2026}'));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn assistant_text_and_tools_coexist() {
        let line = assistant_line(
            r#"[{"type":"text","text":"thinking out loud"},{"type":"tool_use","id":"t1","name":"WebSearch","input":{"query":"tauri tray badge"}}]"#,
            USAGE,
        );
        match parse_line(&line) {
            Line::Assistant { tools, text, .. } => {
                assert_eq!(text.as_deref(), Some("thinking out loud"));
                assert_eq!(tools[0].label, "WebSearch tauri tray badge");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn user_prompt_is_string_content_only() {
        let real = r#"{"type":"user","message":{"role":"user","content":"fix the flaky tests"},"sessionId":"s1"}"#;
        assert_eq!(
            parse_line(real),
            Line::UserPrompt("fix the flaky tests".into())
        );

        let tool_result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]},"toolUseResult":{}}"#;
        assert_eq!(parse_line(tool_result), Line::Other);

        let meta =
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"/compact"}}"#;
        assert_eq!(parse_line(meta), Line::Other);
    }

    #[test]
    fn ai_title_line() {
        let line =
            r#"{"type":"ai-title","aiTitle":"Shepherd macOS menu-bar app","sessionId":"s1"}"#;
        assert_eq!(
            parse_line(line),
            Line::AiTitle("Shepherd macOS menu-bar app".into())
        );
    }

    #[test]
    fn sidechain_and_noise_lines_are_other() {
        let sidechain = assistant_line(
            r#"[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"x"}}]"#,
            USAGE,
        )
        .replace(
            "\"type\":\"assistant\"",
            "\"type\":\"assistant\",\"isSidechain\":true",
        );
        assert_eq!(parse_line(&sidechain), Line::Other);

        for line in [
            r#"{"type":"mode","mode":"default"}"#,
            r#"{"type":"last-prompt","prompt":"hi"}"#,
            r#"{"type":"attachment","attachment":{}}"#,
            r#"{"type":"system","subtype":"informational","content":"ecc warning"}"#,
            "not json at all",
        ] {
            assert_eq!(parse_line(line), Line::Other, "{line}");
        }
    }
}
