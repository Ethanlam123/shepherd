// Shepherd panel - render engine ported from the prototype (the design
// contract). The prototype's simulation engine is replaced by the Tauri
// bridge: state arrives via events, actions go back as controls.

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

/* ---------- types (wire format from shepherd-core, camelCase) ---------- */

type Status = 'running' | 'waiting' | 'paused';

interface PendingPermission {
  kind: 'permission';
  id: string;
  tool: string;
  command: string;
  reason: string;
}
interface PendingInput {
  kind: 'input';
  id: string;
  question: string;
  suggestions: string[];
}
type Pending = PendingPermission | PendingInput;

interface LogLine {
  ts: number;
  kind: string;
  text: string;
}

interface Session {
  id: string;
  agent: string;
  title: string;
  project: string;
  cwd: string;
  status: Status;
  startedAt: number;
  elapsedMs: number;
  tokens: number;
  allowedTools: string[];
  pending: Pending | null;
  activity: LogLine[];
}

/** Session plus panel-local UI state (composer/confirm mode). */
interface LocalSession extends Session {
  ui: null | 'edit' | 'deny' | 'confirm';
}

interface Run {
  id: string;
  agent: string;
  title: string;
  project: string;
  endedAt: number;
  durationMs: number;
  tokens: number;
  stopped: boolean;
  outcome: string;
  files: string[];
}

interface Snapshot {
  sessions: Session[];
  runs: Run[];
  muted: boolean;
}

type Control =
  | { type: 'approve'; pendingId: string }
  | { type: 'approve_always'; pendingId: string; tool: string }
  | { type: 'approve_edited'; pendingId: string; command: string }
  | { type: 'deny'; pendingId: string; note: string | null }
  | { type: 'answer'; pendingId: string; text: string }
  | { type: 'pause' }
  | { type: 'resume' }
  | { type: 'stop' };

/* ---------- agents ---------- */

const AGENTS: Record<string, { name: string; short: string; hue: string }> = {
  cc: { name: 'Claude Code', short: 'CC', hue: 'var(--cc)' },
  oc: { name: 'OpenCode', short: 'OC', hue: 'var(--oc)' },
  pi: { name: 'Pi', short: 'PI', hue: 'var(--pi)' },
  hm: { name: 'Hermes', short: 'HM', hue: 'var(--hm)' },
};

/* ---------- state ---------- */

const state = {
  sessions: new Map<string, LocalSession>(),
  runs: [] as Run[],
  activeTab: 'inbox' as 'inbox' | 'sessions' | 'runs',
  openSessionId: null as string | null,
  detailReturnScroll: 0,
  openRunId: null as string | null,
  muted: false,
  flashId: null as string | null,
  agentFilter: 'all',
};

/** Composer drafts keyed `${sessionId}:${mode}`, so typed text survives
 * re-renders and tab switches (acceptance criterion 7). */
const drafts = new Map<string, string>();

/* ---------- helpers ---------- */

const $ = (sel: string, root: ParentNode = document): HTMLElement =>
  root.querySelector(sel) as HTMLElement;

