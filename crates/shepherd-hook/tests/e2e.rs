//! End-to-end tests for the hook binary: real process, real unix socket,
//! isolated HOME so the machine's real settings never leak in.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

fn hook_bin() -> &'static str {
    env!("CARGO_BIN_EXE_shepherd-hook")
}

struct Env {
    _tmp: TempDir,
    home: std::path::PathBuf,
    sock: std::path::PathBuf,
}

// tempfile is only in core's dev-deps; keep these tests self-contained with
// a tiny tempdir helper instead of adding the dependency for one call.
impl Env {
    fn new() -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        // unix socket paths cap at SUN_LEN (~104): keep the dir short
        let dir = std::env::temp_dir().join(format!(
            "shook{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let home = dir.join("home");
        let sock = dir.join("shepherd.sock");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        Self {
            _tmp: TempDir(dir),
            home,
            sock,
        }
    }

    fn write_settings(&self, v: Value) {
        std::fs::write(self.home.join(".claude/settings.json"), v.to_string()).unwrap();
    }

    fn run(&self, payload: &Value) -> (String, Option<i32>) {
        let mut child = Command::new(hook_bin())
            .arg("--sock")
            .arg(&self.sock)
            .env("HOME", &self.home)
            .current_dir(&self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            out.status.code(),
        )
    }
}

struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One-shot socket server: reads the request line, lets the test inspect it,
/// replies with `reply`.
fn serve_once(sock: &Path, reply: Value) -> thread::JoinHandle<Value> {
    let listener = UnixListener::bind(sock).unwrap();
    thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = String::new();
        conn.read_to_string(&mut buf).unwrap();
        let request: Value = serde_json::from_str(buf.trim()).unwrap_or(Value::Null);
        let mut line = reply.to_string();
        line.push('\n');
        conn.write_all(line.as_bytes()).unwrap();
        request
    })
}

fn pretool(command: &str) -> Value {
    json!({
        "hook_event_name": "PreToolUse",
        "session_id": "11111111-2222-3333-4444-555555555555",
        "transcript_path": "/tmp/t.jsonl",
        "cwd": "/tmp",
        "permission_mode": "default",
        "tool_name": "Bash",
        "tool_input": {"command": command, "description": "run it"},
        "tool_use_id": "toolu_1",
    })
}

#[test]
fn approve_round_trip_through_real_socket() {
    let env = Env::new();
    env.write_settings(json!({}));
    let server = serve_once(&env.sock, json!({"decision": "approve"}));
    let (out, code) = env.run(&pretool("pnpm test auth/"));
    assert_eq!(code, Some(0));
    let v: Value = serde_json::from_str(&out).expect("hook printed decision JSON");
    assert_eq!(
        v.pointer("/hookSpecificOutput/permissionDecision").unwrap(),
        "allow"
    );
    let request = server.join().unwrap();
    assert_eq!(request["type"], "permission");
    assert_eq!(request["tool"], "Bash");
    assert_eq!(request["input"]["command"], "pnpm test auth/");
    assert_eq!(request["sessionId"], "11111111-2222-3333-4444-555555555555");
}

#[test]
fn edited_command_round_trip_as_updated_input() {
    let env = Env::new();
    env.write_settings(json!({}));
    let server = serve_once(
        &env.sock,
        json!({"decision": "approve_edited", "command": "pnpm test --filter unit"}),
    );
    let (out, _) = env.run(&pretool("pnpm test"));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v.pointer("/hookSpecificOutput/updatedInput/command")
            .unwrap(),
        "pnpm test --filter unit"
    );
    assert_eq!(
        v.pointer("/hookSpecificOutput/updatedInput/description")
            .unwrap(),
        "run it"
    );
    server.join().unwrap();
}

#[test]
fn deny_note_round_trip() {
    let env = Env::new();
    env.write_settings(json!({}));
    let server = serve_once(
        &env.sock,
        json!({"decision": "deny", "note": "use git clean"}),
    );
    let (out, _) = env.run(&pretool("rm -rf build"));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v.pointer("/hookSpecificOutput/permissionDecision").unwrap(),
        "deny"
    );
    assert_eq!(
        v.pointer("/hookSpecificOutput/permissionDecisionReason")
            .unwrap(),
        "use git clean"
    );
    server.join().unwrap();
}

/// A live deny-replying server sits on the socket, but the user's own allow
/// rule matches: the hook must not ask (an ask would have printed the deny
/// JSON) and must print nothing.
#[test]
fn user_allow_rule_passes_through_without_asking_shepherd() {
    let env = Env::new();
    env.write_settings(json!({"permissions": {"allow": ["Bash(echo safe:*)"]}}));
    let server = serve_once(
        &env.sock,
        json!({"decision": "deny", "note": "should not happen"}),
    );
    let (out, code) = env.run(&pretool("echo safe hi"));
    assert_eq!(code, Some(0));
    assert_eq!(
        out, "",
        "pass-through prints nothing; a socket ask would have returned the deny JSON"
    );
    // the hook never connected, so the server thread is still waiting: prove
    // accept() is unconsumed by claiming it ourselves with an empty request
    let mut probe = UnixStream::connect(&env.sock).unwrap();
    probe.shutdown(std::net::Shutdown::Write).unwrap();
    let mut buf = String::new();
    probe.read_to_string(&mut buf).unwrap();
    assert_eq!(
        buf.trim(),
        json!({"decision": "deny", "note": "should not happen"}).to_string()
    );
    drop(server.join()); // completes against the probe
}

#[test]
fn no_shepherd_fails_open_with_no_output() {
    let env = Env::new(); // no server, no settings
    let (out, code) = env.run(&pretool("pnpm test"));
    assert_eq!(code, Some(0));
    assert_eq!(out, "");
}

#[test]
fn notification_forwards_and_exits() {
    let env = Env::new();
    let server = serve_once(&env.sock, json!({"decision": "ignore"}));
    let payload = json!({
        "hook_event_name": "Notification",
        "session_id": "11111111-2222-3333-4444-555555555555",
        "message": "Claude is waiting for your input",
    });
    let (out, code) = env.run(&payload);
    assert_eq!(code, Some(0));
    assert_eq!(out, "", "notification hook never prints");
    let request = server.join().unwrap();
    assert_eq!(request["type"], "notification");
    assert_eq!(request["message"], "Claude is waiting for your input");
}
