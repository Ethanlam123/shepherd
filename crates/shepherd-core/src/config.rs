//! Integration constants. Everything Claude-Code-specific lives here so M2/M3
//! adapters read config, never hardcoded paths. Verified against
//! code.claude.com/docs/en/hooks on 2026-09-14.

use std::path::PathBuf;

/// Default intercept set: consequential tools only. Read-only tools
/// (Read, Glob, Grep, LS, ...) pass through - mirrors Claude Code's own
/// default behavior where safe commands never prompt.
pub const DEFAULT_INTERCEPT_TOOLS: &[&str] = &["Bash", "Edit", "Write", "WebFetch", "WebSearch"];

/// Hook timeout in seconds. Claude Code's own default for command hooks is
/// 600s (v2.1.3+) and a timed-out PreToolUse is a NON-BLOCKING error: the
/// tool call proceeds. We set 900s so a user stepping away does not silently
/// auto-approve; the panel marks timed-out cards "auto-proceeded".
pub const HOOK_TIMEOUT_SECS: u64 = 900;

#[derive(Debug, Clone)]
pub struct ShepherdConfig {
    /// Unix socket shepherd-hook connects to. Bound in a 0700 directory so
    /// only the same user can reach it.
    pub socket_path: PathBuf,
    /// SQLite database for sessions/events/runs/settings (M2).
    pub db_path: PathBuf,
    /// User's global Claude Code settings, where shepherd-hook gets installed.
    pub claude_settings_path: PathBuf,
    /// Per-project session transcripts: <encoded-cwd>/<session-id>.jsonl.
    /// Unofficial format; tail-read for the activity log and token counts.
    pub claude_projects_dir: PathBuf,
    /// Live-session metadata keyed by PID. Unofficial; used for startup
    /// discovery backfill of sessions started before Shepherd.
    pub claude_sessions_dir: PathBuf,
    pub intercept_tools: Vec<String>,
    pub hook_timeout_secs: u64,
    /// Claude Code version the permission round trip was last verified
    /// against (edit-before-approve depends on PreToolUse `updatedInput`,
    /// which had regressions; see anthropics/claude-code#15897).
    pub verified_claude_version: String,
}

impl Default for ShepherdConfig {
    fn default() -> Self {
        let home = home::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let app_support = home.join("Library/Application Support/Shepherd");
        Self {
            socket_path: app_support.join("shepherd.sock"),
            db_path: app_support.join("shepherd.db"),
            claude_settings_path: home.join(".claude/settings.json"),
            claude_projects_dir: home.join(".claude/projects"),
            claude_sessions_dir: home.join(".claude/sessions"),
            intercept_tools: DEFAULT_INTERCEPT_TOOLS.iter().map(|s| s.to_string()).collect(),
            hook_timeout_secs: HOOK_TIMEOUT_SECS,
            verified_claude_version: "2.1.3".to_string(),
        }
    }
}