const esc = (s: unknown): string =>
  String(s).replace(/[&<>"']/g, (c) =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c] as string);

function fmtElapsed(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = s % 60;
  const pad = (n: number) => String(n).padStart(2, '0');
  return h > 0 ? h + ':' + pad(m) + ':' + pad(ss) : pad(m) + ':' + pad(ss);
}

function fmtTokens(n: number): string {
  return n >= 1000 ? (n / 1000).toFixed(1) + 'k' : String(n);
}

function fmtClock(ts: number): string {
  return new Date(ts).toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit', second: '2-digit' });
}

function fmtStarted(ts: number): string {
  return new Date(ts).toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit' });
}

function badgeHtml(agentId: string): string {
  const a = AGENTS[agentId];
  return '<span class="badge" style="--hue:' + a.hue + '" aria-hidden="true">' + a.short + '</span>';
}

function sendControl(sessionId: string, control: Control): void {
  invoke('send_control', { sessionId, control }).catch((e) => {
    console.error('control failed', sessionId, control, e);
  });
}

/* ---------- rendering ---------- */

function waitingCount(): number {
  let n = 0;
  for (const s of state.sessions.values()) if (s.status === 'waiting') n++;
  return n;
}

function renderTabs(): void {
  const counts = { inbox: waitingCount(), sessions: state.sessions.size, runs: state.runs.length };
  for (const t of ['inbox', 'sessions', 'runs'] as const) {
    const btn = $('#tab-' + t);
    btn.setAttribute('aria-selected', state.activeTab === t ? 'true' : 'false');
    btn.tabIndex = state.activeTab === t ? 0 : -1;
    const c = $('#c-' + t);
    c.textContent = String(counts[t]);
    c.classList.toggle('hot', t === 'inbox' && counts[t] > 0);
    if (state.activeTab === t) $('#view').setAttribute('aria-labelledby', 'tab-' + t);
  }
}

function renderSummary(): void {
  let running = 0;
  for (const s of state.sessions.values()) running++;
  const waiting = waitingCount();
  const el = $('#summaryLine');
  if (waiting > 0) {
    el.innerHTML =
      '<span class="sum-alert">' + waiting + (waiting === 1 ? ' agent is' : ' agents are') +
      ' waiting on you</span> - ' + running + ' session' + (running === 1 ? '' : 's') + ' open';
  } else {
    el.textContent = running + ' session' + (running === 1 ? '' : 's') + ' open - nothing needs you right now';
  }
}

function typingGuard(): boolean {
  const a = document.activeElement;
  return !!(a && $('#view').contains(a) && (a.tagName === 'TEXTAREA' || a.tagName === 'INPUT'));
}

function renderView(): void {
  // never re-render the view while the user is typing in a composer
  if (typingGuard()) return;
  const view = $('#view');

  if (state.activeTab === 'inbox') {
    view.innerHTML = htmlInbox();
    applyFlash(view);
    return;
  }
  if (state.activeTab === 'sessions') {
    if (state.openSessionId) {
      const s = state.sessions.get(state.openSessionId);
      if (s) {
        view.innerHTML = htmlDetail(s);
        renderLog();
        return;
      }
      state.openSessionId = null;
    }
    view.innerHTML = htmlSessions();
    return;
  }
  view.innerHTML = htmlRuns();
  applyFlash(view);
}

function render(): void {
  renderTabs();
  renderSummary();
  renderView();
}

function applyFlash(view: HTMLElement): void {
  if (!state.flashId) return;
  const card = view.querySelector('[data-sid="' + state.flashId + '"]');
  state.flashId = null;
  if (card) {
    card.classList.add('flash');
    card.scrollIntoView({ block: 'center' });
  }
}

function statusChip(s: LocalSession): string {
  if (s.status === 'waiting') return '<span class="chip-status st-waiting"><span class="dot"></span>Waiting</span>';
  if (s.status === 'paused') return '<span class="chip-status st-paused"><span class="dot"></span>Paused</span>';
  return '<span class="chip-status st-running"><span class="dot"></span>Running</span>';
}

/* ----- inbox ----- */

function htmlInbox(): string {
  const waiting = [...state.sessions.values()].filter((s) => s.status === 'waiting');
  if (waiting.length === 0) {
    return '<div class="empty">' +
      '<svg aria-hidden="true"><use href="#i-check-c"/></svg>' +
      '<p class="e-t">All quiet</p>' +
      '<p class="e-s">No agent is waiting on you. New approvals and questions land here the moment they come up.</p>' +
      '</div>';
  }
  return '<div class="stack">' +
    waiting.map((s) => (s.pending?.kind === 'permission' ? htmlPermCard(s) : htmlInputCard(s))).join('') +
    '</div>';
}

function cardHead(s: LocalSession, tagCls: string, tagText: string): string {
  const a = AGENTS[s.agent];
  return '<header class="od-row card-head">' +
    badgeHtml(s.agent) +
    '<span class="od-field od-fill">' +
    '<span class="c-title od-truncate">' + esc(s.title) + '</span>' +
    '<span class="c-sub od-truncate">' + esc(a.name + ' - ' + s.project) + '</span>' +
    '</span>' +
    '<span class="tag ' + tagCls + '">' + tagText + '</span>' +
    '</header>';
}

function htmlPermCard(s: LocalSession): string {
  const p = s.pending as PendingPermission;
  const body = '<p class="c-reason">' + esc(p.reason) + '</p>' +
    '<pre class="code">' + esc(p.command) + '</pre>' +
    '<p class="c-dir">' + esc(s.cwd) + '</p>';

  let footer: string;
  if (s.ui === 'edit') {
    const value = drafts.get(s.id + ':edit') ?? p.command;
    footer = '<div class="composer">' +
      '<label class="f-label" for="ta-' + s.id + '">Edit before approving</label>' +
      '<textarea id="ta-' + s.id + '" spellcheck="false">' + esc(value) + '</textarea>' +
      '<p class="f-hint" id="eh-' + s.id + '">The agent runs exactly this command.</p>' +
      '<div class="actions od-row">' +
      '<button class="btn btn-primary" data-act="approve-edited" data-sid="' + s.id + '">Approve edited</button>' +
      '<button class="btn btn-ghost" data-act="cancel" data-sid="' + s.id + '">Cancel</button>' +
      '</div></div>';
  } else if (s.ui === 'deny') {
    footer = '<div class="composer">' +
      '<label class="f-label" for="ta-' + s.id + '">Send the agent back with a note</label>' +
      '<div class="chips od-cluster">' +
      '<button class="chip" data-act="quick" data-ans="Denied - use a safer approach that does not touch the database." data-sid="' + s.id + '">Use a safer approach</button>' +
      '<button class="chip" data-act="quick" data-ans="Denied - skip this step and continue without it." data-sid="' + s.id + '">Skip this step</button>' +
      '<button class="chip" data-act="quick" data-ans="Denied - ask me again before anything destructive." data-sid="' + s.id + '">Ask me first next time</button>' +
      '</div>' +
      '<textarea id="ta-' + s.id + '" class="plain" placeholder="Optional note - empty sends a plain deny">' + esc(drafts.get(s.id + ':deny') ?? '') + '</textarea>' +
      '<div class="actions od-row">' +
      '<button class="btn btn-danger" data-act="send-deny" data-sid="' + s.id + '">Deny and send</button>' +
      '<button class="btn btn-ghost" data-act="cancel" data-sid="' + s.id + '">Cancel</button>' +
      '</div></div>';
  } else {
    footer = '<footer class="actions od-row">' +
      '<button class="btn btn-primary" data-act="approve" data-sid="' + s.id + '"><svg aria-hidden="true"><use href="#i-check-c"/></svg>Approve</button>' +
      '<button class="btn btn-ghost" data-act="always" data-sid="' + s.id + '">Always allow ' + esc(p.tool) + '</button>' +
      '<button class="btn btn-ghost" data-act="edit" data-sid="' + s.id + '"><svg aria-hidden="true"><use href="#i-pen"/></svg>Edit</button>' +
      '<button class="btn btn-danger-ghost" data-act="deny" data-sid="' + s.id + '">Deny</button>' +
      '</footer>';
  }

  return '<article class="card attention" data-sid="' + s.id + '">' + cardHead(s, 'tag-amber', 'Approval') + body + footer + '</article>';
}

function htmlInputCard(s: LocalSession): string {
  const q = (s.pending ?? { question: '', suggestions: [] }) as PendingInput;
  let footer: string;
  if (s.ui === 'deny') {
    footer = '<div class="composer">' +
      '<label class="f-label" for="ta-' + s.id + '">Your answer (required)</label>' +
      '<div class="chips od-cluster">' +
      q.suggestions.map((o) => '<button class="chip" data-act="quick" data-ans="' + esc(o) + '" data-sid="' + s.id + '">' + esc(o) + '</button>').join('') +
      '</div>' +
      '<textarea id="ta-' + s.id + '" class="plain" placeholder="Type your answer or pick a suggestion above">' + esc(drafts.get(s.id + ':deny') ?? '') + '</textarea>' +
      '<div class="actions od-row">' +
      '<button class="btn btn-primary" data-act="answer" data-sid="' + s.id + '">Send answer</button>' +
      '<button class="btn btn-ghost" data-act="cancel" data-sid="' + s.id + '">Cancel</button>' +
      '</div></div>';
  } else {
    footer = '<div class="composer">' +
      '<span class="f-label">Suggested answers</span>' +
      '<div class="chips od-cluster">' +
      q.suggestions.map((o) => '<button class="chip" data-act="answer-chip" data-ans="' + esc(o) + '" data-sid="' + s.id + '">' + esc(o) + '</button>').join('') +
      '</div>' +
      '<button class="btn btn-ghost" data-act="deny" data-sid="' + s.id + '">Type a custom answer</button>' +
      '</div>';
  }

  return '<article class="card attention" data-sid="' + s.id + '">' + cardHead(s, 'tag-blue', 'Question') +
    '<p class="c-reason">' + esc(q.question) + '</p>' + footer + '</article>';
}

/* ----- sessions ----- */

function htmlSessions(): string {
  const agentsPresent = Object.keys(AGENTS).filter((k) => {
    for (const s of state.sessions.values()) if (s.agent === k) return true;
    return false;
  });
  const chips = ['<button class="fchip" data-act="filter" data-agent="all" aria-pressed="' + (state.agentFilter === 'all') + '">All</button>']
    .concat(agentsPresent.map((k) =>
      '<button class="fchip" data-act="filter" data-agent="' + k + '" style="--hue:' + AGENTS[k].hue + '" aria-pressed="' + (state.agentFilter === k) + '"><span class="dot" aria-hidden="true"></span>' + esc(AGENTS[k].name) + '</button>'
    )).join('');

  const list = [...state.sessions.values()].filter((s) => state.agentFilter === 'all' || s.agent === state.agentFilter);

  if (list.length === 0) {
    return '<div class="chip-row od-cluster">' + chips + '</div>' +
      '<div class="empty"><p class="e-t">No ' + (state.agentFilter === 'all' ? 'open' : esc(AGENTS[state.agentFilter].name)) + ' sessions</p>' +
      '<p class="e-s">New sessions appear here as agents start working.</p></div>';
  }

  const rows = list.map((s) => {
    const ctl = s.ui === 'confirm'
      ? '<div class="confirm">' +
        '<span class="c-q">Stop this run? Work done so far stays on disk.</span>' +
        '<button class="btn btn-ghost" data-act="cancel" data-sid="' + s.id + '">Cancel</button>' +
        '<button class="btn btn-danger" data-act="stop-yes" data-sid="' + s.id + '">Stop run</button>' +
        '</div>'
      : '<div class="s-ctl">' +
        (s.status === 'running'
          ? '<button class="icon-btn" data-act="pause" data-sid="' + s.id + '" aria-label="Pause ' + esc(s.title) + '"><svg aria-hidden="true"><use href="#i-pause"/></svg></button>'
          : (s.status === 'paused' || s.status === 'waiting'
            ? '<button class="icon-btn" data-act="resume" data-sid="' + s.id + '" aria-label="Resume ' + esc(s.title) + '"><svg aria-hidden="true"><use href="#i-play"/></svg></button>'
            : '')) +
        '<button class="icon-btn danger" data-act="stop" data-sid="' + s.id + '" aria-label="Stop ' + esc(s.title) + '"><svg aria-hidden="true"><use href="#i-stop"/></svg></button>' +
        '</div>';

    return '<article class="s-row" data-sid="' + s.id + '">' +
      '<button class="s-main" data-act="open" data-sid="' + s.id + '">' +
      badgeHtml(s.agent) +
      '<span class="od-field od-fill">' +
      '<span class="c-title od-truncate">' + esc(s.title) + '</span>' +
      '<span class="c-sub od-truncate">' + esc(AGENTS[s.agent].name + ' - ' + s.project + ' - started ' + fmtStarted(s.startedAt)) + '</span>' +
      '</span>' +
      '<span class="s-side">' +
      statusChip(s) +
      '<span class="elapsed od-nowrap" data-elapsed="' + s.id + '">' + fmtElapsed(s.elapsedMs) + '</span>' +
      '</span>' +
      '</button>' +
      ctl +
      '</article>';
  }).join('');

  return '<div class="chip-row od-cluster">' + chips + '</div><div class="s-list">' + rows + '</div>';
}

/* ----- session detail ----- */

function htmlDetail(s: LocalSession): string {
  const isRunning = s.status === 'running';
  const ctl = s.ui === 'confirm'
    ? '<div class="confirm" style="padding-left:12px">' +
      '<span class="c-q">Stop this run? Work done so far stays on disk.</span>' +
      '<button class="btn btn-ghost" data-act="cancel" data-sid="' + s.id + '">Cancel</button>' +
      '<button class="btn btn-danger" data-act="stop-yes" data-sid="' + s.id + '">Stop run</button>' +
      '</div>'
    : '<div class="actions od-row" style="margin-top:0">' +
      (isRunning
        ? '<button class="btn btn-ghost" data-act="pause" data-sid="' + s.id + '"><svg aria-hidden="true"><use href="#i-pause"/></svg>Pause</button>'
        : '<button class="btn btn-ghost" data-act="resume" data-sid="' + s.id + '"><svg aria-hidden="true"><use href="#i-play"/></svg>Resume</button>') +
      '<button class="btn btn-danger-ghost" data-act="stop" data-sid="' + s.id + '"><svg aria-hidden="true"><use href="#i-stop"/></svg>Stop</button>' +
      '</div>';

  return '<article class="detail" data-sid="' + s.id + '">' +
    '<header class="od-row detail-head">' +
    '<button class="icon-btn" data-act="back" aria-label="Back to sessions"><svg aria-hidden="true"><use href="#i-back"/></svg></button>' +
    badgeHtml(s.agent) +
    '<span class="od-field od-fill">' +
    '<span class="c-title">' + esc(s.title) + '</span>' +
    '<span class="c-sub">' + esc(AGENTS[s.agent].name + ' - ' + s.cwd) + '</span>' +
    '</span>' +
    '<span class="s-side">' + statusChip(s) +
    '<span class="elapsed od-nowrap" data-elapsed="' + s.id + '">' + fmtElapsed(s.elapsedMs) + '</span>' +
    '</span>' +
    '</header>' +
    '<div class="d-stats od-row">' +
    '<span class="od-stat"><span class="d-stat-val" data-tokens="' + s.id + '">' + fmtTokens(s.tokens) + '</span><span class="d-stat-cap">tokens</span></span>' +
    '<span class="od-stat"><span class="d-stat-val">' + esc(s.allowedTools.length) + '</span><span class="d-stat-cap">tools always allowed</span></span>' +
    '<span class="od-stat"><span class="d-stat-val">' + esc(s.activity.length) + '</span><span class="d-stat-cap">events</span></span>' +
    '</div>' +
    ctl +
    '<h2 class="sec-t">Activity</h2>' +
    '<div id="log" class="log" aria-label="Live activity"></div>' +
    '</article>';
}

function renderLog(): void {
  const s = state.openSessionId ? state.sessions.get(state.openSessionId) : null;
  const box = $('#log');
  if (!s || !box) return;
  const nearBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 48;
  box.innerHTML = s.activity.slice(-60).map((l) => {
    const cls = l.kind === 'ok' ? 'ln-ok' : l.kind === 'warn' ? 'ln-warn' : l.kind === 'user' ? 'ln-user' : l.kind === 'sys' ? 'ln-sys' : '';
    return '<div class="ln ' + cls + '"><span class="t">' + esc(fmtClock(l.ts)) + '</span><span class="tx">' + esc(l.text) + '</span></div>';
  }).join('');
  if (nearBottom) box.scrollTop = box.scrollHeight;
}

/* ----- runs ----- */

function htmlRuns(): string {
  if (state.runs.length === 0) {
    return '<div class="empty"><p class="e-t">No finished runs yet</p><p class="e-s">Completed sessions collect here with a summary of what changed.</p></div>';
  }
  return '<div class="stack">' + state.runs.map((r) => {
    const open = state.openRunId === r.id;
    const ago = Math.max(0, Math.round((Date.now() - r.endedAt) / 60000));
    const agoTxt = ago < 1 ? 'just now' : ago + 'm ago';
    const head =
      '<button class="run-head" data-act="toggle-run" data-rid="' + r.id + '" aria-expanded="' + open + '">' +
      badgeHtml(r.agent) +
      '<span class="od-field od-fill">' +
      '<span class="c-title od-truncate">' + esc(r.title) + '</span>' +
      '<span class="c-sub od-truncate">' + esc(AGENTS[r.agent].name + ' - ' + (r.stopped ? 'stopped ' : 'finished ') + agoTxt) + '</span>' +
      '</span>' +
      '<span class="chev" aria-hidden="true"><svg><use href="#i-chev"/></svg></span>' +
      '</button>';
    if (!open) return '<article class="run" data-rid="' + r.id + '">' + head + '</article>';
    const body =
      '<div class="run-body">' +
      '<div class="run-meta od-cluster">' +
      '<span class="meta-chip">' + fmtElapsed(r.durationMs) + '</span>' +
      '<span class="meta-chip">' + fmtTokens(r.tokens) + ' tokens</span>' +
      (r.stopped ? '<span class="meta-chip" style="color:var(--danger)">stopped manually</span>' : '') +
      '</div>' +
      '<p class="run-sum">' + esc(r.outcome) + '</p>' +
      (r.files.length ? '<h3 class="sec-t" style="margin-top:2px">Files touched</h3><ul class="files">' + r.files.map((f) => '<li>' + esc(f) + '</li>').join('') + '</ul>' : '') +
      '</div>';
    return '<article class="run" data-rid="' + r.id + '">' + head + body + '</article>';
  }).join('') + '</div>';
}

/* ---------- events from core ---------- */

function upsertSession(s: Session): void {
  const existing = state.sessions.get(s.id);
  state.sessions.set(s.id, { ...s, ui: existing?.ui ?? null });
  render();
}

async function wireEvents(): Promise<void> {
  await listen<Session>('session', (e) => upsertSession(e.payload));
  await listen<{ sessionId: string; line: LogLine }>('activity', (e) => {
    const s = state.sessions.get(e.payload.sessionId);
    if (!s) return;
    s.activity.push(e.payload.line);
    if (s.activity.length > 120) s.activity.shift();
    if (state.openSessionId === s.id && state.activeTab === 'sessions') renderLog();
  });
  await listen<{ sessionId: string }>('removed', (e) => {
    state.sessions.delete(e.payload.sessionId);
    drafts.delete(e.payload.sessionId + ':edit');
    drafts.delete(e.payload.sessionId + ':deny');
    if (state.openSessionId === e.payload.sessionId) state.openSessionId = null;
    render();
  });
  await listen<Run>('run', (e) => {
    state.runs.unshift(e.payload);
    render();
  });
}

/* ---------- local elapsed ticker ---------- */

function updateElapsed(): void {
  for (const s of state.sessions.values()) {
    const el = document.querySelector('[data-elapsed="' + s.id + '"]');
    if (el) el.textContent = fmtElapsed(s.elapsedMs);
    const tk = document.querySelector('[data-tokens="' + s.id + '"]');
    if (tk) tk.textContent = fmtTokens(s.tokens);
  }
}

setInterval(() => {
  for (const s of state.sessions.values()) {
    if (s.status === 'running') s.elapsedMs += 1000;
  }
  updateElapsed();
}, 1000);

/* ---------- actions ---------- */

function findSession(id: string | undefined): LocalSession | undefined {
  return id ? state.sessions.get(id) : undefined;
}

document.addEventListener('click', (e) => {
  const el = (e.target as HTMLElement).closest<HTMLElement>('[data-act]');
  if (!el) return;
  const act = el.dataset.act;
  const sid = el.dataset.sid;
  const s = findSession(sid);

  switch (act) {
    case 'approve':
      if (s?.pending) {
        sendControl(s.id, { type: 'approve', pendingId: s.pending.id });
        s.ui = null;
        render();
      }
      break;
    case 'approve-edited': {
      if (!s?.pending) break;
      const ta = document.querySelector<HTMLTextAreaElement>('[data-sid="' + s.id + '"] textarea');
      const val = (ta?.value ?? '').trim();
      if (!val) {
        const h = $('#eh-' + s.id);
        if (h) {
          h.textContent = 'The command cannot be empty - edit it or approve the original.';
          h.className = 'f-error';
        }
        return;
      }
      sendControl(s.id, { type: 'approve_edited', pendingId: s.pending.id, command: val });
      drafts.delete(s.id + ':edit');
      s.ui = null;
      render();
      break;
    }
    case 'always':
      if (s?.pending?.kind === 'permission') {
        sendControl(s.id, { type: 'approve_always', pendingId: s.pending.id, tool: s.pending.tool });
        s.ui = null;
        render();
      }
      break;
    case 'edit':
      if (s) {
        s.ui = 'edit';
        render();
        focusTa(s.id);
      }
      break;
    case 'deny':
      if (s) {
        s.ui = 'deny';
        render();
        focusTa(s.id);
      }
      break;
    case 'cancel':
      if (s) {
        s.ui = null;
        render();
      }
      break;
    case 'quick': {
      if (!s) break;
      const ta = document.querySelector<HTMLTextAreaElement>('[data-sid="' + s.id + '"] textarea');
      if (ta) ta.value = el.dataset.ans ?? '';
      drafts.set(s.id + ':' + (s.ui === 'edit' ? 'edit' : 'deny'), el.dataset.ans ?? '');
      break;
    }
    case 'send-deny': {
      if (!s?.pending) break;
      const ta = document.querySelector<HTMLTextAreaElement>('[data-sid="' + s.id + '"] textarea');
      const note = ta && ta.value.trim() ? ta.value.trim() : null;
      sendControl(s.id, { type: 'deny', pendingId: s.pending.id, note });
      drafts.delete(s.id + ':deny');
      s.ui = null;
      render();
      break;
    }
    case 'answer': {
      if (!s?.pending) break;
      const ta = document.querySelector<HTMLTextAreaElement>('[data-sid="' + s.id + '"] textarea');
      const val = ta ? ta.value.trim() : '';
      if (!val) {
        ta?.focus();
        return;
      }
      sendControl(s.id, { type: 'answer', pendingId: s.pending.id, text: val });
      drafts.delete(s.id + ':deny');
      s.ui = null;
      render();
      break;
    }
    case 'answer-chip':
      if (s?.pending) {
        sendControl(s.id, { type: 'answer', pendingId: s.pending.id, text: el.dataset.ans ?? '' });
        s.ui = null;
        render();
      }
      break;
    case 'pause':
      if (s) sendControl(s.id, { type: 'pause' });
      break;
    case 'resume':
      if (s) sendControl(s.id, { type: 'resume' });
      break;
    case 'stop':
      if (s) {
        s.ui = 'confirm';
        render();
      }
      break;
    case 'stop-yes':
      if (s) {
        sendControl(s.id, { type: 'stop' });
        s.ui = null;
        render();
      }
      break;
    case 'open': {
      if (!s) break;
      state.detailReturnScroll = $('#view').scrollTop;
      state.openSessionId = s.id;
      render();
      $('#view').scrollTop = 0;
      break;
    }
    case 'back': {
      state.openSessionId = null;
      render();
      $('#view').scrollTop = state.detailReturnScroll;
      break;
    }
    case 'filter':
      state.agentFilter = el.dataset.agent ?? 'all';
      render();
      break;
    case 'toggle-run':
      state.openRunId = state.openRunId === el.dataset.rid ? null : el.dataset.rid ?? null;
      render();
      break;
  }
});

// keep drafts current as the user types
document.addEventListener('input', (e) => {
  const ta = e.target as HTMLTextAreaElement;
  if (ta.tagName !== 'TEXTAREA') return;
  const card = ta.closest<HTMLElement>('[data-sid]');
  if (!card) return;
  const mode = card.querySelector('.f-label')?.textContent?.includes('Edit before approving') ? 'edit' : 'deny';
  drafts.set(card.dataset.sid + ':' + mode, ta.value);
});

function focusTa(sid: string): void {
  requestAnimationFrame(() => {
    const ta = document.querySelector<HTMLTextAreaElement>('[data-sid="' + sid + '"] textarea');
    if (ta) {
      ta.focus();
      ta.setSelectionRange(ta.value.length, ta.value.length);
    }
  });
}

/* ---------- tabs ---------- */

for (const t of ['inbox', 'sessions', 'runs'] as const) {
  $('#tab-' + t).addEventListener('click', () => {
    if (t !== 'sessions') state.openSessionId = null;
    state.activeTab = t;
    render();
    $('#view').scrollTop = 0;
  });
}

document.querySelector('.tabs')!.addEventListener('keydown', (e) => {
  if ((e as KeyboardEvent).key !== 'ArrowRight' && (e as KeyboardEvent).key !== 'ArrowLeft') return;
  e.preventDefault();
  const order = ['inbox', 'sessions', 'runs'] as const;
  const idx = order.indexOf(state.activeTab);
  const next = order[(idx + ((e as KeyboardEvent).key === 'ArrowRight' ? 1 : order.length - 1)) % order.length];
  state.activeTab = next;
  render();
  $('#tab-' + next).focus();
});

/* ---------- mute ---------- */

function syncMuteBtn(): void {
  const btn = $('#muteBtn');
  btn.setAttribute('aria-pressed', state.muted ? 'true' : 'false');
  btn.setAttribute('aria-label', state.muted ? 'Unmute notifications' : 'Mute notifications');
  btn.querySelector('use')!.setAttribute('href', state.muted ? '#i-bell-off' : '#i-bell');
}

$('#muteBtn').addEventListener('click', () => {
  state.muted = !state.muted;
  syncMuteBtn();
  invoke('set_muted', { muted: state.muted }).catch((e) => console.error('set_muted failed', e));
});

/* ---------- escape: close composers, confirms, then detail - never the panel ---------- */

document.addEventListener('keydown', (e) => {
  if ((e as KeyboardEvent).key !== 'Escape') return;
  for (const s of state.sessions.values()) {
    if (s.ui) {
      s.ui = null;
      render();
      return;
    }
  }
  if (state.openSessionId) {
    state.openSessionId = null;
    render();
    $('#view').scrollTop = state.detailReturnScroll;
  }
});

/* ---------- focus keyboard nav when the panel opens ---------- */

window.addEventListener('focus', () => {
  if (!typingGuard() && document.activeElement === document.body) {
    ($('#tab-' + state.activeTab) as HTMLElement).focus();
  }
});

/* ---------- boot ---------- */

async function boot(): Promise<void> {
  // listeners first so no event lands between snapshot and wiring
  await wireEvents();
  const snap = await invoke<Snapshot>('get_state');
  for (const s of snap.sessions) state.sessions.set(s.id, { ...s, ui: null });
  state.runs = snap.runs;
  state.muted = snap.muted;
  syncMuteBtn();
  render();
}

boot().catch((e) => {
  console.error('panel boot failed', e);
  const view = $('#view');
  view.innerHTML = '<div class="empty"><p class="e-t">Shepherd could not start</p>' +
    '<p class="e-s">' + esc(String(e)) + '</p></div>';
});
