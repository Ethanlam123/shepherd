//! Discovery of live Claude Code sessions from the unofficial
//! `~/.claude/sessions/<pid>.json` files, and mapping to per-project
//! transcripts. Format verified against Claude Code 2.1.270 on 2026-09-14;
//! unofficial and may change between versions.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// One parsed session file. Only the fields Shepherd uses.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CcSessionInfo {
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub name: Option<String>,
}

/// Parse every live session in `dir`. Non-json files, unparseable content,
/// and dead pids are skipped; a missing dir is an empty list.
pub fn scan_sessions(dir: &Path) -> Vec<CcSessionInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|text| serde_json::from_str::<CcSessionInfo>(&text).ok())
        .filter(|s| pid_alive(s.pid))
        .collect()
}

/// Signal-zero liveness probe. EPERM means the process exists but belongs to
/// another user, which still counts as alive.
pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 is the standard existence/permission check
    // and never delivers a signal.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// `~/.claude/projects` encodes a cwd by replacing "/" with "-".
pub fn encode_cwd(cwd: &str) -> String {
    cwd.replace('/', "-")
}

/// Transcript for a session: `<projects>/<encoded-cwd>/<session-id>.jsonl`.
pub fn transcript_path(projects_dir: &Path, cwd: &str, session_id: &str) -> PathBuf {
    projects_dir
        .join(encode_cwd(cwd))
        .join(format!("{session_id}.jsonl"))
}

/// Display project name: last path component of the cwd.
pub fn project_name(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn session_json(pid: u32, session_id: &str, cwd: &str) -> String {
        // shape copied from a real ~/.claude/sessions/21335.json
        format!(
            r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"{cwd}","startedAt":1789356924426,"procStart":"Mon Sep 14 03:35:23 2026","version":"2.1.270","kind":"interactive","entrypoint":"cli","name":"shepherd-7d","nameSource":"derived","status":"busy","updatedAt":1789370777307}}"#
        )
    }

    #[test]
    fn scans_live_sessions_and_skips_junk() {
        let dir = tempfile::tempdir().unwrap();
        let live = std::process::id();
        fs::write(
            dir.path().join(format!("{live}.json")),
            session_json(live, "s-one", "/Users/x/code/api"),
        )
        .unwrap();
        // dead pid must be filtered
        fs::write(
            dir.path().join("999999.json"),
            session_json(999_999, "s-dead", "/Users/x/code/api"),
        )
        .unwrap();
        // non-session files must not break the scan
        fs::write(dir.path().join("compaction-log.txt"), "junk").unwrap();
        fs::write(dir.path().join("21335.key"), "junk").unwrap();
        fs::write(dir.path().join("broken.json"), "{not json").unwrap();
        // missing fields -> unparseable -> skipped
        fs::write(dir.path().join("partial.json"), r#"{"pid":1}"#).unwrap();

        let found = scan_sessions(dir.path());
        assert_eq!(found.len(), 1, "only the live pid survives: {found:?}");
        let s = &found[0];
        assert_eq!(s.session_id, "s-one");
        assert_eq!(s.cwd, "/Users/x/code/api");
        assert_eq!(s.started_at, 1_789_356_924_426);
        assert_eq!(s.name.as_deref(), Some("shepherd-7d"));
    }

    #[test]
    fn missing_dir_is_empty() {
        assert!(scan_sessions(Path::new("/nonexistent-shepherd-test")).is_empty());
    }

    #[test]
    fn own_pid_is_alive_and_impossible_pid_is_not() {
        assert!(pid_alive(std::process::id()));
        // macOS pids cap below 100000, so 999999 reliably maps to ESRCH
        assert!(!pid_alive(999_999));
    }

    #[test]
    fn encodes_cwd_like_the_projects_dir() {
        // matches the real layout: -Users-yusinglam-Desktop-code-shepherd
        assert_eq!(
            encode_cwd("/Users/yusinglam/Desktop/code/shepherd"),
            "-Users-yusinglam-Desktop-code-shepherd"
        );
        assert_eq!(
            transcript_path(Path::new("/projects"), "/Users/x/code/api", "abc-123"),
            PathBuf::from("/projects/-Users-x-code-api/abc-123.jsonl")
        );
    }

    #[test]
    fn project_name_is_last_component() {
        assert_eq!(project_name("/Users/x/code/api"), "api");
        assert_eq!(project_name("/"), "/");
    }
}
