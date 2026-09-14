//! shepherd-core: adapter host, session registry, and shared types.
//!
//! Adapters are plugins behind `AgentAdapter`. They emit `AgentEvent`s over a
//! channel and receive `Control`s back; the registry is the only owner of
//! UI-visible state. Adapters never talk to the UI directly.

pub mod config;
pub mod mock;
pub mod registry;

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

/// Everything an adapter needs to run: where to send events, where controls
/// arrive. Adapters are spawned once and run until the app exits.
pub struct AdapterContext {
    pub events: mpsc::UnboundedSender<Envelope>,
    pub controls: mpsc::UnboundedReceiver<(String, Control)>,
}

pub trait AgentAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn spawn(self: Box<Self>, ctx: AdapterContext);
}
