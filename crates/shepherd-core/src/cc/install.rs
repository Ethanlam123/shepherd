//! Installer for Shepherd's hooks in the user-scope ~/.claude/settings.json.
//! Merges (never clobbers): our entries are identified by a command
//! containing "shepherd-hook"; everything else is preserved verbatim,
//! including key order. The exact structure we install is documented in
//! docs/shepherd-hooks-settings.json.

use serde_json::{json, Map, Value};
use std::path::Path;

/// Marker identifying our hook entries among the user's own.
const MARKER: &str = "shepherd-hook";

/// The command line Claude Code runs: quoted absolute paths so spaces survive.
pub fn hook_command(hook_bin: &Path, sock: &Path) -> String {
    format!("\"{}\" --sock \"{}\"", hook_bin.display(), sock.display())
}

/// Whether any of our entries are present.
pub fn installed(settings_path: &Path) -> bool {
    read_settings(settings_path)
        .map(|v| count_ours(&v) > 0)
        .unwrap_or(false)
}

/// Add or refresh our PreToolUse + Notification entries. Idempotent.
pub fn install(settings_path: &Path, hook_bin: &Path, sock: &Path) -> Result<(), String> {
    install_with_timeout(settings_path, hook_bin, sock, 900)
}

pub fn install_with_timeout(
    settings_path: &Path,
    hook_bin: &Path,
    sock: &Path,
    timeout_secs: u64,
) -> Result<(), String> {
    let mut root = read_settings(settings_path)?;
    let command = hook_command(hook_bin, sock);
    let cmd = || {
        json!({
            "type": "command",
            "command": command,
            "timeout": timeout_secs,
        })
    };
    // PreToolUse: consequential tools only (decision 5)
    upsert_group(
        &mut root,
        "PreToolUse",
        json!({"matcher": "Bash|Edit|Write|WebFetch|WebSearch", "hooks": [cmd()]}),
    );
    // Notification: idle nudge only; permission_prompt is PreToolUse's job
    upsert_group(
        &mut root,
        "Notification",
        json!({"matcher": "idle_prompt", "hooks": [cmd()]}),
    );
    write_settings(settings_path, &root)
}

/// Remove exactly our entries. Returns whether the file changed.
pub fn uninstall(settings_path: &Path) -> Result<bool, String> {
    let mut root = read_settings(settings_path)?;
    let mut changed = false;
    for event in ["PreToolUse", "Notification"] {
        let Some(groups) = root
            .get_mut("hooks")
            .and_then(|h| h.get_mut(event))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        let before = groups.len();
        groups.retain(|g| !is_ours(g));
        changed |= groups.len() != before;
        if groups.is_empty() {
            // drop the empty event array
            if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
                hooks.remove(event);
                if hooks.is_empty() {
                    if let Some(obj) = root.as_object_mut() {
                        obj.remove("hooks");
                    }
                }
            }
        }
    }
    if changed {
        write_settings(settings_path, &root)?;
    }
    Ok(changed)
}

fn read_settings(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Value::Object(Map::new())),
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            format!(
                "{} is not valid JSON ({e}); refusing to touch it",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn write_settings(path: &Path, root: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(root).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.shepherd-tmp");
    std::fs::write(&tmp, text + "\n")
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

fn upsert_group(root: &mut Value, event: &str, our_group: Value) {
    if !root.get("hooks").is_some_and(|h| h.is_object()) {
        root.as_object_mut()
            .expect("root is an object")
            .insert("hooks".into(), json!({}));
    }
    let hooks = root
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .expect("hooks object just ensured");
    if !hooks.get(event).is_some_and(|v| v.is_array()) {
        hooks.insert(event.into(), json!([]));
    }
    let groups = hooks
        .get_mut(event)
        .and_then(Value::as_array_mut)
        .expect("array just ensured");
    groups.retain(|g| !is_ours(g));
    groups.push(our_group);
}

/// A group is ours when any of its hook commands mentions the marker binary.
fn is_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(MARKER))
            })
        })
}

