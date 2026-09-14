//! shepherd-core: adapter host, session registry, and shared types.
//!
//! Adapters are plugins behind `AgentAdapter`. They emit `AgentEvent`s over a
//! channel and receive `Control`s back; the registry is the only owner of
//! UI-visible state. Adapters never talk to the UI directly.

pub mod cc;
pub mod config;
pub mod mock;
pub mod registry;
pub mod store;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Current time in epoch ms.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A finished run, as stored and shown on the Runs tab.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    pub agent: String,
    pub title: String,
    pub project: String,
    /// Epoch ms.
    pub ended_at: i64,
    pub duration_ms: u64,
    pub tokens: u64,
    pub stopped: bool,
    pub outcome: String,
    pub files: Vec<String>,
}

/// Live status of a session. Waiting always pairs with a `Pending` in the
/// registry; the tray badge counts waiting sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Waiting,
    Paused,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    /// Adapter id: "cc" (Claude Code), "oc", "pi", "hm".
    pub agent: String,
    pub title: String,
    pub project: String,
    pub cwd: String,
    pub status: Status,
    /// Epoch ms.
    pub started_at: i64,
    pub elapsed_ms: u64,
    pub tokens: u64,
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPermission {
    pub id: String,
    pub tool: String,
    pub command: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInput {
    pub id: String,
    pub question: String,
    pub suggestions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pending {
    #[serde(rename_all = "camelCase")]
    Permission(PendingPermission),
    #[serde(rename_all = "camelCase")]
    Input(PendingInput),
}

impl Pending {
    pub fn id(&self) -> &str {
        match self {
            Pending::Permission(p) => &p.id,
            Pending::Input(p) => &p.id,
        }
    }
}

/// One activity-log line. `kind` is a display hint: tool|info|ok|warn|user|sys.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    /// Epoch ms.
    pub ts: i64,
    pub kind: String,
    pub text: String,
}

/// Events an adapter can emit. Mirrors the spec contract plus `Progress`,
/// which carries elapsed/token updates that hooks don't provide per event.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Started {
        session: Session,
    },
    Activity {
        kind: String,
        line: String,
    },
    PermissionRequested {
        pending: PendingPermission,
    },
    InputRequested {
        pending: PendingInput,
    },
    /// A pending prompt resolved without a user decision (hook timeout, the
    /// hook process was killed, or a nudge answered in the terminal).
    /// Clears the card and resumes Running.
    PendingCleared {
        pending_id: String,
    },
    Progress {
        elapsed_ms: u64,
        tokens: u64,
    },
    Finished {
        outcome: Option<String>,
        files: Vec<String>,
        stopped: bool,
    },
    Failed {
        message: String,
    },
}

/// Adapter event tagged with its origin session.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub agent: &'static str,
    pub session_id: String,
    pub event: AgentEvent,
}

/// User decisions routed back to the adapter that owns the session.
/// Wire format: {"type":"approve", "pendingId":"..."} etc.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    #[serde(rename_all = "camelCase")]
    Approve {
        pending_id: String,
    },
    #[serde(rename_all = "camelCase")]
    ApproveAlways {
        pending_id: String,
        tool: String,
    },
    #[serde(rename_all = "camelCase")]
    ApproveEdited {
        pending_id: String,
        command: String,
    },
    #[serde(rename_all = "camelCase")]
    Deny {
        pending_id: String,
        note: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Answer {
        pending_id: String,
        text: String,
    },
    Pause,
    Resume,
    Stop,
}

/// A boxed, sendable future the app's spawner can run.
pub type BoxFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
/// Spawns a future onto the app's async runtime. The app injects this so the
/// core never depends on the host runtime (and adapters never spawn onto a
/// nonexistent reactor).
pub type Spawner = std::sync::Arc<dyn Fn(BoxFuture) + Send + Sync>;

/// Everything an adapter needs to run: where to send events, where controls
/// arrive, and how to spawn its loop.
pub struct AdapterContext {
    pub events: mpsc::UnboundedSender<Envelope>,
    pub controls: mpsc::UnboundedReceiver<(String, Control)>,
    pub spawn: Spawner,
}

