//! Claude Code adapter: discovers live sessions from `~/.claude/sessions`,
//! streams their transcripts as agent events, and serves the hook socket
//! (M3): PreToolUse permission cards round-trip to shepherd-hook, idle
//! notifications become nudge cards (no answer injection, decision 2), and
//! pause/stop stay unsupported (decision 1). The Stop hook (M4) records a
//! run per completed turn from the hook's `last_assistant_message` while
//! the session stays open for follow-up turns (decision 9).

pub mod discover;
pub mod install;
pub mod server;
pub mod transcript;

pub use discover::CcSessionInfo;

use crate::config::ShepherdConfig;
use crate::{
    now_ms, AdapterContext, AgentAdapter, AgentEvent, Control, Envelope, PendingInput,
    PendingPermission, Session, Status,
};
use serde_json::Value;
use server::{decision_value, HookIn};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio::time::{interval, Duration};
use transcript::{parse_line, Line};

/// Rescan the sessions dir every N tail ticks (tick = 1s).
const RESCAN_TICKS: u64 = 2;
/// Progress correction cadence in seconds.
const PROGRESS_SECS: u64 = 5;
/// On discovery, replay the last N activity lines so the panel has context.
const REPLAY_LINES: usize = 20;
/// Title fallback: first user prompt, truncated.
const TITLE_MAX: usize = 70;
/// Run outcome cap; final assistant messages can run long.
const OUTCOME_MAX: usize = 300;

pub struct CcAdapter {
    config: ShepherdConfig,
}

impl CcAdapter {
    pub fn new(config: ShepherdConfig) -> Self {
        Self { config }
    }
}

/// One live Claude Code session plus everything derived from its transcript.
#[derive(Clone)]
struct Tracked {
    info: CcSessionInfo,
    session_file: PathBuf,
    transcript: PathBuf,
    /// Bytes of the transcript already consumed.
    offset: u64,
    title: Option<String>,
    user_prompt: Option<String>,
    last_text: Option<String>,
    context_tokens: u64,
    output_total: u64,
    files: Vec<String>,
    /// Files touched since the last Stop hook: per-turn scope for runs.
    /// ponytail: the first run after discovery absorbs all replayed history.
    turn_files: Vec<String>,
    /// Whether the last completed turn already recorded its run (Stop hook).
    /// Cleared when the transcript moves (a new turn started).
    turn_recorded: bool,
    /// Tools approved with always-allow for this session (hook-side).
    allowed: HashSet<String>,
    /// Live nudge card id, cleared when transcript activity resumes.
    nudge: Option<String>,
    /// Last title pushed to the registry; a change re-emits the session.
    pushed_title: String,
}

impl Tracked {
    fn new(config: &ShepherdConfig, info: CcSessionInfo) -> Self {
        let session_file = config
            .claude_sessions_dir
            .join(format!("{}.json", info.pid));
        let transcript =
            discover::transcript_path(&config.claude_projects_dir, &info.cwd, &info.session_id);
        Self {
            session_file,
            transcript,
            offset: 0,
            title: None,
            user_prompt: None,
            last_text: None,
            context_tokens: 0,
            output_total: 0,
            files: Vec::new(),
            turn_files: Vec::new(),
            turn_recorded: false,
            allowed: HashSet::new(),
            nudge: None,
            pushed_title: String::new(),
            info,
        }
    }

    /// Display title: AI title > first user prompt > session name > id.
    fn title(&self) -> String {
        self.title
            .clone()
            .or_else(|| {
                self.user_prompt
                    .as_ref()
                    .map(|p| truncate_chars(p, TITLE_MAX))
            })
            .or_else(|| self.info.name.clone())
            .unwrap_or_else(|| format!("Session {}", self.info.session_id))
    }

    fn tokens(&self) -> u64 {
        self.context_tokens + self.output_total
    }

    fn elapsed_ms(&self) -> u64 {
        (now_ms() - self.info.started_at).max(0) as u64
    }

    fn to_session(&self) -> Session {
        Session {
            id: self.info.session_id.clone(),
            agent: "cc".to_string(),
            title: self.pushed_title.clone(),
            project: discover::project_name(&self.info.cwd),
            cwd: self.info.cwd.clone(),
            status: Status::Running,
            started_at: self.info.started_at,
            elapsed_ms: self.elapsed_ms(),
            tokens: self.tokens(),
            allowed_tools: {
                let mut v: Vec<String> = self.allowed.iter().cloned().collect();
                v.sort();
                v
            },
        }
    }

    /// Apply one transcript line, collecting activity labels.
    fn apply(&mut self, line: &str, activity: &mut Vec<String>) {
        match parse_line(line) {
            Line::Assistant {
                tools,
                text,
                context_tokens,
                output_tokens,
            } => {
                for tool in tools {
                    activity.push(format!("> {}", tool.label));
                    if let Some(file) = tool.file {
                        if !self.files.contains(&file) {
                            self.files.push(file.clone());
                        }
                        if !self.turn_files.contains(&file) {
                            self.turn_files.push(file);
                        }
                    }
                }
                if let Some(text) = text {
                    self.last_text = Some(text);
                }
                self.context_tokens = self.context_tokens.max(context_tokens);
                self.output_total += output_tokens;
            }
            Line::UserPrompt(prompt) => {
                if self.user_prompt.is_none() {
                    self.user_prompt = Some(prompt);
                }
            }
            Line::AiTitle(title) => self.title = Some(title),
            Line::Other => {}
        }
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{cut}\u{2026}")
    }
}

type Shared = Arc<Mutex<HashMap<String, Tracked>>>;

