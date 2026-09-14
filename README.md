# Shepherd

A macOS menu-bar app that watches local AI coding agents and brings their
questions to you. Shepherd lives in the tray, streams what your agents are
doing, and turns their "can I run this?" prompts into approval cards - so
a fleet of terminal sessions stops meaning a fleet of tabs to babysit.

V1 supports Claude Code. More agents (OpenCode, Pi, Hermes) are wired in
the panel as mock identities until their integrations land.

## What you get

- **Inbox** - permission cards ("Agent wants to run `pnpm prisma migrate
  reset`") with Approve / Always allow / Edit-before-approve / Deny-with-note,
  and nudge cards when an agent is idle-waiting for input.
- **Sessions** - every live agent session with status, elapsed time, token
  count, and a live activity log (tool calls, results, your decisions).
- **Runs** - a per-turn history: each finished turn records its final
  assistant message, duration, tokens, and files touched. Runs survive
  restarts (SQLite).
- **Tray badge + notifications** - the crook icon grows an amber count when
  an agent is waiting; a macOS notification fires once per wait (mutable).
- **Start at login** - one toggle (SMAppService), no helper processes.

Everything runs locally. No telemetry, no accounts, no network calls except
the update check you trigger yourself.

## How it works

```
Claude Code terminal session
  |-- ~/.claude/sessions/<pid>.json          (discovery, 2s)
  |-- ~/.claude/projects/<cwd>/<sid>.jsonl   (transcript tail, 1s)
  '-- shepherd-hook  <---- hooks in ~/.claude/settings.json
         |
         v  unix socket (0700)
   Shepherd.app  (tray + 400pt panel)
         |-- registry   sessions, runs, badge, persistence
         '-- SQLite     ~/Library/Application Support/Shepherd/shepherd.db
```

- **shepherd-hook** (separate small binary) is installed into
  `~/.claude/settings.json` for `PreToolUse`, `Notification`, and `Stop`.
  It asks Shepherd over a user-only socket and translates your decision
  back into Claude Code's hook protocol.
- **Fail-open everywhere**: if Shepherd is not running, hangs, or the
  socket is gone, the hook exits 0 and Claude Code proceeds with its
  normal permission flow. Install/uninstall merges the settings file and
  never touches your other hooks.
- Discovery and transcripts read Claude Code's on-disk state (session
  files and JSONL transcripts). Those formats are unofficial and were
  verified against Claude Code 2.1.270; a Claude Code update can break
  them, in which case the panel simply goes quiet - it never interferes
  with your terminals.
- Terminal Claude Code sessions cannot be paused or stopped from outside
  the terminal (no such API exists today), so those controls render
  disabled for cc with a pointer back to the terminal. Answers to
  idle-input nudges are given in the terminal too.

## Install

Grab the latest dmg from [releases][releases], drag Shepherd.app to
/Applications, then - because the app is ad-hoc signed and not notarized -
right-click Shepherd.app and choose **Open**, then **Open** in the dialog
(first launch only). Or: `xattr -d com.apple.quarantine /Applications/Shepherd.app`.

Then click the plug toggle in the panel header to connect Claude Code
hooks. Open a `claude` session in any terminal and it appears in the panel
within a couple of seconds.

[releases]: https://github.com/Ethanlam123/shepherd/releases

## Development

```sh
npm install
npm run sidecar                     # build the shepherd-hook binary tauri-build expects
npm run dev                         # tray + panel; hooks toggle installs into ~/.claude/settings.json
SHEPHERD_MOCK=1 npm run dev         # plus demo agents exercising the full controls
```

`npm run sidecar` (also run by dev/build via beforeDevCommand) places the
shepherd-hook sidecar where `externalBin` expects it. Bare `cargo` commands
that build `shepherd-app` need it to exist first.

Workspace layout:

| Path | What |
| --- | --- |
| `crates/shepherd-core` | registry, adapters (cc + mocks), hook socket server, SQLite store, settings installer |
| `crates/shepherd-hook` | the Claude Code hook binary (fail-open, one process per hook event) |
| `src-tauri` | app shell: tray, panel window, IPC, notifications, login item, updater |
| `ui` | the panel (vanilla TypeScript + CSS, no framework) |
| `docs/shepherd-hooks-settings.json` | the exact settings contract we install |

Checks: `cargo clippy --workspace --tests -- -D warnings && cargo test` and
`npm run check`. Warnings and errors are logged to
`~/Library/Logs/Shepherd/shepherd.log`.

## Building a release

One-time signing key for the updater (minisign; keep the private key secret,
it is not committed):

```sh
npx tauri signer generate -w ~/.tauri/shepherd.key
```

The matching public key is already embedded in `src-tauri/tauri.conf.json`.

Then build a universal (arm64 + x86_64), ad-hoc signed app and dmg:

```sh
TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/shepherd.key)" npm run release
```

Artifacts land in `src-tauri/target/universal-apple-darwin/release/bundle/`:

- `macos/Shepherd.app` - the app; drag into /Applications
- `dmg/Shepherd_0.1.0_universal.dmg`
- `macos/Shepherd.app.tar.gz` + `.sig` - minisign-signed updater archive

## Updates (minisign)

The panel's refresh button checks the release endpoint and installs signed
updates. To publish: create a GitHub release with the version's
`Shepherd.app.tar.gz`, `.sig`, and a `latest.json` describing it, and the
in-app check will find it.

## Status and roadmap

M1-M5 are done (panel, Claude Code integration, hooks round trip, per-turn
runs + notifications, packaging). Known issues and planned hardening are
tracked in the [code-review issues][review].

[review]: https://github.com/Ethanlam123/shepherd/issues?q=label%3Acode-review

## Limitations

- v1 tracks interactive terminal Claude Code sessions; headless `claude -p`
  sessions are not discovered.
- Session discovery and transcripts depend on unofficial on-disk formats
  (verified against Claude Code 2.1.270).
- Edit-before-approve relies on the PreToolUse `updatedInput` hook field,
  which has had regressions in some Claude Code versions.
- No Apple Developer certificate: ad-hoc signing, no notarization, and the
  Gatekeeper right-click step above.
