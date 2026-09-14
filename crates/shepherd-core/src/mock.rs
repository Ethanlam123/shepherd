//! Mock adapter: drives the panel with the prototype's scripted sessions so
//! the whole UI is exercisable before any real agent integration. Scripts are
//! the prototype's simulation data ported verbatim (copy, states, timings).

use crate::{
    now_ms, AdapterContext, AgentAdapter, AgentEvent, Control, Envelope, PendingInput,
    PendingPermission, Session, Status,
};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{interval, Duration};

#[derive(Clone)]
pub struct Script {
    pub title: &'static str,
    pub project: &'static str,
    pub steps: Vec<Step>,
}

#[derive(Clone)]
pub struct Step {
    /// Seconds-from-start gate, in ms (prototype `at` semantics).
    pub at_ms: u64,
    pub kind: StepKind,
}

#[derive(Clone)]
pub enum StepKind {
    Log {
        kind: &'static str,
        text: &'static str,
    },
    Permission {
        tool: &'static str,
        command: &'static str,
        reason: &'static str,
    },
    Input {
        question: &'static str,
        suggestions: &'static [&'static str],
    },
    Finish {
        outcome: &'static str,
        files: &'static [&'static str],
    },
}

/// Prototype step factories: L/P/Q/F.
fn l(at: u64, kind: &'static str, text: &'static str) -> Step {
    Step {
        at_ms: at * 1000,
        kind: StepKind::Log { kind, text },
    }
}
fn p(at: u64, tool: &'static str, command: &'static str, reason: &'static str) -> Step {
    Step {
        at_ms: at * 1000,
        kind: StepKind::Permission {
            tool,
            command,
            reason,
        },
    }
}
fn q(at: u64, question: &'static str, suggestions: &'static [&'static str]) -> Step {
    Step {
        at_ms: at * 1000,
        kind: StepKind::Input {
            question,
            suggestions,
        },
    }
}
fn f(at: u64, outcome: &'static str, files: &'static [&'static str]) -> Step {
    Step {
        at_ms: at * 1000,
        kind: StepKind::Finish { outcome, files },
    }
}

// ---------- scripts (ported verbatim from the prototype) ----------

fn script_auth() -> Script {
    Script {
        title: "Refactor auth middleware to typed sessions",
        project: "api-server",
        steps: vec![
            l(2, "tool", "> Read src/auth/middleware.ts"),
            l(4, "info", "> Grep \"verifyToken(\" - 12 matches in 4 files"),
            l(7, "tool", "> Read src/auth/tokens.ts (214 lines)"),
            l(10, "warn", "! Session table needs a schema migration before the refactor"),
            p(12, "Bash", "pnpm prisma migrate reset --force",
              "Reset the dev database to apply the new sessions migration. Wipes local data in dev.db only."),
            l(15, "ok", "OK prisma migrate reset - 7 tables recreated"),
            l(18, "tool", "> Edit src/auth/tokens.ts +36 -18"),
            l(21, "tool", "> Edit src/auth/middleware.ts +52 -40"),
            l(24, "tool", "> Bash pnpm test auth/"),
            l(25, "ok", "OK 18 tests passed"),
            f(27, "Replaced string session tokens with typed Session records. Auth tests pass against the reset schema; no API surface changed.",
              &["src/auth/tokens.ts", "src/auth/middleware.ts", "prisma/schema.prisma"]),
        ],
    }
}

fn script_flaky() -> Script {
    Script {
        title: "Fix flaky checkout tests",
        project: "web-store",
        steps: vec![
            l(2, "info", "> Bash pnpm test checkout/"),
            l(4, "warn", "! Failed 2 of 14 - retry #2 passed, race in cart hydration"),
            l(6, "tool", "> Read tests/checkout.spec.ts"),
            l(9, "tool", "> Edit tests/checkout.spec.ts +12 -4"),
            l(12, "tool", "> Bash pnpm test checkout/"),
            l(13, "ok", "OK 14 tests passed"),
            l(15, "tool", "> Edit src/cart/hydrate.ts +3 -1"),
            l(18, "tool", "> Bash pnpm test"),
            l(19, "ok", "OK 96 tests passed"),
            f(21, "The flake was a hydration race: cart state was asserted before restore resolved. Added an explicit await in the spec and a guard in hydrate().",
              &["tests/checkout.spec.ts", "src/cart/hydrate.ts"]),
        ],
    }
}

fn script_mdx() -> Script {
    Script {
        title: "Migrate docs to MDX",
        project: "docs-site",
        steps: vec![
            l(2, "tool", "> Read docs/pages/**.md (38 files)"),
            l(5, "ok", "OK converted 24 pages"),
            q(8, "Two paths for the 14 legacy redirect pages: convert them to MDX and keep the URL redirects, or archive them under /legacy and drop the routes. Which way?",
              &["Convert and keep redirects", "Archive under /legacy"]),
            l(12, "tool", "> Convert remaining 14 pages"),
            l(15, "ok", "OK 38 of 38 pages converted"),
            l(17, "tool", "> Write docs/redirects.mjs"),
            l(19, "tool", "> Bash pnpm build docs"),
            l(20, "ok", "OK build passed - 38 pages"),
            f(22, "All 38 docs pages now MDX with frontmatter. Redirect map generated from the old URL table; build passes with no broken links.",
              &["docs/pages/**.mdx", "docs/redirects.mjs"]),
        ],
    }
}

fn script_audit() -> Script {
    Script {
        title: "Weekly dependency audit",
        project: "monorepo",
        steps: vec![
            l(2, "tool", "> Bash pnpm audit"),
            l(4, "warn", "! 17 advisories: 3 moderate, 14 low"),
            l(7, "tool", "> Bash pnpm outdated"),
            l(10, "info", "> 14 patch and 3 minor updates available in workspace"),
            l(14, "tool", "> Read pnpm-lock.yaml (changes staged)"),
            l(17, "sys", "Reviewing update impact per package"),
            p(20, "Bash", "pnpm -r update",
              "Apply the 14 patch and 3 minor updates across the workspace. Rewrites pnpm-lock.yaml and runs install."),
            l(23, "ok", "OK 17 packages updated"),
            l(26, "tool", "> Bash pnpm -r test"),
            l(28, "ok", "OK 132 tests passed"),
            l(30, "tool", "> Write SECURITY-REPORT.md"),
            f(32, "All patch and minor updates applied, no breaking changes, 132 tests passing. Audit report saved with the remaining low advisories triaged.",
              &["pnpm-lock.yaml", "SECURITY-REPORT.md"]),
        ],
    }
}

fn script_index() -> Script {
    Script {
        title: "Index codebase for semantic search",
        project: "monorepo",
        steps: vec![
            l(2, "info", "> Scan workspace - 1,842 files"),
            l(6, "tool", "> Embed packages/shared (412 files)"),
            l(8, "ok", "OK 412 embedded"),
            l(12, "tool", "> Embed apps/api (603 files)"),
            l(14, "ok", "OK 603 embedded"),
            l(19, "tool", "> Embed apps/web (701 files)"),
            l(21, "ok", "OK 701 embedded"),
            l(26, "tool", "> Build vector index"),
            l(27, "ok", "OK index built - 1,842 files, 6.1k chunks"),
            f(29, "Workspace fully indexed for semantic search. Chunks stored with symbol anchors; incremental watch mode left running.",
              &["index/manifest.json", "index/chunks.bin"]),
        ],
    }
}

fn pool_orders() -> Script {
    Script {
        title: "Add pagination to orders API",
        project: "api-server",
        steps: vec![
            l(2, "tool", "> Read src/routes/orders.ts"),
            l(5, "tool", "> Grep \"findMany(\" - 9 matches"),
            l(8, "tool", "> Edit src/routes/orders.ts +28 -6"),
            l(11, "tool", "> Edit src/validators/orders.ts +14 -0"),
            l(14, "tool", "> Bash pnpm test orders/"),
            l(15, "ok", "OK 11 tests passed"),
            f(17, "Cursor pagination on GET /orders with limit/after params. Backwards compatible: no params returns the first page like before.",
              &["src/routes/orders.ts", "src/validators/orders.ts"]),
        ],
    }
}

fn pool_tokens() -> Script {
    Script {
        title: "Extract design tokens to CSS variables",
        project: "web-store",
        steps: vec![
            l(2, "tool", "> Read src/styles/*.css (6 files)"),
            l(5, "info", "> 63 hardcoded colors found"),
            l(8, "tool", "> Write src/styles/tokens.css"),
            l(11, "tool", "> Edit 5 component stylesheets"),
            l(14, "tool", "> Bash pnpm build"),
            l(15, "ok", "OK build passed"),
            f(17, "All repeated colors, radii and shadows moved into tokens.css; visual output unchanged.",
              &["src/styles/tokens.css", "src/styles/components.css"]),
        ],
    }
}

fn pool_ci() -> Script {
    Script {
        title: "Tighten GitHub Actions cache usage",
        project: "monorepo",
        steps: vec![
            l(2, "tool", "> Read .github/workflows/*.yml (5 files)"),
            l(5, "info", "> 3 jobs restore full pnpm store on every run"),
            l(8, "tool", "> Edit 3 workflow files"),
            l(11, "ok", "OK cache keys pinned to lockfile hash"),
            f(13, "CI jobs now key the pnpm store cache on the lockfile hash instead of restoring everything; warm runs skip the install step.",
              &[".github/workflows/ci.yml", ".github/workflows/release.yml"]),
        ],
    }
}

fn pool_docs() -> Script {
    Script {
        title: "Refresh API docs from OpenAPI spec",
        project: "api-server",
        steps: vec![
            l(2, "tool", "> Read openapi.yaml (1,204 lines)"),
            l(6, "tool", "> Diff against docs/api/*.md"),
            l(10, "warn", "! 4 endpoints missing, 2 stale examples"),
            l(13, "tool", "> Edit docs/api/*.md"),
            l(16, "ok", "OK 6 docs pages updated"),
            f(18, "Docs now match the spec: added the 4 missing endpoints, refreshed stale request examples, regenerated the auth section.",
              &["docs/api/*.md"]),
        ],
    }
}

// ---------- adapter ----------

struct MockSession {
    id: String,
    title: String,
    project: String,
    status: Status,
    elapsed_ms: u64,
    tokens: u64,
    allowed: HashSet<String>,
    /// Tool of the prompt currently blocking the session, for log lines.
    pending_tool: Option<String>,
    pending_seq: u64,
    script: VecDeque<Step>,
}

impl MockSession {
    fn new(id: String, script: &Script, rng: &mut u64) -> Self {
        Self {
            id,
            title: script.title.to_string(),
            project: script.project.to_string(),
            status: Status::Running,
            elapsed_ms: 0,
            tokens: 1200 + lcg(rng) % 900,
            allowed: HashSet::new(),
            pending_tool: None,
            pending_seq: 1,
            script: script.steps.clone().into_iter().collect(),
        }
    }

    fn to_session(&self, agent: &str) -> Session {
        let mut allowed: Vec<String> = self.allowed.iter().cloned().collect();
        allowed.sort();
        Session {
            id: self.id.clone(),
            agent: agent.to_string(),
            title: self.title.clone(),
            project: self.project.clone(),
            cwd: format!("~/code/{}", self.project),
            status: self.status,
            started_at: now_ms(),
            elapsed_ms: self.elapsed_ms,
            tokens: self.tokens,
            allowed_tools: allowed,
        }
    }
}

pub struct MockAdapter {
    agent: &'static str,
    initial: Vec<Script>,
    pool: Vec<Script>,
}

/// All four mock agents, one per identity hue. Each owns its scripts and its
/// respawn pool so the agent mix stays stable across respawns.
pub fn adapters() -> Vec<Box<dyn AgentAdapter>> {
    vec![
        Box::new(MockAdapter {
            agent: "cc",
            initial: vec![script_auth(), script_flaky()],
            pool: vec![pool_orders()],
        }),
        Box::new(MockAdapter {
            agent: "oc",
            initial: vec![script_mdx()],
            pool: vec![pool_tokens()],
        }),
        Box::new(MockAdapter {
            agent: "pi",
            initial: vec![script_audit()],
            pool: vec![pool_ci()],
        }),
        Box::new(MockAdapter {
            agent: "hm",
            initial: vec![script_index()],
            pool: vec![pool_docs()],
        }),
    ]
}

fn display_name(agent: &str) -> &'static str {
    match agent {
        "cc" => "Claude Code",
        "oc" => "OpenCode",
        "pi" => "Pi",
        "hm" => "Hermes",
        _ => "Agent",
    }
}

/// Tiny LCG so the mock has no rand dependency.
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 33
}