/// One live permission card waiting on a hook connection.
struct HookPending {
    id: String,
    tool: String,
    reply: Option<oneshot::Sender<Value>>,
    deadline_ms: i64,
}

/// The pending id a control targets; None for movement controls.
fn pending_id_of(control: &Control) -> Option<String> {
    match control {
        Control::Approve { pending_id }
        | Control::ApproveAlways { pending_id, .. }
        | Control::ApproveEdited { pending_id, .. }
        | Control::Deny { pending_id, .. }
        | Control::Answer { pending_id, .. } => Some(pending_id.clone()),
        _ => None,
    }
}

type Pendings = HashMap<String, HookPending>;

impl AgentAdapter for CcAdapter {
    fn id(&self) -> &'static str {
        "cc"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn spawn(self: Box<Self>, ctx: AdapterContext) {
        let AdapterContext {
            events,
            mut controls,
            spawn,
        } = ctx;
        let config = self.config;
        let state: Shared = Arc::new(Mutex::new(HashMap::new()));

        spawn(Box::pin(async move {
            // hook socket: bind inside the runtime; a failed bind (another
            // Shepherd owns the socket) just means no permission cards
            let (hook_tx, mut hook_rx) = tokio::sync::mpsc::unbounded_channel::<HookIn>();
            match server::bind(&config.socket_path) {
                Ok(listener) => {
                    tokio::spawn(server::serve(listener, hook_tx));
                }
                Err(e) => eprintln!("shepherd: hook socket unavailable: {e}"),
            }

            let mut tick = interval(Duration::from_secs(1));
            let mut ticks: u64 = 0;
            let mut pendings: Pendings = HashMap::new();
            let mut seq: u64 = 0;
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        ticks += 1;
                        if ticks == 1 || ticks.is_multiple_of(RESCAN_TICKS) {
                            rescan(&config, &events, &state, &mut pendings);
                        }
                        stream(&events, &state, ticks);
                        sweep_timeouts(&events, &mut pendings);
                    }
                    Some((sid, control)) = controls.recv() => {
                        apply_control(&events, &state, &mut pendings, &sid, control);
                    }
                    Some(msg) = hook_rx.recv() => {
                        handle_hook_in(&config, &events, &state, &mut pendings, &mut seq, msg);
                    }
                    else => break,
                }
            }
        }));
    }
}

/// A card that expired: reply timeout to the hook (it is likely already
/// gone), mark the card auto-proceeded, and resume Running.
fn sweep_timeouts(events: &UnboundedSender<Envelope>, pendings: &mut Pendings) {
    let now = now_ms();
    let expired: Vec<String> = pendings
        .iter()
        .filter(|(_, p)| p.deadline_ms <= now)
        .map(|(sid, _)| sid.clone())
        .collect();
    for sid in expired {
        if let Some(p) = pendings.remove(&sid) {
            send_reply(p.reply, decision_value("timeout"));
            send_activity(
                events,
                &sid,
                "warn",
                format!("! {} auto-proceeded after timeout", p.tool),
            );
            let _ = events.send(Envelope {
                agent: "cc",
                session_id: sid,
                event: AgentEvent::PendingCleared { pending_id: p.id },
            });
        }
    }
}

/// Deliver a decision to the waiting hook; a failed send means the hook
/// process died while the user was deciding - nothing to do.
fn send_reply(reply: Option<oneshot::Sender<Value>>, decision: Value) {
    if let Some(tx) = reply {
        let _ = tx.send(decision);
    }
}

fn handle_hook_in(
    config: &ShepherdConfig,
    events: &UnboundedSender<Envelope>,
    state: &Shared,
    pendings: &mut Pendings,
    seq: &mut u64,
    msg: HookIn,
) {
    match msg {
        HookIn::Permission {
            session_id,
            tool,
            input,
            reply,
        } => {
            let tracked = state.lock().unwrap().contains_key(&session_id);
            if !tracked {
                // unknown session (shepherd restarted mid-flight): fail open
                send_reply(Some(reply), decision_value("pass"));
                return;
            }
            let always = state
                .lock()
                .unwrap()
                .get(&session_id)
                .is_some_and(|t| t.allowed.contains(&tool));
            if always {
                send_activity(
                    events,
                    &session_id,
                    "sys",
                    format!("Auto-approved {tool} (always allowed this session)"),
                );
                send_reply(Some(reply), decision_value("approve_always"));
                return;
            }
            if pendings.contains_key(&session_id) {
                // one card at a time; Claude Code serializes per session, so
                // this only happens on races - let the terminal handle it
                send_reply(Some(reply), decision_value("pass"));
                return;
            }
            clear_nudge(events, state, &session_id);
            *seq += 1;
            let id = format!("hp{seq}");
            let deadline = now_ms() + (config.hook_timeout_secs as i64) * 1000;
            let (command, reason) = card_fields(&tool, &input);
            let _ = events.send(Envelope {
                agent: "cc",
                session_id: session_id.clone(),
                event: AgentEvent::PermissionRequested {
                    pending: PendingPermission {
                        id: id.clone(),
                        tool: tool.clone(),
                        command,
                        reason,
                    },
                },
            });
            pendings.insert(
                session_id,
                HookPending {
                    id,
                    tool,
                    reply: Some(reply),
                    deadline_ms: deadline,
                },
            );
        }
        HookIn::Notification {
            session_id,
            message,
        } => {
            let mut st = state.lock().unwrap();
            let Some(t) = st.get_mut(&session_id) else {
                return;
            };
            if t.nudge.is_some() || pendings.contains_key(&session_id) {
                return;
            }
            *seq += 1;
            let id = format!("hn{seq}");
            t.nudge = Some(id.clone());
            let _ = events.send(Envelope {
                agent: "cc",
                session_id,
                event: AgentEvent::InputRequested {
                    pending: PendingInput {
                        id,
                        question: message,
                        suggestions: Vec::new(),
                    },
                },
            });
        }
        HookIn::Stop {
            session_id,
            last_message,
        } => {
            clear_nudge(events, state, &session_id);
            let mut st = state.lock().unwrap();
            let Some(t) = st.get_mut(&session_id) else {
                return;
            };
            if last_message.trim().is_empty() {
                return; // no-op turn (e.g. session opened and immediately stopped)
            }
            t.turn_recorded = true;
            let files = std::mem::take(&mut t.turn_files);
            let _ = events.send(Envelope {
                agent: "cc",
                session_id,
                event: AgentEvent::TurnFinished {
                    outcome: truncate_chars(&last_message, OUTCOME_MAX),
                    files,
                },
            });
        }
        HookIn::Dropped { session_id } => {
            if let Some(p) = pendings.remove(&session_id) {
                send_activity(
                    events,
                    &session_id,
                    "warn",
                    format!("! {} auto-proceeded (Claude stopped waiting)", p.tool),
                );
                let _ = events.send(Envelope {
                    agent: "cc",
                    session_id,
                    event: AgentEvent::PendingCleared { pending_id: p.id },
                });
            }
        }
    }
}

