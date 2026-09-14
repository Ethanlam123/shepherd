//! shepherd-hook: the Claude Code hook binary. One process per hook event.
//!
//! Contract (fail-open, everywhere): any error, missing socket, or timeout
//! exits 0 with no stdout output, so Claude Code continues its normal
//! permission flow. stdout belongs to the hook protocol; diagnostics go to
//! stderr only.
//!
//! Behavior, per docs verified 2026-09-14 (code.claude.com/docs/en/hooks):
//! - PreToolUse: pass through (no output) when Claude Code itself would not
//!   prompt: bypassPermissions mode, acceptEdits for file tools, or the call
//!   matches one of the user's own permissions.allow rules. Otherwise ask
//!   Shepherd over the socket and translate the decision to
//!   hookSpecificOutput.permissionDecision (allow/deny) + updatedInput for
//!   edited commands. Claude Code still enforces deny/ask rules on top of an
//!   allow, so the user's deny rules are never bypassed.
//! - Notification: forward the idle nudge to Shepherd and exit.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Wait for Shepherd's decision just under the 900s configured hook timeout
/// so we exit first (silently) rather than being killed mid-write.
const DECISION_TIMEOUT: Duration = Duration::from_secs(890);

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let payload = match read_stdin_json() {
        Some(v) => v,
        None => return 0,
    };
    let sock = socket_path();
    match payload.get("hook_event_name").and_then(Value::as_str) {
        Some("PreToolUse") => handle_pre_tool_use(&sock, &payload),
        Some("Notification") => {
            // fire and forget; a dropped nudge costs nothing
            let _ = exchange(
                &sock,
                &json!({
                    "type": "notification",
                    "sessionId": payload.get("session_id"),
                    "message": payload.get("message"),
                }),
            );
            0
        }
        _ => 0,
    }
}

fn handle_pre_tool_use(sock: &std::path::Path, payload: &Value) -> i32 {
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let mode = payload
        .get("permission_mode")
        .and_then(Value::as_str)
        .unwrap_or("");

    // Mirror Claude Code's own no-prompt cases (decision 5).
    if mode == "bypassPermissions" {
        return 0;
    }
    if mode == "acceptEdits" && matches!(tool, "Edit" | "Write" | "NotebookEdit") {
        return 0;
    }
    if allow_rule_matches(payload) {
        return 0; // user's own rule auto-approves; deny/ask still apply
    }

    let request = json!({
        "type": "permission",
        "sessionId": payload.get("session_id"),
        "tool": tool,
        "input": input,
        "cwd": payload.get("cwd"),
    });
    let Some(reply) = exchange(sock, &request) else {
        return 0; // Shepherd down or timed out: normal flow
    };
    match decision_output(&reply, &input) {
        Some(out) => {
            println!("{out}");
            0
        }
        None => 0,
    }
}

/// Translate Shepherd's decision into the Claude Code hook output schema.
/// None (pass) for unknown or pass/timeout decisions.
fn decision_output(reply: &Value, original_input: &Value) -> Option<String> {
    let decision = reply.get("decision").and_then(Value::as_str)?;
    let base = |decision: &str, reason: &str| {
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": decision,
                "permissionDecisionReason": reason,
            }
        })
    };
    match decision {
        "approve" => Some(base("allow", "Approved in Shepherd").to_string()),
        "approve_always" => {
            Some(base("allow", "Always allowed this session (Shepherd)").to_string())
        }
        "approve_edited" => {
            let command = reply.get("command").and_then(Value::as_str)?;
            let mut updated = original_input.as_object().cloned().unwrap_or_default();
            updated.insert("command".into(), json!(command));
            let mut out = base("allow", "Approved with edits in Shepherd");
            out["hookSpecificOutput"]["updatedInput"] = Value::Object(updated);
            Some(out.to_string())
        }
        "deny" => {
            let note = reply
                .get("note")
                .and_then(Value::as_str)
                .filter(|n| !n.trim().is_empty())
                .unwrap_or("Denied in Shepherd - take a different approach.");
            Some(base("deny", note).to_string())
        }
        _ => None,
    }
}

