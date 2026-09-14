//! Canonical session state. The registry is the single owner of UI-visible
//! state: adapters produce semantic events, the registry materializes them,
//! computes the badge, and pushes `UiEvent`s to the app layer. Persistence
//! (SQLite) and the event log land here in M2.

use crate::{now_ms, AgentEvent, Control, Envelope, LogLine, Pending, Run, Session, Status};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Fixed outcome text for runs stopped by the user (prototype copy).
pub const STOPPED_OUTCOME: &str =
    "Stopped manually before finishing. Progress up to the stop point is on disk; nothing was reverted.";

/// Activity ring buffer per session; matches the prototype cap.
const ACTIVITY_CAP: usize = 120;

/// Where the registry pushes UI-visible changes. The app layer maps these to
/// webview events and tray badge updates.
pub enum UiEvent {
    Session(UiSession),
    Activity { session_id: String, line: LogLine },
    Removed { session_id: String },
    Run(Run),
    Badge { count: usize },
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: UiEvent);
}

/// Session as the panel sees it: session fields plus the pending prompt (if
/// waiting) and the activity tail.
#[derive(Debug, Clone, Serialize)]
pub struct UiSession {
    #[serde(flatten)]
    pub session: Session,
    pub pending: Option<Pending>,
    pub activity: Vec<LogLine>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub sessions: Vec<UiSession>,
    pub runs: Vec<Run>,
    pub muted: bool,
}

struct SessionRec {
    session: Session,
    pending: Option<Pending>,
    activity: VecDeque<LogLine>,
}

struct Inner {
    sessions: HashMap<String, SessionRec>,
    runs: Vec<Run>,
    adapters: HashMap<String, mpsc::UnboundedSender<(String, Control)>>,
    muted: bool,
    last_badge: usize,
}

pub struct Registry {
    sink: Arc<dyn EventSink>,
    run_seq: AtomicU64,
    inner: Mutex<Inner>,
}