/// Bash cards carry the raw command (edit-before-approve needs it verbatim);
/// other tools show the same label the activity log uses.
fn card_fields(tool: &str, input: &Value) -> (String, String) {
    if tool == "Bash" {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let reason = input
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        (command, reason)
    } else {
        let l = transcript::tool_label(tool, input);
        (l.label, String::new())
    }
}

/// User decision from the panel: reply to the hook and log it.
fn apply_control(
    events: &UnboundedSender<Envelope>,
    state: &Shared,
    pendings: &mut Pendings,
    sid: &str,
    control: Control,
) {
    let Some(pending_id) = pending_id_of(&control) else {
        not_supported(events, sid, control);
        return;
    };
    // nudges (input cards) accept no answer: clear if the panel sends one
    {
        let mut st = state.lock().unwrap();
        if st
            .get(sid)
            .is_some_and(|t| t.nudge.as_deref() == Some(pending_id.as_str()))
        {
            if let Some(t) = st.get_mut(sid) {
                t.nudge = None;
            }
            send_activity(
                events,
                sid,
                "sys",
                ">> nudge dismissed; answer in the terminal".to_string(),
            );
            let _ = events.send(Envelope {
                agent: "cc",
                session_id: sid.to_string(),
                event: AgentEvent::PendingCleared { pending_id },
            });
            return;
        }
    }
    let Some(p) = pendings.get(sid) else { return };
    if p.id != pending_id {
        return; // stale decision for an already-replaced card
    }
    let p = pendings.remove(sid).expect("just checked");
    let tool = p.tool.clone();
    match control {
        Control::Approve { .. } => {
            send_activity(events, sid, "user", format!("OK approved {tool}"));
            send_reply(p.reply, decision_value("approve"));
        }
        Control::ApproveAlways { tool, .. } => {
            if let Some(t) = state.lock().unwrap().get_mut(sid) {
                t.allowed.insert(tool.clone());
            }
            send_activity(
                events,
                sid,
                "user",
                format!("OK {tool} added to always-allow for this session"),
            );
            send_reply(p.reply, decision_value("approve_always"));
        }
        Control::ApproveEdited { command, .. } => {
            send_activity(events, sid, "user", "OK approved (edited):".to_string());
            send_activity(events, sid, "user", command.clone());
            send_reply(
                p.reply,
                serde_json::json!({"decision": "approve_edited", "command": command}),
            );
        }
        Control::Deny { note, .. } => {
            let note = note
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| "Denied - take a different approach.".to_string());
            send_activity(events, sid, "user", format!("X denied: {note}"));
            send_reply(
                p.reply,
                serde_json::json!({"decision": "deny", "note": note}),
            );
        }
        // unreachable: filtered above
        _ => {}
    }
}

fn clear_nudge(events: &UnboundedSender<Envelope>, state: &Shared, sid: &str) {
    let mut st = state.lock().unwrap();
    if let Some(t) = st.get_mut(sid) {
        if let Some(id) = t.nudge.take() {
            let _ = events.send(Envelope {
                agent: "cc",
                session_id: sid.to_string(),
                event: AgentEvent::PendingCleared { pending_id: id },
            });
        }
    }
}