// ---------- permissions.allow rule matching (the mirror of decision 5) ----------

/// True when the call matches an allow rule from the user's global settings
/// or the session project's settings files.
fn allow_rule_matches(payload: &Value) -> bool {
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let input = payload.get("tool_input");
    let cwd = payload.get("cwd").and_then(Value::as_str).unwrap_or("");
    let mut rules: Vec<String> = Vec::new();
    let home = std::env::var("HOME").unwrap_or_default();
    collect_allow_rules(
        &PathBuf::from(&home).join(".claude/settings.json"),
        &mut rules,
    );
    let project = PathBuf::from(cwd);
    collect_allow_rules(&project.join(".claude/settings.json"), &mut rules);
    collect_allow_rules(&project.join(".claude/settings.local.json"), &mut rules);
    rules.iter().any(|r| rule_matches(r, tool, input))
}

fn collect_allow_rules(path: &std::path::Path, rules: &mut Vec<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    if let Some(list) = v.pointer("/permissions/allow").and_then(Value::as_array) {
        rules.extend(list.iter().filter_map(Value::as_str).map(String::from));
    }
}

/// One rule, matched with Claude Code's documented semantics (settings
/// reference + permissions docs, verified 2026-09-14). The goal is only to
/// skip cards the user's rules would auto-approve anyway; Claude Code still
/// evaluates its real rules on top of whatever we decide.
fn rule_matches(rule: &str, tool: &str, input: Option<&Value>) -> bool {
    let Some((rule_tool, spec)) = split_rule(rule) else {
        return false;
    };
    // File rules live on Edit only: "Edit(path) rules govern all built-in
    // tools that write files, including Write and NotebookEdit; a Write(path)
    // rule is never matched by the file permission checks."
    let effective_tool = match tool {
        "Write" | "NotebookEdit" => "Edit",
        t => t,
    };
    if rule_tool != effective_tool {
        return false;
    }
    let Some(spec) = spec else { return true }; // bare "Bash" allows all Bash
    let Some(input) = input else { return false };
    match effective_tool {
        "Bash" => input
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|cmd| bash_spec_matches(spec, cmd)),
        "Edit" => input
            .get("file_path")
            .and_then(Value::as_str)
            .is_some_and(|path| path_spec_matches(spec, path)),
        "WebFetch" => input
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(|url| domain_spec_matches(spec, url)),
        _ => false,
    }
}

/// Bash specs: exact, legacy "prefix:*" (any continuation), and "*"-globs
/// where each "*" matches anything ("npm run *", "git -C * status *").
fn bash_spec_matches(spec: &str, cmd: &str) -> bool {
    if let Some(prefix) = spec.strip_suffix(":*") {
        return cmd.starts_with(prefix);
    }
    if !spec.contains('*') {
        return cmd == spec;
    }
    // glob: literal segments must appear in order; anchored at both ends
    let mut rest = cmd;
    let parts: Vec<&str> = spec.split('*').collect();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue; // leading/trailing "*" or "**": unanchored side
        }
        let found = if i == 0 {
            rest.starts_with(*part).then(|| &rest[part.len()..])
        } else if i == parts.len() - 1 {
            rest.ends_with(*part).then_some("")
        } else {
            rest.find(*part).map(|at| &rest[at + part.len()..])
        };
        match found {
            Some(next) => rest = next,
            None => return false,
        }
    }
    true
}

/// File specs: "path/**" matches a subtree, other paths match exactly.
/// Absolute ("//x", "/x") compare directly; relative specs match the
/// trailing path components (gitignore-style against the cwd).
// ponytail: relative anchors approximate gitignore; refine if real rules miss
fn path_spec_matches(spec: &str, path: &str) -> bool {
    if let Some(dir) = spec.strip_suffix("/**") {
        let dir = dir.trim_end_matches('/');
        return path == dir || path.starts_with(&format!("{dir}/"));
    }
    if spec.starts_with('/') {
        return path == spec.trim_start_matches("//") || path == spec;
    }
    // relative: match trailing components ("/w/src/a.ts" vs "src/a.ts")
    let suffix = format!("/{}", spec.trim_start_matches("./"));
    path == spec || path.ends_with(&suffix)
}