/// Shared loop state, clonable so respawn tasks can start sessions.
#[derive(Clone)]
struct LoopCtx {
    agent: &'static str,
    events: UnboundedSender<Envelope>,
    sessions: Arc<Mutex<Vec<MockSession>>>,
    seq: Arc<AtomicU64>,
    pool: Vec<Script>,
    pool_idx: Arc<AtomicU64>,
}

impl LoopCtx {
    fn send(&self, session_id: &str, event: AgentEvent) {
        let _ = self.events.send(Envelope {
            agent: self.agent,
            session_id: session_id.to_string(),
            event,
        });
    }

    fn log(&self, session_id: &str, kind: &str, text: impl Into<String>) {
        self.send(
            session_id,
            AgentEvent::Activity {
                kind: kind.to_string(),
                line: text.into(),
            },
        );
    }

    fn start_session(&self, script: &Script, rng: &mut u64) {
        let id = format!(
            "{}-{}",
            self.agent,
            self.seq.fetch_add(1, Ordering::Relaxed)
        );
        let s = MockSession::new(id.clone(), script, rng);
        self.log(&id, "sys", format!("Session started - {}", s.title));
        self.send(
            &id,
            AgentEvent::Started {
                session: s.to_session(self.agent),
            },
        );
        self.sessions.lock().unwrap().push(s);
    }