/// Discover new sessions and release finished ones. A session is finished
/// when its pid is dead or its session file vanished (Claude Code removes
/// the file on normal exit; a crash leaves it behind with a dead pid).
fn rescan(
    config: &ShepherdConfig,
    events: &UnboundedSender<Envelope>,
    state: &Shared,
    pendings: &mut Pendings,
) {
    let mut started: Vec<(Tracked, Vec<String>)> = Vec::new();
    let mut finished: Vec<Tracked> = Vec::new();
    {
        let mut st = state.lock().unwrap();
        for info in discover::scan_sessions(&config.claude_sessions_dir) {
            if st.contains_key(&info.session_id) {
                continue;
            }
            let mut t = Tracked::new(config, info);
            // ponytail: whole-transcript replay at discovery; cap if these
            // ever grow big enough that the one-shot parse is noticeable
            let mut activity = Vec::new();
            tail(&mut t, &mut activity);
            t.pushed_title = t.title();
            started.push((t, activity));
        }
        let dead: Vec<String> = st
            .iter()
            .filter(|(_, t)| !t.session_file.exists() || !discover::pid_alive(t.info.pid))
            .map(|(id, _)| id.clone())
            .collect();
        for id in dead {
            // a card for a finished session can never be answered: pass
            if let Some(p) = pendings.remove(&id) {
                send_reply(p.reply, decision_value("pass"));
            }
            finished.push(st.remove(&id).expect("checked above"));
        }
    }
    for (t, activity) in started {
        let replay: Vec<String> = activity
            .into_iter()
            .rev()
            .take(REPLAY_LINES)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        send_session(events, &t);
        for line in replay {
            send_activity(events, &t.info.session_id, "tool", line);
        }
        state.lock().unwrap().insert(t.info.session_id.clone(), t);
    }
    for t in finished {
        // hooks installed: the last turn's run was recorded at its Stop; the
        // session ending must not duplicate it. Without hooks, fall back to
        // the transcript's final assistant message.
        let outcome = if t.turn_recorded {
            None
        } else {
            Some(
                t.last_text
                    .as_deref()
                    .map(|s| truncate_chars(s, OUTCOME_MAX))
                    .unwrap_or_else(|| "Session ended.".to_string()),
            )
        };
        let _ = events.send(Envelope {
            agent: "cc",
            session_id: t.info.session_id.clone(),
            event: AgentEvent::Progress {
                elapsed_ms: t.elapsed_ms(),
                tokens: t.tokens(),
            },
        });
        let _ = events.send(Envelope {
            agent: "cc",
            session_id: t.info.session_id,
            event: AgentEvent::Finished {
                outcome,
                files: t.files,
                stopped: false,
            },
        });
    }
}

/// Tail every tracked transcript and emit new activity, title refreshes, and
/// periodic progress corrections.
fn stream(events: &UnboundedSender<Envelope>, state: &Shared, ticks: u64) {
    let mut outbox: Vec<(String, AgentEvent)> = Vec::new();
    let mut updates: Vec<Tracked> = Vec::new();
    {
        let mut st = state.lock().unwrap();
        for (id, t) in st.iter_mut() {
            let mut activity = Vec::new();
            let moved = tail(t, &mut activity);
            for line in &activity {
                outbox.push((
                    id.clone(),
                    AgentEvent::Activity {
                        kind: "tool".to_string(),
                        line: line.clone(),
                    },
                ));
            }
            if moved {
                // a new turn's output: the last recorded run no longer
                // covers what the transcript holds
                t.turn_recorded = false;
                // the conversation moved: the user answered in the terminal
                if let Some(nid) = t.nudge.take() {
                    outbox.push((
                        id.clone(),
                        AgentEvent::Activity {
                            kind: "sys".to_string(),
                            line: ">> answered in the terminal".to_string(),
                        },
                    ));
                    outbox.push((id.clone(), AgentEvent::PendingCleared { pending_id: nid }));
                }
            }
            if t.title() != t.pushed_title {
                t.pushed_title = t.title();
                updates.push(t.clone());
            }
        }
    }
    for (id, event) in outbox {
        let _ = events.send(Envelope {
            agent: "cc",
            session_id: id,
            event,
        });
    }
    // Started re-emits are upserts in the registry: a title refresh.
    for t in updates {
        send_session(events, &t);
    }
    if ticks.is_multiple_of(PROGRESS_SECS) {
        let st = state.lock().unwrap();
        for (id, t) in st.iter() {
            let _ = events.send(Envelope {
                agent: "cc",
                session_id: id.clone(),
                event: AgentEvent::Progress {
                    elapsed_ms: t.elapsed_ms(),
                    tokens: t.tokens(),
                },
            });
        }
    }
}

/// A Started re-emit refreshes title/elapsed in the registry (upsert by id).
fn send_session(events: &UnboundedSender<Envelope>, t: &Tracked) {
    let _ = events.send(Envelope {
        agent: "cc",
        session_id: t.info.session_id.clone(),
        event: AgentEvent::Started {
            session: t.to_session(),
        },
    });
}

fn send_activity(events: &UnboundedSender<Envelope>, sid: &str, kind: &str, line: String) {
    let _ = events.send(Envelope {
        agent: "cc",
        session_id: sid.to_string(),
        event: AgentEvent::Activity {
            kind: kind.into(),
            line,
        },
    });
}

