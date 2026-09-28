# HQ

A small desktop app that runs many Claude Code chat sessions side by side, each in its own
folder, on any drive. It is inspired by HNA-Code, with these differences: the board starts
empty, you add sessions one at a time, the grid lays itself out, and each cell is a chat view
styled like the VS Code extension instead of a terminal.

- **Rust + Tauri 2.** The app uses the WebView2 runtime that ships with Windows. The release
  binary is about 4.3 MB (the installer 1.5 MB). The shell (without Claude) uses about 180 MB
  of private memory. Each session's `claude` process uses its own memory on top of that.
- **Thin by design.** Each cell runs the installed `claude` CLI with the documented stream-json
  protocol. The Agent SDKs and the VS Code extension use the same protocol. The Rust side
  forwards raw JSON lines and never interprets them. The UI ignores message types it does not
  know, so a Claude Code update hides a new feature instead of breaking the app. HQ adds no
  hooks and never writes Claude's settings files itself. The one exception is the MCP
  menu: turning a server on or off asks Claude to do it, and Claude saves the choice for that
  folder, just as `/mcp` does in the CLI.

## Build and run

Prerequisites: the Rust toolchain (MSVC), and `claude` on `PATH`.

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

- Reads `ui\` from disk, so F5 (or Ctrl+R) shows a UI edit without a rebuild. A reload stops
  every `claude` in the dev copy; the cells come back from its board. Rust changes still need
  `dev.cmd`.
- Opens DevTools on port 9223 (`HQ_DEBUG_PORT` changes it), for CDP tools such as the Chrome
  DevTools MCP server. Right-click > Inspect works too.

## Releases and updates

An installed copy of HQ updates itself:

- It checks for a new release 20 seconds after it starts, then every 4 hours.
- It downloads the new installer in the background and checks its signature.
- It installs when nothing would be lost: no session is working or waiting on you, no draft
  or unseen reply is left, and there has been no mouse or key input for 2 minutes (15 while
  HQ is the active window). HQ then restarts with the same board. Sessions resume when you
  next send them a message.
- Until then, an **Update** button in the top bar installs it at once. If you close HQ
  first, the update installs as HQ closes.

Only a copy installed with the setup program updates itself. A build under `target\` never
does. Set `HQ_NO_UPDATE=1` to turn updates off.

To publish a release, commit your changes and run:

```powershell
.\release.ps1 0.2.1          # notes default to the commit subjects since the last tag
```

The script does these steps:

1. Sets the version.
2. Builds the NSIS installer in `target\dist` with the Tauri CLI (through `npx`, in the same
   VS 2022 environment as `build.cmd`).
3. Signs the installer with `%USERPROFILE%\.tauri\hq-updater.key`.
4. Commits, tags and pushes.
5. Publishes the installer, its signature and `latest.json` as a GitHub release.

Installed copies read `latest.json` from the newest release at the endpoint in
`tauri.conf.json`. **Back up the private key.** Without it, no new release can be signed, and
every installed copy must be reinstalled by hand.

## Using it

- **New session** (Ctrl+N) lists recent folders. You can also browse to any folder on any drive.
  A new cell starts `claude` only when you send its first message, so empty cells cost nothing.
- **Resume** (Ctrl+O) continues a session started elsewhere, such as a VS Code tab, a terminal
  or the desktop app:
  - It lists the 80 most recent sessions from Claude's own transcripts, newest first.
  - Search by title, prompt or folder, or filter by where the session was last used.
  - Click a session to open it in a cell with its history. Claude resumes it (`--resume`) when
    you send a message.
  - A session changed in the last 5 minutes is marked **Active**. Close it in VS Code first,
    because two windows writing to one session make a mess of it.
  - Alternatively, **Fork** it (the branch icon): your first message starts a copy
    (`--fork-session`) and leaves the original alone.
- **Plan usage** sits in the top bar: the 5-hour usage and the time until it resets.
  - It is colored on a heat scale: green at 0%, yellow at 60%, red at 100%.
  - Hover it for the 5-hour and weekly limits with their reset times. Click it to refresh.
  - The numbers come from Claude's own `get_usage` request, the same data `/usage` shows.
    HQ asks a running session when there is one. Otherwise it starts a short-lived `claude`
    that skips MCP servers and hooks and saves no transcript.
  - It refreshes after each finished turn, and every 5 minutes (15 when no session is running).
- Each project folder gets its own color. The cell header and its entry in the sessions list
  use it, so sessions in the same folder are easy to spot.
- Status cues on each cell:
  - A band of light sweeps across the project name in the cell header: Claude is working.
  - Pulsing amber glow: Claude is waiting on you (a permission, a question or a plan).
  - Green glow: the session finished while you were looking elsewhere.
  - Spinner badge with a count, and a hollow status dot: background agents or shells that
    Claude started are still running. Hover the badge to see them. Closing the cell, or an
    update restart, would stop them, so HQ treats the cell as busy until they finish.
  - The top bar shows the totals. The taskbar button flashes when HQ is in the background.
- **Session names:** the cell header shows the session's name after the project name.
  - Claude names a new session after its first reply, as the VS Code extension does.
  - Click the name (or press F2) to rename it. The new name is saved in Claude's own
    transcript, so VS Code and `claude --resume` show it too.
- **Prompts and replies only:** the eye button in the cell header hides tool calls, thinking,
  narration and status notes. It is set per cell and kept across restarts. Errors, and the
  last words of a turn you stopped, stay visible.
- Claude's text comes in two styles:
  - **Narration:** muted grey text between tool calls, where Claude says what it is doing.
  - **Reply:** the answer that ends a turn, shown in a card with an orange edge.

  A reply streams in as plain text and becomes a card when the turn finishes. Prompts and
  replies show the time they were sent in the top-right corner (with the date once it is not
  today).
- **Questions:** when Claude asks you something, the chat keeps a one-line record of the
  question and your answer. Click it to see every option and which ones you picked. The eye
  button never hides it.
- **Sessions list** (Ctrl+B, or the button at the top left) shows every session in one column:
  - Each row has its status, first prompt and latest reply.
  - Type to search by folder, path, prompt or reply. Press Enter to jump to the first match.
  - Click a session to focus it and flash it. If another cell is maximized, the clicked
    session takes its place.
  - Hover a session to highlight its cell.
- **Images:** paste them (Ctrl+V), drop them on a cell, or use the image button. Images larger
  than 2000 px or 3.75 MB are scaled down before they are sent.
- **Model and effort:** the model button in the composer opens a menu.
  - Pick a model, then an effort level (low to max). Only the levels that model supports are
    shown.
  - The list comes from Claude itself. A change applies at once to a running session, or at
    start for a new one.
- **MCP servers:** the plug button lists the servers for that session with their status.
  - Turn a server on or off, or reconnect it. Opening the menu starts Claude if it is not
    running yet.
  - The button turns amber when a server has failed or needs sign-in.
  - Server settings (which can hold tokens) are never shown or stored by HQ.
- Permission cards work like the extension:
  - **Yes**: allow this tool use once.
  - **Yes, and don't ask again**: uses Claude's own suggested rule.
  - **No**: deny, with optional feedback for Claude.
  - Keys 1/2/3 answer the top card.
- Permission mode selector per cell: ask before edits, edit automatically, plan, auto, bypass.
  If Claude rejects a switch, the cell shows the reason and keeps the old mode. For example,
  bypass can't be turned on mid-session, and auto isn't available on every model.
- Keys:
  - Esc: stop the focused session.
  - Ctrl+1…9: focus a cell.
  - Ctrl+M: maximize the focused cell. You can also double-click its header.
  - Ctrl+W: close the focused cell.
  - Ctrl+B: show or hide the sessions list.
  - Ctrl+O: resume a session from VS Code or the terminal.
  - F2: rename the focused session.
- The board is saved to `%APPDATA%\dev.brydon.hq\board.json` (`dev.brydon.hq.<profile>` for
  another profile):
  - It stores each cell's folder, session, mode, model, effort, name and view (all, or
    prompts and replies only).
  - On restart, each cell shows its history from Claude's own transcript.
  - Claude starts again (`--resume`) only when you send a message, so restored cells use no
    memory until you use them.
- Header buttons open the cell's folder in VS Code or in Explorer.

Environment overrides:

- `HQ_CLAUDE`: path to the `claude` binary.
- `HQ_MODEL`: default model for HQ sessions that have no model picked.
- `HQ_NO_UPDATE`: never check for updates.
- `HQ_UPDATE_ANY_COPY`: check for updates even in a copy that was not installed (for testing).
- `HQ_PROFILE`: run as a named profile (see [One copy per profile](#one-copy-per-profile)).
- `HQ_DEBUG_PORT`: DevTools port for a debug build (default 9223).
- `HQ_STATE_DIR`: folder for `board.json`, overriding the profile's own.

## Layout

```
src/session.rs   spawns one `claude -p --input-format stream-json ...` per cell; Windows job
                 object kills the whole process tree when a cell (or HQ) closes; usage probe
src/store.rs     board.json persistence, transcript lookup and session list, VS Code lookup
src/update.rs    self-update: checks and downloads signed releases; installs when told to
src/main.rs      Tauri commands, profiles, single-instance lock
src/dev.rs       debug builds only: UI read from disk, DevTools port
ui/app.js        cells, protocol handling, prompts, menus, sessions list, grid layout, persistence
ui/render.js     markdown (raw HTML escaped, http(s) links only), tool rows, diffs, images,
                 project colors
ui/style.css     VS Code Dark Modern look
release.ps1      builds, signs and publishes a release
dev.cmd          rebuilds and restarts the dev copy
```

## Known limits

- Sessions run in `-p` (print) mode, which skips Claude's workspace-trust dialog. Only open
  folders you trust.
- Slash commands that only work in the interactive terminal, such as `/config`, are not
  available. Use the composer menus for model, effort and MCP servers. Skills, custom commands
  and `/compact` work.
- @-file mentions are not implemented yet.