impl Registry {
    pub fn new(sink: Arc<dyn EventSink>) -> Self {
        Self {
            sink,
            run_seq: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                runs: Vec::new(),
                adapters: HashMap::new(),
                muted: false,
                last_badge: 0,
            }),
        }
    }

    /// Control channel back to the adapter that owns an agent's sessions.
    pub fn register_adapter(
        &self,
        agent: &str,
        controls: mpsc::UnboundedSender<(String, Control)>,
    ) {
        self.inner
            .lock()
            .unwrap()
            .adapters
            .insert(agent.to_string(), controls);
    }

    pub fn set_muted(&self, muted: bool) {
        self.inner.lock().unwrap().muted = muted;
    }

    /// Load persisted runs (newest first, as `Store::recent_runs` returns) at
    /// startup, before any adapter spawns so in-memory runs stay newest-first.
    pub fn seed_runs(&self, runs: Vec<Run>) {
        self.inner.lock().unwrap().runs.extend(runs);
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.inner.lock().unwrap();
        let mut sessions: Vec<UiSession> = inner
            .sessions
            .values()
            .map(|rec| UiSession {
                session: rec.session.clone(),
                pending: rec.pending.clone(),
                activity: rec.activity.iter().cloned().collect(),
            })
            .collect();
        sessions.sort_by_key(|s| s.session.started_at);
        Snapshot {
            sessions,
            runs: inner.runs.clone(),
            muted: inner.muted,
        }
    }

    pub fn waiting_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .values()
            .filter(|r| r.session.status == Status::Waiting)
            .count()
    }

    /// Apply one adapter event. Emits to the sink while holding the lock; the
    /// production sink (tauri emit) never re-enters the registry, so this is
    /// safe and keeps emissions ordered.
    pub fn handle_event(&self, env: Envelope) {
        let mut inner = self.inner.lock().unwrap();
        let sink = self.sink.as_ref();
        match env.event {
            AgentEvent::Started { session } => {
                let id = session.id.clone();
                inner.sessions.insert(
                    id.clone(),
                    SessionRec {
                        session,
                        pending: None,
                        activity: VecDeque::new(),
                    },
                );
                emit_session(&inner, &id, sink);
                badge_check(&mut inner, sink);
            }
            AgentEvent::Activity { kind, line } => {
                if let Some(rec) = inner.sessions.get_mut(&env.session_id) {
                    let entry = LogLine {
                        ts: now_ms(),
                        kind,
                        text: line,
                    };
                    rec.activity.push_back(entry.clone());
                    if rec.activity.len() > ACTIVITY_CAP {
                        rec.activity.pop_front();
                    }
                    sink.emit(UiEvent::Activity {
                        session_id: env.session_id,
                        line: entry,
                    });
                }
            }
            AgentEvent::PermissionRequested { pending } => {
                if let Some(rec) = inner.sessions.get_mut(&env.session_id) {
                    rec.session.status = Status::Waiting;
                    rec.pending = Some(Pending::Permission(pending));
                    emit_session(&inner, &env.session_id, sink);
                    badge_check(&mut inner, sink);
                }
            }
            AgentEvent::InputRequested { pending } => {
                if let Some(rec) = inner.sessions.get_mut(&env.session_id) {
                    rec.session.status = Status::Waiting;
                    rec.pending = Some(Pending::Input(pending));
                    emit_session(&inner, &env.session_id, sink);
                    badge_check(&mut inner, sink);
                }
            }
            AgentEvent::Progress { elapsed_ms, tokens } => {
                if let Some(rec) = inner.sessions.get_mut(&env.session_id) {
                    rec.session.elapsed_ms = elapsed_ms;
                    rec.session.tokens = tokens;
                    emit_session(&inner, &env.session_id, sink);
                }
            }
            AgentEvent::Finished {
                outcome,
                files,
                stopped,
            } => {
                let run_id = self.next_run_id();
                if let Some(run) =
                    finish(&mut inner, &env.session_id, run_id, outcome, files, stopped)
                {
                    sink.emit(UiEvent::Run(run));
                }
                sink.emit(UiEvent::Removed {
                    session_id: env.session_id,
                });
                badge_check(&mut inner, sink);
            }
            AgentEvent::Failed { message } => {
                let run_id = self.next_run_id();
                if let Some(run) = finish(
                    &mut inner,
                    &env.session_id,
                    run_id,
                    Some(format!("Failed: {message}")),
                    Vec::new(),
                    false,
                ) {
                    sink.emit(UiEvent::Run(run));
                }
                sink.emit(UiEvent::Removed {
                    session_id: env.session_id,
                });
                badge_check(&mut inner, sink);
            }
        }
    }

    /// Apply a user decision: update local state, then forward the control to
    /// the owning adapter. Returns false for unknown sessions.
    pub fn handle_control(&self, session_id: &str, control: Control) -> bool {
        let agent = {
            let mut inner = self.inner.lock().unwrap();
            let sink = self.sink.as_ref();
            let Some(rec) = inner.sessions.get_mut(session_id) else {
                return false;
            };
            let agent = rec.session.agent.clone();
            match &control {
                Control::Approve { .. }
                | Control::ApproveEdited { .. }
                | Control::Deny { .. }
                | Control::Answer { .. } => {
                    // pending_id is validated adapter-side; one prompt per session.
                    rec.pending = None;
                    rec.session.status = Status::Running;
                }
                Control::ApproveAlways { tool, .. } => {
                    rec.pending = None;
                    rec.session.status = Status::Running;
                    if !rec.session.allowed_tools.iter().any(|t| t == tool) {
                        rec.session.allowed_tools.push(tool.clone());
                    }
                }
                Control::Pause => {
                    if rec.session.status == Status::Running {
                        rec.session.status = Status::Paused;
                    }
                }
                Control::Resume => {
                    if rec.session.status != Status::Running {
                        // A still-pending prompt keeps the session waiting.
                        rec.session.status = if rec.pending.is_some() {
                            Status::Waiting
                        } else {
                            Status::Running
                        };
                    }
                }
                Control::Stop => {} // adapter emits Finished{stopped}
            }
            emit_session(&inner, session_id, sink);
            badge_check(&mut inner, sink);
            agent
        };
        // Forward outside the registry lock so adapters can safely re-enter.
        let inner = self.inner.lock().unwrap();
        if let Some(tx) = inner.adapters.get(&agent) {
            let _ = tx.send((session_id.to_string(), control));
        }
        true
    }

    fn next_run_id(&self) -> String {
        format!("r{}", self.run_seq.fetch_add(1, Ordering::Relaxed))
    }
}

