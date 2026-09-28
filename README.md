# HQ

A small desktop app that runs many Claude Code chat sessions side by side, each in its own
folder, on any drive. It is inspired by HNA-Code, with these differences: the board starts
empty, you add sessions one at a time, the grid lays itself out, and each cell is a chat view
styled like the VS Code extension instead of a terminal.

- **Rust + Tauri 2.** The app uses the WebView2 runtime that ships with Windows. The release
  binary is about 3.5 MB. The shell (without Claude) uses about 180 MB of private memory.
  Each session's `claude` process uses its own memory on top of that.
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
rustc would otherwise pick it. To build an installer (NSIS), install the Tauri CLI with
`cargo install tauri-cli --version "^2"` and run `cargo tauri build`.

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
  - Blue progress bar: Claude is working.
  - Pulsing amber glow: Claude is waiting on you (a permission, a question or a plan).
  - Green glow: the session finished while you were looking elsewhere.
  - The top bar shows the totals. The taskbar button flashes when HQ is in the background.
- Claude's text comes in two styles:
  - **Narration:** muted grey text between tool calls, where Claude says what it is doing.
  - **Reply:** the answer that ends a turn, shown in a card marked **Claude**.

  A reply streams in as plain text and becomes a card when the turn finishes.
- **Sessions list** (Ctrl+B, or the button at the top left) shows every session in one column:
  - Each row has its status, first prompt and latest reply.
  - Type to search by folder, path, prompt or reply. Press Enter to jump to the first match.
  - Click a session to focus it and flash it. If another cell is maximized, the clicked
    session takes its place.
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
- The board is saved to `%APPDATA%\dev.brydon.hq\board.json`:
  - It stores each cell's folder, session, mode, model, effort and first prompt.
  - On restart, each cell shows its history from Claude's own transcript.
  - Claude starts again (`--resume`) only when you send a message, so restored cells use no
    memory until you use them.
- Header buttons open the cell's folder in VS Code or in Explorer.

Environment overrides:

- `HQ_CLAUDE`: path to the `claude` binary.
- `HQ_MODEL`: default model for HQ sessions that have no model picked.
- `HQ_STATE_DIR`: folder for `board.json`, so a second copy (such as a test build) keeps its own
  board.

## Layout

```
src/session.rs   spawns one `claude -p --input-format stream-json ...` per cell; Windows job
                 object kills the whole process tree when a cell (or HQ) closes; usage probe
src/store.rs     board.json persistence, transcript lookup and session list, VS Code lookup
src/main.rs      Tauri commands
ui/app.js        cells, protocol handling, prompts, menus, sessions list, grid layout, persistence
ui/render.js     markdown (raw HTML escaped, http(s) links only), tool rows, diffs, images,
                 project colors
ui/style.css     VS Code Dark Modern look
```

## Known limits

- Sessions run in `-p` (print) mode, which skips Claude's workspace-trust dialog. Only open
  folders you trust.
- Slash commands that only work in the interactive terminal, such as `/config`, are not
  available. Use the composer menus for model, effort and MCP servers. Skills, custom commands
  and `/compact` work.
- @-file mentions are not implemented yet.