    /// Advance scripts for running sessions; remove finished ones and
    /// schedule respawns (normal finishes only - stop never respawns).
    fn process_steps(&self, rng: &mut u64) {
        let mut finished: Vec<String> = Vec::new();
        {
            let mut sx = self.sessions.lock().unwrap();
            for s in sx.iter_mut() {
                if s.status != Status::Running {
                    continue;
                }
                while s.status == Status::Running {
                    let Some(step) = s.script.front() else { break };
                    if step.at_ms > s.elapsed_ms {
                        break;
                    }
                    let step = s.script.pop_front().unwrap();
                    match step.kind {
                        StepKind::Log { kind, text } => self.log(&s.id, kind, text),
                        StepKind::Permission {
                            tool,
                            command,
                            reason,
                        } => {
                            if s.allowed.contains(tool) {
                                self.log(
                                    &s.id,
                                    "sys",
                                    format!("Auto-approved {tool} (always allowed this session)"),
                                );
                                continue;
                            }
                            s.status = Status::Waiting;
                            s.pending_tool = Some(tool.to_string());
                            self.log(&s.id, "warn", format!("! Waiting for approval - {tool}"));
                            let pending = PendingPermission {
                                id: format!("{}-p{}", s.id, s.pending_seq),
                                tool: tool.to_string(),
                                command: command.to_string(),
                                reason: reason.to_string(),
                            };
                            s.pending_seq += 1;
                            self.send(&s.id, AgentEvent::PermissionRequested { pending });
                            break;
                        }
                        StepKind::Input {
                            question,
                            suggestions,
                        } => {
                            s.status = Status::Waiting;
                            self.log(&s.id, "warn", format!("! Question for you - {question}"));
                            let pending = PendingInput {
                                id: format!("{}-p{}", s.id, s.pending_seq),
                                question: question.to_string(),
                                suggestions: suggestions.iter().map(|x| x.to_string()).collect(),
                            };
                            s.pending_seq += 1;
                            self.send(&s.id, AgentEvent::InputRequested { pending });
                            break;
                        }
                        StepKind::Finish { outcome, files } => {
                            self.log(&s.id, "ok", "OK run complete");
                            self.send(
                                &s.id,
                                AgentEvent::Finished {
                                    outcome: Some(outcome.to_string()),
                                    files: files.iter().map(|x| x.to_string()).collect(),
                                    stopped: false,
                                },
                            );
                            finished.push(s.id.clone());
                            break;
                        }
                    }
                }
            }
            sx.retain(|s| !finished.contains(&s.id));
        }
        // prototype respawns one session per normal finish, 16-25s later
        for _ in &finished {
            let idx = self.pool_idx.fetch_add(1, Ordering::Relaxed) as usize % self.pool.len();
            let script = self.pool[idx].clone();
            let ctx = self.clone();
            let delay = 16_000 + lcg(rng) % 9_000;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let mut rng = delay;
                ctx.start_session(&script, &mut rng);
            });
        }
    }
}