/// Consume newly appended complete lines from the transcript into `activity`.
/// Returns whether any complete line was consumed (the conversation moved).
/// A trailing partial line (writer mid-append) is left for the next tick.
fn tail(t: &mut Tracked, activity: &mut Vec<String>) -> bool {
    let Ok(len) = std::fs::metadata(&t.transcript).map(|m| m.len()) else {
        return false;
    };
    if len < t.offset {
        t.offset = 0; // truncated or rotated: start over
    }
    if len == t.offset {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(&t.transcript) else {
        return false;
    };
    if file.seek(SeekFrom::Start(t.offset)).is_err() {
        return false;
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return false; // ponytail: partial UTF-8 at the boundary errors; retry next tick
    }
    let Some(end) = buf.rfind('\n') else {
        return false; // no complete line yet
    };
    for line in buf[..end].lines() {
        t.apply(line, activity);
    }
    t.offset += end as u64 + 1;
    true
}

/// M2/M3 gap: controls cannot reach a terminal session. Answer instead of
/// silently dropping, so the user sees why nothing happened.
fn not_supported(events: &UnboundedSender<Envelope>, sid: &str, control: Control) {
    let what = match control {
        Control::Pause | Control::Resume => "Pause/resume",
        Control::Stop => "Stop",
        _ => "This action",
    };
    send_activity(
        events,
        sid,
        "sys",
        format!("X {what} is not supported for Claude Code sessions"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{EventSink, Registry, UiEvent};
    use std::fs;
    use tokio::sync::mpsc::unbounded_channel;

    struct Tap(Mutex<Vec<UiEvent>>);
    impl EventSink for Tap {
        fn emit(&self, event: UiEvent) {
            self.0.lock().unwrap().push(event);
        }
    }
    impl Tap {
        fn has_activity(&self, pat: &str) -> bool {
            self.0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, UiEvent::Activity { line, .. } if line.text.contains(pat)))
        }
    }

    /// Real-clock wait helper (unlike the mock suite, files change on real
    /// time, so the paused clock cannot help here).
    async fn wait_for(mut cond: impl FnMut() -> bool, secs: u64) -> bool {
        for _ in 0..secs * 5 {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        cond()
    }

    fn fixture_config(tmp: &std::path::Path, cwd: &str) -> (ShepherdConfig, PathBuf) {
        let config = ShepherdConfig {
            claude_sessions_dir: tmp.join("sessions"),
            claude_projects_dir: tmp.join("projects"),
            ..ShepherdConfig::default()
        };
        fs::create_dir_all(&config.claude_sessions_dir).unwrap();
        let project_dir = config.claude_projects_dir.join(discover::encode_cwd(cwd));
        fs::create_dir_all(&project_dir).unwrap();
        (config, project_dir)
    }

    fn write_session(config: &ShepherdConfig, pid: u32, sid: &str, cwd: &str) {
        let started = now_ms() - 5_000;
        fs::write(
            config.claude_sessions_dir.join(format!("{pid}.json")),
            format!(
                r#"{{"pid":{pid},"sessionId":"{sid}","cwd":"{cwd}","startedAt":{started},"kind":"interactive"}}"#
            ),
        )
        .unwrap();
    }

    const USAGE: &str = r#"{"input_tokens":600,"cache_creation_input_tokens":0,"cache_read_input_tokens":64000,"output_tokens":400}"#;

    fn assistant(content: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":{content},"usage":{USAGE}}},"sessionId":"SID"}}"#
        )
    }

    #[tokio::test]
    async fn discovers_streams_and_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "11111111-2222-3333-4444-555555555555";
        let cwd = "/tmp/shepherd-cc-test";
        let (config, project_dir) = fixture_config(tmp.path(), cwd);
        let transcript = project_dir.join(format!("{sid}.jsonl"));

        // pre-existing transcript: prompt, ai title, one tool, final text
        fs::write(
            &transcript,
            format!(
                "{}\n{}\n{}\n{}\n",
                r#"{"type":"user","message":{"role":"user","content":"Fix the flaky checkout tests"}}"#,
                r#"{"type":"ai-title","aiTitle":"Fix flaky checkout tests"}"#,
                assistant(
                    r#"[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"pnpm test checkout/"}}]"#
                ),
                assistant(
                    r#"[{"type":"text","text":"The flake was a hydration race; added an await."}]"#
                ),
            ),
        )
        .unwrap();
        write_session(&config, std::process::id(), sid, cwd);

        let tap = Arc::new(Tap(Mutex::new(Vec::new())));
        let registry = Arc::new(Registry::new(tap.clone() as Arc<dyn EventSink>));
        let (ctl_tx, ctl_rx) = unbounded_channel();
        registry.register_adapter("cc", ctl_tx);
        let (ev_tx, mut ev_rx) = unbounded_channel();
        let boxed: Box<dyn AgentAdapter> = Box::new(CcAdapter::new(config.clone()));
        boxed.spawn(AdapterContext {
            events: ev_tx,
            controls: ctl_rx,
            spawn: Arc::new(|f| {
                tokio::spawn(f);
            }),
        });
        let reg = registry.clone();
        tokio::spawn(async move {
            while let Some(env) = ev_rx.recv().await {
                reg.handle_event(env);
            }
        });

        // discovery: session appears with the ai-title, tokens from usage
        assert!(
            wait_for(
                || {
                    registry.snapshot().sessions.iter().any(|s| {
                        s.session.id == sid && s.session.title == "Fix flaky checkout tests"
                    })
                },
                10
            )
            .await,
            "session discovered with ai-title"
        );
        let snap = registry.snapshot();
        let s = &snap.sessions[0];
        assert_eq!(s.session.project, "shepherd-cc-test");
        assert_eq!(s.session.tokens, 64_600 + 400 * 2);
        assert!(
            tap.has_activity("> Bash pnpm test checkout/"),
            "replayed activity"
        );

        // live tailing: an appended tool line shows up as activity
        let mut transcript_file = fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        use std::io::Write;
        writeln!(
            transcript_file,
            "{}",
            assistant(r#"[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"src/cart/hydrate.ts","old_string":"a","new_string":"b"}}]"#)
        )
        .unwrap();
        assert!(
            wait_for(|| tap.has_activity("> Edit src/cart/hydrate.ts +1 -1"), 8).await,
            "appended line tailed"
        );

        // finish: session file removed -> run recorded with final text + files
        fs::remove_file(
            config
                .claude_sessions_dir
                .join(format!("{}.json", std::process::id())),
        )
        .unwrap();
        assert!(
            wait_for(
                || {
                    let snap = registry.snapshot();
                    snap.sessions.is_empty()
                        && snap.runs.len() == 1
                        && snap.runs[0]
                            .outcome
                            .starts_with("The flake was a hydration race")
                        && snap.runs[0].files == vec!["src/cart/hydrate.ts".to_string()]
                        && !snap.runs[0].stopped
                },
                10
            )
            .await,
            "finish recorded with outcome and files"
        );
    }

    #[tokio::test]
    async fn controls_answer_not_supported() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "22222222-3333-4444-5555-666666666666";
        let cwd = "/tmp/shepherd-cc-test2";
        let (config, project_dir) = fixture_config(tmp.path(), cwd);
        fs::write(project_dir.join(format!("{sid}.jsonl")), "").unwrap();
        write_session(&config, std::process::id(), sid, cwd);

        let tap = Arc::new(Tap(Mutex::new(Vec::new())));
        let registry = Arc::new(Registry::new(tap.clone() as Arc<dyn EventSink>));
        let (ctl_tx, ctl_rx) = unbounded_channel();
        registry.register_adapter("cc", ctl_tx);
        let (ev_tx, mut ev_rx) = unbounded_channel();
        let boxed: Box<dyn AgentAdapter> = Box::new(CcAdapter::new(config.clone()));
        boxed.spawn(AdapterContext {
            events: ev_tx,
            controls: ctl_rx,
            spawn: Arc::new(|f| {
                tokio::spawn(f);
            }),
        });
        let reg = registry.clone();
        tokio::spawn(async move {
            while let Some(env) = ev_rx.recv().await {
                reg.handle_event(env);
            }
        });

        assert!(
            wait_for(
                || {
                    registry
                        .snapshot()
                        .sessions
                        .iter()
                        .any(|s| s.session.id == sid)
                },
                10
            )
            .await
        );
        registry.handle_control(sid, Control::Stop);
        assert!(
            wait_for(
                || tap.has_activity("not supported for Claude Code sessions"),
                5
            )
            .await,
            "stop answers with a sys line"
        );
        assert!(
            registry
                .snapshot()
                .sessions
                .iter()
                .any(|s| s.session.id == sid),
            "session survives a stop attempt"
        );
    }

    /// A prompt-only transcript: title falls back to the first user message.
    #[tokio::test]
    async fn title_falls_back_to_first_user_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "33333333-4444-5555-6666-777777777777";
        let cwd = "/tmp/shepherd-cc-test3";
        let (config, project_dir) = fixture_config(tmp.path(), cwd);
        let long_prompt = "Please make the checkout tests stop flaking all the time, they fail every third CI run and nobody trusts the suite anymore".to_string();
        fs::write(
            project_dir.join(format!("{sid}.jsonl")),
            format!(
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{long_prompt}\"}}}}\n"
            ),
        )
        .unwrap();
        write_session(&config, std::process::id(), sid, cwd);

        let tap = Arc::new(Tap(Mutex::new(Vec::new())));
        let registry = Arc::new(Registry::new(tap as Arc<dyn EventSink>));
        let (ctl_tx, ctl_rx) = unbounded_channel();
        registry.register_adapter("cc", ctl_tx);
        let (ev_tx, mut ev_rx) = unbounded_channel();
        let boxed: Box<dyn AgentAdapter> = Box::new(CcAdapter::new(config.clone()));
        boxed.spawn(AdapterContext {
            events: ev_tx,
            controls: ctl_rx,
            spawn: Arc::new(|f| {
                tokio::spawn(f);
            }),
        });
        let reg = registry.clone();
        tokio::spawn(async move {
            while let Some(env) = ev_rx.recv().await {
                reg.handle_event(env);
            }
        });

        assert!(
            wait_for(
                || {
                    registry.snapshot().sessions.iter().any(|s| {
                        s.session.title.starts_with("Please make the checkout")
                            && s.session.title.len() < long_prompt.len()
                    })
                },
                10
            )
            .await,
            "title is the truncated first prompt"
        );
    }

    // ---------- M3 hook round trips (real socket, real adapter loop) ----------

    use crate::Pending;
    use std::io::Write as _;
    use std::os::unix::net::UnixStream as StdStream;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const M3_CWD: &str = "/tmp/shepherd-hooktest-m3";

    /// Config + dirs + registry + adapter + one discovered session.
    async fn hook_env(sid: &str, timeout_secs: u64) -> (ShepherdConfig, Arc<Registry>, Arc<Tap>) {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = Box::leak(Box::new(tmp)); // keep dirs alive for the test
        let mut config = ShepherdConfig {
            socket_path: tmp.path().join("s.sock"),
            ..ShepherdConfig::default()
        };
        config.hook_timeout_secs = timeout_secs;
        config.claude_sessions_dir = tmp.path().join("sessions");
        config.claude_projects_dir = tmp.path().join("projects");
        let project_dir = config
            .claude_projects_dir
            .join(discover::encode_cwd(M3_CWD));
        fs::create_dir_all(&config.claude_sessions_dir).unwrap();
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join(format!("{sid}.jsonl")), "").unwrap();
        write_session(&config, std::process::id(), sid, M3_CWD);

        let tap = Arc::new(Tap(Mutex::new(Vec::new())));
        let registry = Arc::new(Registry::new(tap.clone() as Arc<dyn EventSink>));
        let (ctl_tx, ctl_rx) = unbounded_channel();
        registry.register_adapter("cc", ctl_tx);
        let (ev_tx, mut ev_rx) = unbounded_channel();
        let boxed: Box<dyn AgentAdapter> = Box::new(CcAdapter::new(config.clone()));
        boxed.spawn(AdapterContext {
            events: ev_tx,
            controls: ctl_rx,
            spawn: Arc::new(|f| {
                tokio::spawn(f);
            }),
        });
        let reg = registry.clone();
        tokio::spawn(async move {
            while let Some(env) = ev_rx.recv().await {
                reg.handle_event(env);
            }
        });
        assert!(
            wait_for(
                || registry
                    .snapshot()
                    .sessions
                    .iter()
                    .any(|s| s.session.id == sid),
                10
            )
            .await,
            "session discovered"
        );
        (config, registry, tap)
    }

    async fn hook_ask(
        sock: std::path::PathBuf,
        req: serde_json::Value,
    ) -> Option<serde_json::Value> {
        let mut s = tokio::net::UnixStream::connect(&sock).await.ok()?;
        let mut line = req.to_string();
        line.push('\n');
        s.write_all(line.as_bytes()).await.ok()?;
        let mut buf = String::new();
        s.read_to_string(&mut buf).await.ok()?;
        serde_json::from_str(buf.trim()).ok()
    }

    fn permission_req(sid: &str, tool: &str, command: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "permission",
            "sessionId": sid,
            "tool": tool,
            "input": {"command": command, "description": "run it"},
            "cwd": M3_CWD,
        })
    }

    async fn wait_pending(registry: &Registry, sid: &str) -> crate::PendingPermission {
        for _ in 0..50 {
            if let Some(p) = registry
                .snapshot()
                .sessions
                .iter()
                .find(|s| s.session.id == sid)
                .and_then(|s| match s.pending.as_ref() {
                    Some(Pending::Permission(p)) => Some(p.clone()),
                    _ => None,
                })
            {
                return p.clone();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("permission card never appeared");
    }

    fn is_running(registry: &Registry, sid: &str) -> bool {
        registry
            .snapshot()
            .sessions
            .iter()
            .any(|s| s.session.id == sid && s.session.status == Status::Running)
    }

    #[tokio::test]
    async fn hook_permission_round_trip_approve() {
        let sid = "aaaaaaaa-0000-0000-0000-000000000001";
        let (config, registry, tap) = hook_env(sid, 30).await;

        let reply = tokio::spawn(hook_ask(
            config.socket_path.clone(),
            permission_req(sid, "Bash", "pnpm test auth/"),
        ));
        let p = wait_pending(&registry, sid).await;
        assert_eq!(
            p.command, "pnpm test auth/",
            "raw command so edit-before-approve works"
        );
        assert_eq!(p.reason, "run it");
        assert_eq!(registry.waiting_count(), 1);

        registry.handle_control(sid, Control::Approve { pending_id: p.id });
        assert_eq!(
            reply.await.unwrap().expect("hook got a reply"),
            serde_json::json!({"decision": "approve"})
        );
        assert!(
            wait_for(|| tap.has_activity("OK approved Bash"), 5).await,
            "approval logged"
        );
        assert!(wait_for(|| is_running(&registry, sid), 5).await);
    }

    #[tokio::test]
    async fn always_allow_auto_approves_later_hook_requests() {
        let sid = "aaaaaaaa-0000-0000-0000-000000000002";
        let (config, registry, tap) = hook_env(sid, 30).await;

        let first = tokio::spawn(hook_ask(
            config.socket_path.clone(),
            permission_req(sid, "Bash", "pnpm test"),
        ));
        let p = wait_pending(&registry, sid).await;
        registry.handle_control(
            sid,
            Control::ApproveAlways {
                pending_id: p.id,
                tool: "Bash".to_string(),
            },
        );
        assert_eq!(
            first.await.unwrap().unwrap(),
            serde_json::json!({"decision": "approve_always"})
        );

        // the next Bash never waits: immediate decision, no card
        let second = tokio::spawn(hook_ask(
            config.socket_path.clone(),
            permission_req(sid, "Bash", "rm -rf build"),
        ));
        assert_eq!(
            second.await.unwrap().unwrap(),
            serde_json::json!({"decision": "approve_always"})
        );
        assert!(
            wait_for(
                || tap.has_activity("Auto-approved Bash (always allowed this session)"),
                5
            )
            .await
        );
        assert_eq!(registry.waiting_count(), 0);
        assert_eq!(
            registry.snapshot().sessions[0].session.allowed_tools,
            vec!["Bash".to_string()],
            "always-allow chip visible in the panel"
        );
    }

    #[tokio::test]
    async fn deny_note_and_edited_command_round_trip() {
        let sid = "aaaaaaaa-0000-0000-0000-000000000003";
        let (config, registry, tap) = hook_env(sid, 30).await;

        let deny = tokio::spawn(hook_ask(
            config.socket_path.clone(),
            permission_req(sid, "Bash", "rm -rf build"),
        ));
        let p = wait_pending(&registry, sid).await;
        registry.handle_control(
            sid,
            Control::Deny {
                pending_id: p.id,
                note: Some("use git clean".to_string()),
            },
        );
        assert_eq!(
            deny.await.unwrap().unwrap(),
            serde_json::json!({"decision": "deny", "note": "use git clean"})
        );
        assert!(wait_for(|| tap.has_activity("X denied: use git clean"), 5).await);

        let edited = tokio::spawn(hook_ask(
            config.socket_path.clone(),
            permission_req(sid, "Bash", "pnpm test"),
        ));
        let p = wait_pending(&registry, sid).await;
        registry.handle_control(
            sid,
            Control::ApproveEdited {
                pending_id: p.id,
                command: "pnpm test --filter unit".to_string(),
            },
        );
        assert_eq!(
            edited.await.unwrap().unwrap(),
            serde_json::json!({"decision": "approve_edited", "command": "pnpm test --filter unit"})
        );
        assert!(wait_for(|| tap.has_activity("OK approved (edited):"), 5).await);
        assert!(wait_for(|| tap.has_activity("pnpm test --filter unit"), 5).await);
    }

    #[tokio::test]
    async fn card_expires_after_hook_timeout() {
        let sid = "aaaaaaaa-0000-0000-0000-000000000004";
        let (config, registry, tap) = hook_env(sid, 2).await;

        let reply = tokio::spawn(hook_ask(
            config.socket_path.clone(),
            permission_req(sid, "Bash", "pnpm test"),
        ));
        wait_pending(&registry, sid).await;
        assert_eq!(registry.waiting_count(), 1);

        // never act: the card expires, the hook times out, the badge clears
        assert_eq!(
            reply.await.unwrap().unwrap(),
            serde_json::json!({"decision": "timeout"})
        );
        assert!(
            wait_for(|| tap.has_activity("Bash auto-proceeded after timeout"), 5).await,
            "timeout is visible in the log"
        );
        assert!(wait_for(|| is_running(&registry, sid), 5).await);
        assert_eq!(registry.waiting_count(), 0);
    }

    #[tokio::test]
    async fn dead_hook_clears_its_card() {
        let sid = "aaaaaaaa-0000-0000-0000-000000000005";
        let (config, registry, tap) = hook_env(sid, 60).await;

        // a hook that dies while waiting (Claude killed it at its own timeout)
        let mut conn = StdStream::connect(&config.socket_path).unwrap();
        let mut line = permission_req(sid, "Bash", "pnpm test").to_string();
        line.push('\n');
        conn.write_all(line.as_bytes()).unwrap();
        wait_pending(&registry, sid).await;
        drop(conn);

        assert!(
            wait_for(
                || tap.has_activity("Bash auto-proceeded (Claude stopped waiting)"),
                5
            )
            .await
        );
        assert!(wait_for(|| is_running(&registry, sid), 5).await);
    }

    #[tokio::test]
    async fn idle_notification_becomes_nudge_cleared_by_terminal_activity() {
        let sid = "aaaaaaaa-0000-0000-0000-000000000006";
        let (config, registry, tap) = hook_env(sid, 60).await;

        let mut conn = StdStream::connect(&config.socket_path).unwrap();
        let nudge = serde_json::json!({
            "type": "notification",
            "sessionId": sid,
            "message": "Claude is waiting for your input",
        });
        let mut line = nudge.to_string();
        line.push('\n');
        conn.write_all(line.as_bytes()).unwrap();
        drop(conn);

        assert!(
            wait_for(
                || {
                    registry.snapshot().sessions.iter().any(|s| {
                        s.session.id == sid
                            && matches!(
                                &s.pending,
                                Some(Pending::Input(q)) if q.question
                                    == "Claude is waiting for your input"
                            )
                    })
                },
                5
            )
            .await,
            "nudge card appears"
        );

        // the user answers in the terminal: the transcript moves, card retires
        let transcript = discover::transcript_path(&config.claude_projects_dir, M3_CWD, sid);
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(f, "{}", assistant(r#"[{"type":"text","text":"thanks"}]"#)).unwrap();
        assert!(
            wait_for(
                || {
                    registry
                        .snapshot()
                        .sessions
                        .iter()
                        .any(|s| s.session.id == sid && s.pending.is_none())
                },
                10
            )
            .await,
            "nudge clears when the conversation moves"
        );
        assert!(wait_for(|| tap.has_activity(">> answered in the terminal"), 5).await);
    }

    /// M4: the Stop hook records a run per completed turn while the session
    /// stays open; ending the session does not duplicate the recorded run,
    /// but a turn that dies without a Stop still gets its transcript run.
    #[tokio::test]
    async fn stop_hook_records_turn_runs_without_duplicating_finish() {
        let sid = "aaaaaaaa-0000-0000-0000-000000000007";
        let (config, registry, tap) = hook_env(sid, 30).await;

        // turn 1: one file edit, then the Stop hook fires
        let transcript = discover::transcript_path(&config.claude_projects_dir, M3_CWD, sid);
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(
            f,
            "{}",
            assistant(
                r#"[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"src/a.ts","old_string":"a","new_string":"b"}}]"#
            )
        )
        .unwrap();
        assert!(
            wait_for(|| tap.has_activity("> Edit src/a.ts +1 -1"), 8).await,
            "turn-1 work tailed before the Stop arrives"
        );

        let reply = tokio::spawn(hook_ask(
            config.socket_path.clone(),
            serde_json::json!({
                "type": "stop",
                "sessionId": sid,
                "lastMessage": "Done: fixed the hydration race and added an await.",
            }),
        ));
        assert_eq!(
            reply.await.unwrap().unwrap(),
            serde_json::json!({"decision": "ack"})
        );

        assert!(
            wait_for(
                || {
                    let snap = registry.snapshot();
                    snap.runs.len() == 1 && snap.sessions.len() == 1
                },
                8
            )
            .await,
            "run recorded while the session stays open"
        );
        let run = registry.snapshot().runs[0].clone();
        assert!(run.outcome.starts_with("Done: fixed the hydration"));
        assert_eq!(run.files, vec!["src/a.ts".to_string()]);
        assert!(!run.stopped);

        // turn 2 starts, then the terminal closes mid-turn: the finish path
        // records a second run from the transcript's final message
        writeln!(
            f,
            "{}",
            assistant(
                r#"[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"pnpm lint"}}]"#
            )
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            assistant(r#"[{"type":"text","text":"lint is clean now"}]"#)
        )
        .unwrap();
        assert!(wait_for(|| tap.has_activity("> Bash pnpm lint"), 8).await);
        fs::remove_file(
            config
                .claude_sessions_dir
                .join(format!("{}.json", std::process::id())),
        )
        .unwrap();

        assert!(
            wait_for(
                || {
                    let snap = registry.snapshot();
                    snap.sessions.is_empty() && snap.runs.len() == 2
                },
                10
            )
            .await,
            "session removed, second run recorded"
        );
        assert_eq!(registry.snapshot().runs[0].outcome, "lint is clean now");
        assert!(registry.snapshot().runs[1]
            .outcome
            .starts_with("Done: fixed the hydration"));
    }
}