/// Domain specs: "domain:host" (current docs) or legacy bare "host";
/// the host itself, its www variant, and subdomains all match.
fn domain_spec_matches(spec: &str, url: &str) -> bool {
    let domain = spec.strip_prefix("domain:").unwrap_or(spec);
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("");
    let host = host.strip_prefix("www.").unwrap_or(host);
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// "Tool(...)" -> ("Tool", Some("...")), "Tool" -> ("Tool", None).
fn split_rule(rule: &str) -> Option<(&str, Option<&str>)> {
    let rule = rule.trim();
    match rule.split_once('(') {
        Some((tool, rest)) if rest.ends_with(')') => Some((tool, Some(&rest[..rest.len() - 1]))),
        _ => Some((rule, None)),
    }
}

// ---------- socket exchange ----------

fn socket_path() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--sock") {
        if let Some(p) = args.get(i + 1) {
            return PathBuf::from(p);
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join("Library/Application Support/Shepherd/shepherd.sock")
}

/// Send one JSON request line, read one JSON reply. Any failure -> None.
/// The write side stays open: Shepherd watches it to learn when this
/// process dies while waiting (Claude Code kills us at its hook timeout).
fn exchange(sock: &std::path::Path, request: &Value) -> Option<Value> {
    let mut stream = UnixStream::connect(sock).ok()?;
    stream.set_read_timeout(Some(DECISION_TIMEOUT)).ok()?;
    let mut line = serde_json::to_string(request).ok()?;
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).ok()?; // Shepherd closes after replying
    serde_json::from_str(buf.trim()).ok()
}