impl AgentAdapter for MockAdapter {
    fn id(&self) -> &'static str {
        self.agent
    }

    fn display_name(&self) -> &'static str {
        display_name(self.agent)
    }

    fn spawn(self, ctx: AdapterContext) {
        let AdapterContext {
            events,
            mut controls,
        } = ctx;
        let agent = self.agent;
        let loop_ctx = LoopCtx {
            agent,
            events: events.clone(),
            sessions: Arc::new(Mutex::new(Vec::new())),
            seq: Arc::new(AtomicU64::new(1)),
            pool: self.pool,
            pool_idx: Arc::new(AtomicU64::new(0)),
        };
        // all initial sessions start immediately (prototype seed())
        let mut rng = agent.as_bytes().iter().map(|b| *b as u64).sum::<u64>() | 1;
        for script in &self.initial {
            loop_ctx.start_session(script, &mut rng);
        }

        tokio::spawn(async move {
            let ctx = loop_ctx;
            let mut rng = rng;
            let mut tick = interval(Duration::from_secs(1));
            let mut ticks: u64 = 0;
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        ticks += 1;
                        {
                            let mut sx = ctx.sessions.lock().unwrap();
                            for s in sx.iter_mut() {
                                if s.status == Status::Running {
                                    s.elapsed_ms += 1000;
                                    s.tokens += 220 + lcg(&mut rng) % 520;
                                }
                            }
                        }
                        ctx.process_steps(&mut rng);
                        if ticks.is_multiple_of(10) {
                            // periodic correction for panel-local timers
                            let sx = ctx.sessions.lock().unwrap();
                            for s in sx.iter() {
                                if s.status == Status::Running {
                                    ctx.send(&s.id, AgentEvent::Progress {
                                        elapsed_ms: s.elapsed_ms,
                                        tokens: s.tokens,
                                    });
                                }
                            }
                        }
                    }
                    Some((sid, control)) = controls.recv() => {
                        apply_control(&ctx, &sid, control);
                    }
                    else => break,
                }
            }
        });
    }
}

