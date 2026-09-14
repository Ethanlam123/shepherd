# Shepherd

A macOS menu-bar app that watches and controls local AI coding agents.
V1 supports Claude Code terminal sessions: live activity, permission
approval cards, per-turn run history, and macOS notifications when an agent
is waiting on you.

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

Checks: `cargo clippy --workspace --tests -- -D warnings && cargo test` and
`npm run check`.

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
updates. Before your first published release, replace `OWNER` in
`tauri.conf.json` (`plugins.updater.endpoints`) with the GitHub owner of the
release repo. To publish: create a GitHub release with the version's
`Shepherd.app.tar.gz`, `.sig`, and a `latest.json` describing it, and the
in-app check will find it.

## Install notes (no Apple Developer certificate)

Shepherd ships ad-hoc signed and not notarized, so Gatekeeper blocks a plain
double-click open. The first time:

1. Move Shepherd.app to /Applications.
2. Right-click Shepherd.app and choose Open, then Open in the dialog.
   (Or `xattr -d com.apple.quarantine /Applications/Shepherd.app`.)

- Start at login uses SMAppService (macOS 13+): the power toggle in the
  panel header registers Shepherd as a login item. It requires the release
  app bundle, so the toggle shows an error when running from `npm run dev`.
- Terminal Claude Code sessions cannot be paused or stopped from outside the
  terminal; those controls render disabled by design.