pub trait AgentAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn spawn(self: Box<Self>, ctx: AdapterContext);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The panel sends controls as {"type":"...", ...} with camelCase fields.
    /// This is the wire contract with ui/panel.ts: if either side drifts,
    /// this breaks first.
    #[test]
    fn control_wire_format_matches_panel() {
        type Check = Box<dyn Fn(&Control) -> bool>;
        let cases: Vec<(&str, serde_json::Value, Check)> = vec![
            (
                "approve",
                json!({"type":"approve","pendingId":"p1"}),
                Box::new(
                    |c| matches!(c, Control::Approve { pending_id } if pending_id.as_str() == "p1"),
                ),
            ),
            (
                "approve_always",
                json!({"type":"approve_always","pendingId":"p1","tool":"Bash"}),
                Box::new(
                    |c| matches!(c, Control::ApproveAlways { pending_id, tool } if pending_id.as_str() == "p1" && tool.as_str() == "Bash"),
                ),
            ),
            (
                "approve_edited",
                json!({"type":"approve_edited","pendingId":"p1","command":"echo hi"}),
                Box::new(
                    |c| matches!(c, Control::ApproveEdited { pending_id, command } if pending_id.as_str() == "p1" && command.as_str() == "echo hi"),
                ),
            ),
            (
                "deny with note",
                json!({"type":"deny","pendingId":"p1","note":"nope"}),
                Box::new(
                    |c| matches!(c, Control::Deny { pending_id, note } if pending_id.as_str() == "p1" && note.as_deref() == Some("nope")),
                ),
            ),
            (
                "deny plain",
                json!({"type":"deny","pendingId":"p1","note":null}),
                Box::new(|c| matches!(c, Control::Deny { note: None, .. })),
            ),
            (
                "deny missing note",
                json!({"type":"deny","pendingId":"p1"}),
                Box::new(|c| matches!(c, Control::Deny { note: None, .. })),
            ),
            (
                "answer",
                json!({"type":"answer","pendingId":"p1","text":"yes"}),
                Box::new(
                    |c| matches!(c, Control::Answer { pending_id, text } if pending_id.as_str() == "p1" && text.as_str() == "yes"),
                ),
            ),
            (
                "pause",
                json!({"type":"pause"}),
                Box::new(|c| matches!(c, Control::Pause)),
            ),
            (
                "resume",
                json!({"type":"resume"}),
                Box::new(|c| matches!(c, Control::Resume)),
            ),
            (
                "stop",
                json!({"type":"stop"}),
                Box::new(|c| matches!(c, Control::Stop)),
            ),
        ];
        for (name, raw, check) in cases {
            let c: Control = serde_json::from_value(raw)
                .unwrap_or_else(|e| panic!("{name}: should deserialize: {e}"));
            assert!(check(&c), "{name}: wrong variant");
        }
    }

    #[test]
    fn unknown_control_type_is_rejected() {
        assert!(serde_json::from_value::<Control>(json!({"type":"explode"})).is_err());
    }

    #[test]
    fn pending_wire_format_is_internally_tagged() {
        let perm = Pending::Permission(PendingPermission {
            id: "p1".into(),
            tool: "Bash".into(),
            command: "pnpm test".into(),
            reason: "run tests".into(),
        });
        let v = serde_json::to_value(&perm).unwrap();
        assert_eq!(v["kind"], "permission");
        assert_eq!(v["id"], "p1");
        assert_eq!(v["tool"], "Bash");
        assert_eq!(v["command"], "pnpm test");
        assert_eq!(v["reason"], "run tests");

        let input = Pending::Input(PendingInput {
            id: "p2".into(),
            question: "Which way?".into(),
            suggestions: vec!["A".into(), "B".into()],
        });
        let v = serde_json::to_value(&input).unwrap();
        assert_eq!(v["kind"], "input");
        assert_eq!(v["question"], "Which way?");
        assert_eq!(v["suggestions"], json!(["A", "B"]));
    }

    #[test]
    fn session_wire_format_is_camel_case() {
        let s = Session {
            id: "cc-1".into(),
            agent: "cc".into(),
            title: "t".into(),
            project: "p".into(),
            cwd: "~/code/p".into(),
            status: Status::Waiting,
            started_at: 123,
            elapsed_ms: 45,
            tokens: 678,
            allowed_tools: vec!["Bash".into()],
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["id"], "cc-1");
        assert_eq!(v["status"], "waiting"); // snake_case enum only
        assert_eq!(v["startedAt"], 123);
        assert_eq!(v["elapsedMs"], 45);
        assert_eq!(v["allowedTools"], json!(["Bash"]));
    }

    #[test]
    fn run_wire_format_is_camel_case() {
        let r = Run {
            id: "r0".into(),
            agent: "cc".into(),
            title: "t".into(),
            project: "p".into(),
            ended_at: 1000,
            duration_ms: 2000,
            tokens: 3000,
            stopped: true,
            outcome: "o".into(),
            files: vec!["a.ts".into()],
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["endedAt"], 1000);
        assert_eq!(v["durationMs"], 2000);
        assert_eq!(v["stopped"], true);
    }

    #[test]
    fn log_line_wire_format() {
        let v = serde_json::to_value(&LogLine {
            ts: 5,
            kind: "tool".into(),
            text: "x".into(),
        })
        .unwrap();
        assert_eq!(v["ts"], 5);
        assert_eq!(v["kind"], "tool");
        assert_eq!(v["text"], "x");
    }
}