fn emit_session(inner: &Inner, session_id: &str, sink: &dyn EventSink) {
    if let Some(rec) = inner.sessions.get(session_id) {
        sink.emit(UiEvent::Session(UiSession {
            session: rec.session.clone(),
            pending: rec.pending.clone(),
            activity: rec.activity.iter().cloned().collect(),
        }));
    }
}

/// Emit a Badge event only when the waiting count actually changed.
fn badge_check(inner: &mut Inner, sink: &dyn EventSink) {
    let count = inner
        .sessions
        .values()
        .filter(|r| r.session.status == Status::Waiting)
        .count();
    if count != inner.last_badge {
        inner.last_badge = count;
        sink.emit(UiEvent::Badge { count });
    }
}

/// Remove the session and record a run. Returns None if the session is gone.
fn finish(
    inner: &mut Inner,
    session_id: &str,
    run_id: String,
    outcome: Option<String>,
    files: Vec<String>,
    stopped: bool,
) -> Option<crate::Run> {
    let rec = inner.sessions.remove(session_id)?;
    let run = crate::Run {
        id: run_id,
        agent: rec.session.agent,
        title: rec.session.title,
        project: rec.session.project,
        ended_at: now_ms(),
        duration_ms: rec.session.elapsed_ms,
        tokens: rec.session.tokens,
        stopped,
        outcome: if stopped {
            STOPPED_OUTCOME.to_string()
        } else {
            outcome.unwrap_or_default()
        },
        files: if stopped { Vec::new() } else { files },
    };
    inner.runs.insert(0, run.clone());
    Some(run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PendingPermission;

    /// Records sink events as short strings for assertion.
    struct Sink(Mutex<Vec<String>>);
    impl EventSink for Sink {
        fn emit(&self, event: UiEvent) {
            let label = match event {
                UiEvent::Session(s) => format!("session:{}:{:?}", s.session.id, s.session.status),
                UiEvent::Activity { session_id, .. } => format!("activity:{session_id}"),
                UiEvent::Removed { session_id } => format!("removed:{session_id}"),
                UiEvent::Run(r) => format!("run:{}:{}", r.id, r.stopped),
                UiEvent::Badge { count } => format!("badge:{count}"),
            };
            self.0.lock().unwrap().push(label);
        }
    }

    fn seen(sink: &Sink, pat: &str) -> bool {
        sink.0.lock().unwrap().iter().any(|x| x.contains(pat))
    }

    fn registry() -> (Arc<Registry>, Arc<Sink>) {
        let sink = Arc::new(Sink(Mutex::new(Vec::new())));
        (
            Arc::new(Registry::new(sink.clone() as Arc<dyn EventSink>)),
            sink,
        )
    }

    fn session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            agent: "cc".to_string(),
            title: "test session".to_string(),
            project: "api-server".to_string(),
            cwd: "~/code/api-server".to_string(),
            status: Status::Running,
            started_at: now_ms(),
            elapsed_ms: 0,
            tokens: 1000,
            allowed_tools: Vec::new(),
        }
    }

    fn started(s: Session) -> Envelope {
        Envelope {
            agent: "cc",
            session_id: s.id.clone(),
            event: AgentEvent::Started { session: s },
        }
    }

    fn permission(sid: &str, pid: &str) -> Envelope {
        Envelope {
            agent: "cc",
            session_id: sid.to_string(),
            event: AgentEvent::PermissionRequested {
                pending: PendingPermission {
                    id: pid.to_string(),
                    tool: "Bash".to_string(),
                    command: "pnpm test".to_string(),
                    reason: "run tests".to_string(),
                },
            },
        }
    }

    #[test]
    fn permission_waits_then_approve_resumes_and_badge_tracks() {
        let (reg, sink) = registry();
        reg.handle_event(started(session("cc-1")));
        reg.handle_event(permission("cc-1", "p1"));

        assert_eq!(reg.waiting_count(), 1);
        assert!(seen(&sink, "badge:1"));

        let snap = reg.snapshot();
        assert_eq!(snap.sessions[0].session.status, Status::Waiting);
        assert!(snap.sessions[0].pending.is_some());

        // register a capture adapter so we can assert forwarding
        let (tx, mut rx) = mpsc::unbounded_channel();
        reg.register_adapter("cc", tx);
        assert!(reg.handle_control(
            "cc-1",
            Control::Approve {
                pending_id: "p1".to_string()
            }
        ));

        assert_eq!(reg.waiting_count(), 0);
        assert!(seen(&sink, "badge:0"));
        assert!(matches!(rx.try_recv(), Ok((sid, Control::Approve { .. })) if sid == "cc-1"));
        let snap = reg.snapshot();
        assert_eq!(snap.sessions[0].session.status, Status::Running);
        assert!(snap.sessions[0].pending.is_none());
    }

    #[test]
    fn approve_always_adds_tool_once() {
        let (reg, _sink) = registry();
        let (tx, _rx) = mpsc::unbounded_channel();
        reg.register_adapter("cc", tx);
        reg.handle_event(started(session("cc-1")));
        reg.handle_event(permission("cc-1", "p1"));
        reg.handle_control(
            "cc-1",
            Control::ApproveAlways {
                pending_id: "p1".to_string(),
                tool: "Bash".to_string(),
            },
        );
        reg.handle_event(permission("cc-1", "p2"));
        reg.handle_control(
            "cc-1",
            Control::ApproveAlways {
                pending_id: "p2".to_string(),
                tool: "Bash".to_string(),
            },
        );
        assert_eq!(
            reg.snapshot().sessions[0].session.allowed_tools,
            vec!["Bash"]
        );
    }

    #[test]
    fn finish_records_run_and_stopped_outcome() {
        let (reg, sink) = registry();
        let mut s = session("cc-1");
        s.elapsed_ms = 65_000;
        s.tokens = 5_000;
        reg.handle_event(started(s));
        reg.handle_event(Envelope {
            agent: "cc",
            session_id: "cc-1".to_string(),
            event: AgentEvent::Finished {
                outcome: Some("did the thing".to_string()),
                files: vec!["a.ts".to_string()],
                stopped: false,
            },
        });
        let snap = reg.snapshot();
        assert!(snap.sessions.is_empty());
        assert_eq!(snap.runs.len(), 1);
        assert_eq!(snap.runs[0].outcome, "did the thing");
        assert_eq!(snap.runs[0].files, vec!["a.ts"]);
        assert_eq!(snap.runs[0].duration_ms, 65_000);
        assert!(seen(&sink, "run:r0:false"));
        assert!(seen(&sink, "removed:cc-1"));

        reg.handle_event(started(session("cc-2")));
        reg.handle_event(Envelope {
            agent: "cc",
            session_id: "cc-2".to_string(),
            event: AgentEvent::Finished {
                outcome: None,
                files: vec![],
                stopped: true,
            },
        });
        let snap = reg.snapshot();
        assert_eq!(snap.runs[0].outcome, STOPPED_OUTCOME);
        assert!(snap.runs[0].stopped);
        assert!(snap.runs[0].files.is_empty());
    }

    #[test]
    fn activity_is_capped() {
        let (reg, _sink) = registry();
        reg.handle_event(started(session("cc-1")));
        for i in 0..200 {
            reg.handle_event(Envelope {
                agent: "cc",
                session_id: "cc-1".to_string(),
                event: AgentEvent::Activity {
                    kind: "tool".to_string(),
                    line: format!("line {i}"),
                },
            });
        }
        assert_eq!(reg.snapshot().sessions[0].activity.len(), ACTIVITY_CAP);
    }

    #[test]
    fn unknown_session_control_returns_false() {
        let (reg, _sink) = registry();
        assert!(!reg.handle_control("nope", Control::Pause));
    }

    #[test]
    fn pause_and_resume_keep_pending_wait() {
        let (reg, _sink) = registry();
        let (tx, _rx) = mpsc::unbounded_channel();
        reg.register_adapter("cc", tx);
        reg.handle_event(started(session("cc-1")));
        reg.handle_control("cc-1", Control::Pause);
        assert_eq!(reg.snapshot().sessions[0].session.status, Status::Paused);
        reg.handle_event(permission("cc-1", "p1"));
        // resume while a prompt is pending stays waiting
        reg.handle_control("cc-1", Control::Resume);
        assert_eq!(reg.snapshot().sessions[0].session.status, Status::Waiting);
    }

    #[test]
    fn progress_updates_elapsed_and_tokens() {
        let (reg, sink) = registry();
        reg.handle_event(started(session("cc-1")));
        reg.handle_event(Envelope {
            agent: "cc",
            session_id: "cc-1".to_string(),
            event: AgentEvent::Progress {
                elapsed_ms: 12_000,
                tokens: 55_000,
            },
        });
        let snap = reg.snapshot();
        assert_eq!(snap.sessions[0].session.elapsed_ms, 12_000);
        assert_eq!(snap.sessions[0].session.tokens, 55_000);
        // progress emits a session event so the panel can correct its timer
        assert!(seen(&sink, "session:cc-1"));
    }

    #[test]
    fn failed_event_records_failed_run() {
        let (reg, _sink) = registry();
        reg.handle_event(started(session("cc-1")));
        reg.handle_event(Envelope {
            agent: "cc",
            session_id: "cc-1".to_string(),
            event: AgentEvent::Failed {
                message: "hook died".to_string(),
            },
        });
        let snap = reg.snapshot();
        assert!(snap.sessions.is_empty());
        assert_eq!(snap.runs.len(), 1);
        assert_eq!(snap.runs[0].outcome, "Failed: hook died");
        assert!(!snap.runs[0].stopped);
    }

    /// Unknown agents must never crash the registry (spec: unknown agents
    /// never crash the panel).
    #[test]
    fn events_for_unknown_session_are_ignored() {
        let (reg, sink) = registry();
        reg.handle_event(Envelope {
            agent: "cc",
            session_id: "ghost".to_string(),
            event: AgentEvent::Activity {
                kind: "tool".to_string(),
                line: "x".to_string(),
            },
        });
        reg.handle_event(permission("ghost", "p1"));
        reg.handle_event(Envelope {
            agent: "cc",
            session_id: "ghost".to_string(),
            event: AgentEvent::Finished {
                outcome: None,
                files: vec![],
                stopped: false,
            },
        });
        let snap = reg.snapshot();
        assert!(snap.sessions.is_empty());
        assert!(snap.runs.is_empty());
        assert!(!seen(&sink, "badge:1"));
    }

    #[test]
    fn badge_emitted_only_when_count_changes() {
        let (reg, sink) = registry();
        reg.handle_event(started(session("cc-1")));
        reg.handle_event(started(session("cc-2"))); // ids differ, agent same
        reg.handle_event(permission("cc-1", "p1"));
        reg.handle_event(permission("cc-2", "p1"));
        let badges: Vec<String> = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|x| x.starts_with("badge:"))
            .cloned()
            .collect();
        // 0 -> 1 -> 2, exactly one emit per change, no duplicates
        assert_eq!(badges, vec!["badge:1".to_string(), "badge:2".to_string()]);
    }

    #[test]
    fn controls_route_to_owning_adapter() {
        let (reg, _sink) = registry();
        let (cc_tx, mut cc_rx) = mpsc::unbounded_channel();
        let (oc_tx, mut oc_rx) = mpsc::unbounded_channel();
        reg.register_adapter("cc", cc_tx);
        reg.register_adapter("oc", oc_tx);

        let mut s = session("oc-1");
        s.agent = "oc".to_string();
        reg.handle_event(started(s));
        assert!(reg.handle_control("oc-1", Control::Pause));

        assert!(matches!(
            cc_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(oc_rx.try_recv(), Ok((sid, Control::Pause)) if sid == "oc-1"));
    }

    #[test]
    fn snapshot_orders_sessions_by_started_at() {
        let (reg, _sink) = registry();
        let mut older = session("cc-2");
        older.started_at = 1_000;
        let mut newer = session("cc-3");
        newer.started_at = 2_000;
        reg.handle_event(started(older)); // inserted first, older timestamp
        reg.handle_event(started(newer));
        let snap = reg.snapshot();
        assert_eq!(snap.sessions[0].session.id, "cc-2");
        assert_eq!(snap.sessions[1].session.id, "cc-3");
    }

    /// State applies even if the owning adapter is gone (send fails
    /// silently); the user still gets a consistent panel.
    #[test]
    fn control_applies_state_without_registered_adapter() {
        let (reg, _sink) = registry();
        reg.handle_event(started(session("cc-1")));
        reg.handle_event(permission("cc-1", "p1"));
        assert!(reg.handle_control(
            "cc-1",
            Control::Approve {
                pending_id: "p1".to_string()
            }
        ));
        let snap = reg.snapshot();
        assert_eq!(snap.sessions[0].session.status, Status::Running);
        assert!(snap.sessions[0].pending.is_none());
    }

    #[test]
    fn deny_and_answer_clear_waiting() {
        let (reg, _sink) = registry();
        let (tx, _rx) = mpsc::unbounded_channel();
        reg.register_adapter("cc", tx);
        reg.handle_event(started(session("cc-1")));
        reg.handle_event(permission("cc-1", "p1"));
        assert!(reg.handle_control(
            "cc-1",
            Control::Deny {
                pending_id: "p1".to_string(),
                note: Some("no".to_string())
            }
        ));
        assert_eq!(reg.snapshot().sessions[0].session.status, Status::Running);

        reg.handle_event(permission("cc-1", "p2"));
        assert!(reg.handle_control(
            "cc-1",
            Control::Answer {
                pending_id: "p2".to_string(),
                text: "yes".to_string()
            }
        ));
        assert_eq!(reg.snapshot().sessions[0].session.status, Status::Running);
        assert_eq!(reg.waiting_count(), 0);
    }

    /// Snapshot sessions carry session fields at the top level (serde
    /// flatten) with the pending prompt and activity tail beside them.
    #[test]
    fn ui_session_flattens_in_snapshot() {
        let (reg, _sink) = registry();
        reg.handle_event(started(session("cc-1")));
        reg.handle_event(permission("cc-1", "p1"));
        reg.handle_event(Envelope {
            agent: "cc",
            session_id: "cc-1".to_string(),
            event: AgentEvent::Activity {
                kind: "tool".to_string(),
                line: "> Read x".to_string(),
            },
        });
        let snap = reg.snapshot();
        let v = serde_json::to_value(&snap).unwrap();
        let s = &v["sessions"][0];
        assert_eq!(s["id"], "cc-1");
        assert_eq!(s["status"], "waiting");
        assert_eq!(s["pending"]["kind"], "permission");
        assert_eq!(s["pending"]["tool"], "Bash");
        assert_eq!(s["activity"][0]["text"], "> Read x");
    }
}