fn read_stdin_json() -> Option<Value> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok()?;
    serde_json::from_str(&buf).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(pairs: &[(&str, Value)]) -> Value {
        let mut map = serde_json::Map::new();
        for (k, v) in pairs {
            map.insert((*k).into(), v.clone());
        }
        Value::Object(map)
    }

    #[test]
    fn bash_rules_exact_prefix_glob_and_bare() {
        let i = Some(&json!({"command": "pnpm test auth/"}));
        assert!(rule_matches("Bash(pnpm test auth/)", "Bash", i), "exact");
        assert!(
            rule_matches("Bash(pnpm test:*)", "Bash", i),
            "legacy prefix"
        );
        assert!(
            rule_matches("Bash(pnpm:*)", "Bash", i),
            "plain string prefix"
        );
        assert!(rule_matches("Bash(pnpm *)", "Bash", i), "glob prefix");
        assert!(
            !rule_matches("Bash(pnpmx *)", "Bash", i),
            "glob needs the space"
        );
        assert!(rule_matches("Bash", "Bash", i), "bare tool allows all");
        assert!(!rule_matches("Edit", "Bash", i), "tool mismatch");
        // mid-command glob, the docs' "git -C * status *" case
        let g = Some(&json!({"command": "git -C /repo status --short"}));
        assert!(rule_matches("Bash(git -C * status *)", "Bash", g));
        assert!(!rule_matches("Bash(git -C * push *)", "Bash", g));
        // prefix rules also match the bare prefix command itself
        assert!(rule_matches(
            "Bash(pnpm test:*)",
            "Bash",
            Some(&json!({"command": "pnpm test"}))
        ));
    }

    #[test]
    fn edit_rules_exact_dir_relative_and_write_via_edit() {
        let i = Some(&json!({"file_path": "/w/src/auth/tokens.ts"}));
        assert!(rule_matches("Edit(/w/src/auth/tokens.ts)", "Edit", i));
        assert!(rule_matches("Edit(/w/src/**)", "Edit", i));
        assert!(!rule_matches("Edit(/w/other/**)", "Edit", i));
        assert!(!rule_matches("Edit(/w/src/auth/tokens.ts.bak)", "Edit", i));
        // relative specs match trailing components (gitignore-style)
        assert!(rule_matches("Edit(src/auth/tokens.ts)", "Edit", i));
        assert!(rule_matches("Edit(./src/auth/tokens.ts)", "Edit", i));
        // gitignore-style: trailing components match at any depth
        assert!(rule_matches("Edit(auth/tokens.ts)", "Edit", i));
        // Edit rules govern Write; Write rules never match
        let w = Some(&json!({"file_path": "/w/src/new.ts"}));
        assert!(rule_matches("Edit(/w/src/**)", "Write", w));
        assert!(!rule_matches("Write(/w/src/**)", "Write", w));
    }

    #[test]
    fn webfetch_domain_rules_with_subdomains() {
        let i = Some(&json!({"url": "https://code.claude.com/docs/en/hooks"}));
        assert!(
            rule_matches("WebFetch(domain:claude.com)", "WebFetch", i),
            "current syntax"
        );
        assert!(
            rule_matches("WebFetch(claude.com)", "WebFetch", i),
            "legacy syntax"
        );
        assert!(rule_matches(
            "WebFetch(domain:code.claude.com)",
            "WebFetch",
            i
        ));
        assert!(!rule_matches("WebFetch(example.com)", "WebFetch", i));
        assert!(!rule_matches(
            "WebFetch(claude.com.evil.net)",
            "WebFetch",
            i
        ));
        let www = Some(&json!({"url": "https://www.claude.com/x"}));
        assert!(
            rule_matches("WebFetch(claude.com)", "WebFetch", www),
            "www stripped"
        );
    }

    #[test]
    fn decision_outputs_match_the_hook_schema() {
        let original = json!({"command": "pnpm test", "description": "run tests"});
        let approve = decision_output(&json!({"decision": "approve"}), &original).unwrap();
        let v: Value = serde_json::from_str(&approve).unwrap();
        assert_eq!(
            v.pointer("/hookSpecificOutput/permissionDecision").unwrap(),
            "allow"
        );

        let edited = decision_output(
            &json!({"decision": "approve_edited", "command": "pnpm test --filter unit"}),
            &original,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&edited).unwrap();
        assert_eq!(
            v.pointer("/hookSpecificOutput/updatedInput/command")
                .unwrap(),
            "pnpm test --filter unit"
        );
        assert_eq!(
            v.pointer("/hookSpecificOutput/updatedInput/description")
                .unwrap(),
            "run tests",
            "other input fields survive the edit"
        );

        let deny = decision_output(
            &json!({"decision": "deny", "note": "use git clean"}),
            &original,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&deny).unwrap();
        assert_eq!(
            v.pointer("/hookSpecificOutput/permissionDecision").unwrap(),
            "deny"
        );
        assert_eq!(
            v.pointer("/hookSpecificOutput/permissionDecisionReason")
                .unwrap(),
            "use git clean"
        );

        let default_deny = decision_output(&json!({"decision": "deny"}), &original).unwrap();
        let v: Value = serde_json::from_str(&default_deny).unwrap();
        assert_eq!(
            v.pointer("/hookSpecificOutput/permissionDecisionReason")
                .unwrap(),
            "Denied in Shepherd - take a different approach."
        );

        assert!(decision_output(&json!({"decision": "pass"}), &original).is_none());
        assert!(decision_output(&json!({"decision": "timeout"}), &original).is_none());
        assert!(decision_output(&json!({}), &original).is_none());
    }

    #[test]
    fn accept_edits_and_bypass_pass_through_in_run() {
        // exercised indirectly through handle_pre_tool_use: a bypassed mode
        // returns 0 without ever touching the (nonexistent) socket
        let payload = input(&[
            ("hook_event_name", json!("PreToolUse")),
            ("tool_name", json!("Bash")),
            ("tool_input", json!({"command": "rm -rf /"})),
            ("permission_mode", json!("bypassPermissions")),
            ("session_id", json!("s")),
        ]);
        assert_eq!(
            handle_pre_tool_use(std::path::Path::new("/nonexistent.sock"), &payload),
            0
        );
    }
}