fn count_ours(root: &Value) -> usize {
    ["PreToolUse", "Notification"]
        .iter()
        .filter_map(|event| {
            root.pointer(&format!("/hooks/{event}"))
                .and_then(Value::as_array)
        })
        .map(|groups| groups.iter().filter(|g| is_ours(g)).count())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        tmp.path().join("settings.json")
    }

    #[test]
    fn installs_into_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = settings(&tmp);
        install(
            &p,
            Path::new("/opt/shepherd-hook"),
            Path::new("/opt/s.sock"),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            v.pointer("/hooks/PreToolUse/0/matcher").unwrap(),
            "Bash|Edit|Write|WebFetch|WebSearch"
        );
        assert_eq!(
            v.pointer("/hooks/Notification/0/matcher").unwrap(),
            "idle_prompt"
        );
        let cmd = v
            .pointer("/hooks/PreToolUse/0/hooks/0/command")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(cmd, "\"/opt/shepherd-hook\" --sock \"/opt/s.sock\"");
        assert_eq!(
            v.pointer("/hooks/PreToolUse/0/hooks/0/timeout").unwrap(),
            900
        );
        assert!(installed(&p));
    }

    #[test]
    fn install_preserves_other_hooks_and_keys_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let p = settings(&tmp);
        std::fs::write(
            &p,
            r#"{
  "model": "opus",
  "permissions": {"allow": ["Bash(echo:*)"]},
  "hooks": {
    "PreToolUse": [
      {"matcher": "Bash", "hooks": [{"type": "command", "command": "/usr/local/bin/guard.sh"}]}
    ],
    "Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]
  }
}"#,
        )
        .unwrap();
        install(
            &p,
            Path::new("/opt/shepherd-hook"),
            Path::new("/opt/s.sock"),
        )
        .unwrap();

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["model", "permissions", "hooks"],
            "key order preserved"
        );
        let pre = v.pointer("/hooks/PreToolUse").unwrap().as_array().unwrap();
        assert_eq!(pre.len(), 2, "user's hook survives");
        assert_eq!(
            pre[0].pointer("/hooks/0/command").unwrap(),
            "/usr/local/bin/guard.sh"
        );
        assert!(pre[1]
            .pointer("/hooks/0/command")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("shepherd-hook"));
        assert_eq!(
            v.pointer("/hooks/Stop/0/hooks/0/command").unwrap(),
            "say done"
        );
        assert_eq!(v.pointer("/permissions/allow/0").unwrap(), "Bash(echo:*)");

        // reinstall is idempotent, not duplicated
        install(
            &p,
            Path::new("/opt/shepherd-hook"),
            Path::new("/opt/s.sock"),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            v.pointer("/hooks/PreToolUse")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn uninstall_removes_only_ours() {
        let tmp = tempfile::tempdir().unwrap();
        let p = settings(&tmp);
        std::fs::write(
            &p,
            r#"{
  "model": "opus",
  "hooks": {
    "PreToolUse": [
      {"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]},
      {"matcher": "Bash|Edit", "hooks": [{"type": "command", "command": "shepherd-hook --sock s"}]}
    ],
    "Notification": [
      {"matcher": "idle_prompt", "hooks": [{"type": "command", "command": "shepherd-hook --sock s"}]}
    ],
    "Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]
  }
}"#,
        )
        .unwrap();
        assert!(uninstall(&p).unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        // our two entries gone, user's PreToolUse + Stop kept, empty Notification dropped
        assert_eq!(
            v.pointer("/hooks/PreToolUse")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            v.pointer("/hooks/PreToolUse/0/hooks/0/command").unwrap(),
            "guard.sh"
        );
        assert!(v.pointer("/hooks/Notification").is_none());
        assert_eq!(
            v.pointer("/hooks/Stop/0/hooks/0/command").unwrap(),
            "say done"
        );
        assert_eq!(v.pointer("/model").unwrap(), "opus");
        assert!(!installed(&p));
        assert!(!uninstall(&p).unwrap(), "second uninstall is a no-op");
    }

    #[test]
    fn corrupted_settings_refuses_instead_of_clobbering() {
        let tmp = tempfile::tempdir().unwrap();
        let p = settings(&tmp);
        std::fs::write(&p, "{broken").unwrap();
        let err = install(&p, Path::new("/opt/h"), Path::new("/opt/s")).unwrap_err();
        assert!(err.contains("not valid JSON"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{broken");
    }
}
