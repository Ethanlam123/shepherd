//! Claude Code adapter: discovers live sessions from `~/.claude/sessions`
//! and streams their transcripts as agent events. M2 scope is read-only:
//! no permission cards (M3 hooks) and pause/stop stay unsupported (decision
//! 1: terminal sessions cannot be driven from outside).

pub mod discover;
pub mod install;
pub mod transcript;

pub use discover::CcSessionInfo;

use crate::config::ShepherdConfig;
use crate::{now_ms, AdapterContext, AgentAdapter, AgentEvent, Control, Envelope, Session, Status};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;
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
            allowed_tools: Vec::new(),
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
                            self.files.push(file);
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
            let mut tick = interval(Duration::from_secs(1));
            let mut ticks: u64 = 0;
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        ticks += 1;
                        if ticks == 1 || ticks.is_multiple_of(RESCAN_TICKS) {
                            rescan(&config, &events, &state);
                        }
                        stream(&events, &state, ticks);
                    }
                    Some((sid, control)) = controls.recv() => {
                        not_supported(&events, &sid, control);
                    }
                    else => break,
                }
            }
        }));
    }
}

/// Discover new sessions and release finished ones. A session is finished
/// when its pid is dead or its session file vanished (Claude Code removes
/// the file on normal exit; a crash leaves it behind with a dead pid).
fn rescan(config: &ShepherdConfig, events: &UnboundedSender<Envelope>, state: &Shared) {
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
        let outcome = t
            .last_text
            .as_deref()
            .map(|s| truncate_chars(s, OUTCOME_MAX))
            .unwrap_or_else(|| "Session ended.".to_string());
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
                outcome: Some(outcome),
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
            tail(t, &mut activity);
            for line in &activity {
                outbox.push((
                    id.clone(),
                    AgentEvent::Activity {
                        kind: "tool".to_string(),
                        line: line.clone(),
                    },
                ));
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
/// A trailing partial line (writer mid-append) is left for the next tick.
fn tail(t: &mut Tracked, activity: &mut Vec<String>) {
    let Ok(len) = std::fs::metadata(&t.transcript).map(|m| m.len()) else {
        return;
    };
    if len < t.offset {
        t.offset = 0; // truncated or rotated: start over
    }
    if len == t.offset {
        return;
    }
    let Ok(mut file) = std::fs::File::open(&t.transcript) else {
        return;
    };
    if file.seek(SeekFrom::Start(t.offset)).is_err() {
        return;
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return; // ponytail: partial UTF-8 at the boundary errors; retry next tick
    }
    let Some(end) = buf.rfind('\n') else {
        return; // no complete line yet
    };
    for line in buf[..end].lines() {
        t.apply(line, activity);
    }
    t.offset += end as u64 + 1;
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
}