fn apply_control(ctx: &LoopCtx, sid: &str, control: Control) {
    let mut stopped = false;
    {
        let mut sx = ctx.sessions.lock().unwrap();
        let Some(s) = sx.iter_mut().find(|s| s.id == sid) else {
            return;
        };
        let resume = |s: &mut MockSession| {
            s.pending_tool = None;
            s.status = Status::Running;
        };
        match control {
            Control::Approve { .. } => {
                let tool = s.pending_tool.clone().unwrap_or_default();
                ctx.log(sid, "user", format!("OK approved {tool}"));
                resume(s);
            }
            Control::ApproveAlways { tool, .. } => {
                s.allowed.insert(tool.clone());
                ctx.log(
                    sid,
                    "user",
                    format!("OK {tool} added to always-allow for this session"),
                );
                resume(s);
            }
            Control::ApproveEdited { command, .. } => {
                ctx.log(sid, "user", "OK approved (edited):");
                ctx.log(sid, "user", command);
                resume(s);
            }
            Control::Deny { note, .. } => {
                let note = note
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| "Denied - take a different approach.".to_string());
                ctx.log(sid, "user", format!("X denied: {note}"));
                resume(s);
            }
            Control::Answer { text, .. } => {
                ctx.log(sid, "user", format!("OK your answer: {text}"));
                resume(s);
            }
            Control::Pause => {
                if s.status == Status::Running {
                    s.status = Status::Paused;
                    ctx.log(sid, "sys", "|| paused by you");
                }
            }
            Control::Resume => {
                if s.status != Status::Running {
                    if s.pending_tool.is_some() {
                        ctx.log(
                            sid,
                            "sys",
                            ">> resumed by you (still waiting on the same prompt)",
                        );
                    } else {
                        ctx.log(sid, "sys", ">> resumed by you");
                        s.status = Status::Running;
                    }
                }
            }
            Control::Stop => {
                ctx.log(sid, "sys", "Run stopped by user");
                ctx.send(
                    sid,
                    AgentEvent::Finished {
                        outcome: None,
                        files: Vec::new(),
                        stopped: true,
                    },
                );
                stopped = true;
            }
        }
        if stopped {
            sx.retain(|s| s.id != sid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity checks on the ported script data: every script is non-empty,
    /// gated in non-decreasing time order, and ends with a Finish step.
    #[test]
    fn scripts_are_well_formed() {
        let all = [
            script_auth(),
            script_flaky(),
            script_mdx(),
            script_audit(),
            script_index(),
            pool_orders(),
            pool_tokens(),
            pool_ci(),
            pool_docs(),
        ];
        assert_eq!(all.len(), 9);
        for script in &all {
            assert!(!script.steps.is_empty(), "{}: empty", script.title);
            assert!(
                script.steps.windows(2).all(|w| w[0].at_ms <= w[1].at_ms),
                "{}: steps out of order",
                script.title
            );
            assert!(
                matches!(
                    script.steps.last().map(|s| &s.kind),
                    Some(StepKind::Finish { .. })
                ),
                "{}: does not end with Finish",
                script.title
            );
        }
    }
}
