# Building, developing and releasing HQ

For people working on HQ itself. To just use it, see the [README](../README.md).

- **Rust + Tauri 2.** The app uses the WebView2 runtime that ships with Windows. The release
  binary is about 4.3 MB (the installer 1.5 MB). The shell (without Claude) uses about 180 MB
  of private memory. Each session's `claude` process uses its own memory on top of that.
- **Thin by design.** Each cell runs the installed `claude` CLI with the documented stream-json
  protocol. The Agent SDKs and the VS Code extension use the same protocol. The Rust side
  forwards raw JSON lines and never interprets them. The UI ignores message types it does not
  know, so a Claude Code update hides a new feature instead of breaking the app. HQ adds no
  hooks and never writes Claude's settings files itself. The exceptions are the MCP
  menu (turning a server on or off asks Claude to do it, and Claude saves the choice for that
  folder, just as `/mcp` does in the CLI) and [HQ tools everywhere](REFERENCE.md#hq-tools-mcp), which runs
  `claude mcp add` when you ask it to.

## Build and run

Prerequisites: the Rust toolchain (MSVC). HQ installs Claude Code itself on first run if it is missing.

```bat
build.cmd            :: debug build   -> target\debug\hq.exe
build.cmd --release  :: release build -> target\release\hq.exe
```

`build.cmd` runs `cargo build` inside the VS 2022 Build Tools environment. The newest Visual
Studio on this machine does not include the desktop x64 CRT libraries (`msvcrt.lib`), and
rustc would otherwise pick it.

### One copy per profile

Only one HQ runs per profile. Starting it again brings the open window to the front. Each
profile has its own board, WebView2 data folder and lock, so copies in different profiles
never touch each other's sessions.

- The installed app uses the default profile.
- Any other copy (a build under `target\`) uses the `dev` profile. Its window title and top
  bar say so.
- `HQ_PROFILE=<name>` picks another profile, for example to run a second test copy.
  `HQ_PROFILE=default` makes a build share the installed app's board.

### Local development

```bat
dev.cmd   :: stops the running dev copy, builds a debug build and starts it
```

The dev copy runs next to the installed HQ. A debug build also:

- Reads `ui\` from disk, so Ctrl+R shows a UI edit (F5 is turned off) without a rebuild. A reload stops
  every `claude` in the dev copy; the cells come back from its board. Rust changes still need
  `dev.cmd`.
- Opens DevTools on port 9223 (`HQ_DEBUG_PORT` changes it), for CDP tools such as the Chrome
  DevTools MCP server. Right-click > Inspect works too.

`cargo test` runs the backend's unit tests (stream filtering, batching, transcript reading,
session search, token usage and prices). `node --test tests/*.test.cjs` runs the setup and usage-chart
regressions; both suites and a build run on Windows and macOS for each PR. On Windows, use the
Build Tools environment selected by `build.cmd` for Cargo commands. With `codex` on PATH,
`cargo test codex::live -- --ignored --test-threads=1` drives a real Codex through HQ's bridge
against a fake model server, so it needs no account.

## Releases and updates

An installed copy of HQ updates itself:

- It checks for a new release 20 seconds after it starts, then every 4 hours.
- It downloads the new installer in the background and checks its signature.
- It installs when nothing would be lost: no draft or unseen reply is left, and there has been
  no mouse or key input for 2 minutes (15 while HQ is the active window). HQ then restarts
  with the same board.
- Claude Code and Hermes sessions keep running through the restart: turns in flight, background
  shells and servers, and HQ's MCP tools all carry on, and each cell reconnects to its session
  when HQ is back. They run in the **session host**, a small background copy of HQ
  (`~/.hq/bin/hq-session-host-*`) that the installer does not stop. It waits up to 30 minutes
  for HQ to come back after an update and 15 after a crash, then ends its sessions. Quitting HQ
  ends them at once. Its log is in `~/.hq/hosts/host.log`; set `HQ_NO_HOST=1` to run sessions
  inside HQ as before. Codex and cloud sessions still stop, so HQ waits while one of those is
  working, and resumes it when it next gets a message.
- Until then, an **Update** button in the top bar installs it at once. If you close HQ
  first, the update installs as HQ closes.

Only a copy installed with the setup program updates itself. A build under `target\` never
does. Set `HQ_NO_UPDATE=1` to turn updates off.

To publish a release, commit your changes and run:

```powershell
.\release.ps1 0.2.1 -Notes "- What changed, for the people who install HQ"
```

Release notes are required: neither the script nor the workflow publishes without them.

The script does these steps:

1. Sets the version.
2. Builds the NSIS installer in `target\dist` with the Tauri CLI (through `npx`, in the same
   VS 2022 environment as `build.cmd`).
3. Signs the installer with `%USERPROFILE%\.tauri\hq-updater.key`.
4. Commits, tags and pushes.
5. Publishes the installer, its signature and `latest.json` as a GitHub release in `HQ-releases`,
   then the same release, files and notes in this repo, so both repos list the same releases.

Or release from GitHub without a Windows PC: **Actions > Release HQ > Run workflow** on `main`,
with the version and the release notes in Markdown. It builds Windows and the Mac app, and only once both succeed
commits the version, tags, pushes and publishes the same files and `latest.json` as
`release.ps1 -MacArtifacts`. Besides the two signing-key secrets below it needs
`RELEASES_TOKEN`, a fine-grained token with Contents read and write on `HQ-releases`.

Installed copies read `latest.json` from the newest release at the endpoint in
`tauri.conf.json`. **Back up the private key.** Without it, no new release can be signed, and
every installed copy must be reinstalled by hand.

## macOS builds

Build on a Mac with `./build-mac.sh` (universal build, output under
`target/universal-apple-darwin/release/bundle`) and run a dev copy with `./dev-mac.sh`. The
`Build HQ for macOS` GitHub workflow runs the same build; publish its output with
`.\release.ps1 <version> -MacArtifacts <folder>` (see the script's header). The workflow needs
the repository secrets `TAURI_SIGNING_PRIVATE_KEY` (the contents of the updater key) and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.

## Environment overrides

- `HQ_CLAUDE`: path to the `claude` binary. The **Claude command** setting overrides it.
- `HQ_MODEL`: default model for HQ's Claude sessions that have no model picked.
- `HQ_CODEX`: path to the `codex` binary. The **Codex command** setting overrides it.
- `HQ_HERMES`: path to the `hermes` binary (default: `hermes` on PATH).
- `HQ_NO_UPDATE`: never check for updates.
- `HQ_UPDATE_ANY_COPY`: check for updates even in a copy that was not installed (for testing).
- `HQ_PROFILE`: run as a named profile (see [One copy per profile](#one-copy-per-profile)).
- `HQ_DEBUG_PORT`: DevTools port for a debug build (default 9223).
- `HQ_STATE_DIR`: folder for `board.json`, overriding the profile's own.

## Layout

```
src/session.rs   spawns one `claude -p --input-format stream-json ...` per cell; a writer thread
                 feeds its stdin in order and a batcher sends its stdout to the UI as arrays
                 (dropping streaming lines the UI never uses); Windows job object kills the
                 whole process tree when a cell (or HQ) closes; usage probe
src/store.rs     board.json persistence, transcript loading (newest part, backwards), cached
                 session list, search inside transcripts, VS Code and workspace lookup
src/codex.rs     Codex cells: runs `codex app-server` and translates it to and from the stream-json
                 lines the UI speaks; a shared helper reads history, sessions and models; runs
                 `codex update`. Built only with the `codex` Cargo feature (off by default)
src/hermes.rs    Hermes cells: runs `hermes acp` and translates ACP to and from the stream-json
                 lines the UI speaks; HQ's copy of each Hermes conversation. Built with the
                 `hermes` Cargo feature (on by default; `--no-default-features` leaves it out)
src/update.rs    self-update: checks and downloads signed releases; installs when told to
src/main.rs      Tauri commands, profiles, single-instance lock
src/dev.rs       debug builds only: UI read from disk, DevTools port (Windows)
src/macos.rs     macOS only: login-shell PATH, Terminal and VS Code launchers, installed-app check
ui/app.js        cells, protocol handling, prompts, menus, sessions list, grid layout, persistence,
                 context indicator, Remote Control, settings
ui/codex.js      what Codex cells do differently: their model list, commands, defaults, modes,
                 keeping Codex up to date
ui/hermes.js     what Hermes cells do differently: their model list, commands, defaults, modes
ui/afk.js        AFK mode: tells sessions you are away, answers or defers their prompts, nudges
                 them on and lists what it did once you are back
ui/render.js     markdown (raw HTML escaped, http(s) links only; incremental while streaming),
                 tool rows, line diffs, images, project colors
ui/style.css     slate and pastel look, and a light theme
release.ps1      builds, signs and publishes a release
.github/workflows/release.yml  the same release, built and published on GitHub
dev.cmd          rebuilds and restarts the dev copy
```

