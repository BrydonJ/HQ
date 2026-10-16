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

`cargo test` runs the backend's unit tests (stream filtering, batching, transcript reading,
session search). The UI has none; check it in the dev copy.

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

## macOS

HQ also runs on macOS 10.15 or later, Apple Silicon and Intel (one universal app). It needs
Claude Code installed (`curl -fsSL https://claude.ai/install.sh | bash`).

**Install:** open `HQ_<version>_universal.dmg` and drag HQ to **Applications**. Run it from
there: only a copy in `/Applications` (or `~/Applications`) updates itself, and it updates the
same way as on Windows, from the same releases.

**First launch:** the app is signed ad hoc, without an Apple Developer ID, so macOS blocks it
once. Either open **System Settings > Privacy & Security** and click **Open Anyway** next to the
HQ message, or run this once in Terminal:

```sh
xattr -dr com.apple.quarantine /Applications/HQ.app
```

An in-app update needs neither step, because the updater's download carries no quarantine flag.

What differs from Windows:

- Shortcuts use Cmd where Windows uses Ctrl (Cmd+N, Cmd+O, Cmd+B, Cmd+1...9, Cmd+M, Cmd+W,
  Cmd+Shift+M, Cmd+,). Cmd+W closes the cell, not the window.
- A shell block's Run button opens Terminal and runs the code with its own shell (bash, sh, or
  zsh for anything else), then leaves a shell open there. cmd blocks have no Run button.
  PowerShell blocks run when `pwsh` is installed. The terminal button opens Terminal, and the
  VS Code button uses the `code` command or the VS Code app.
- An app started from the Dock gets only a bare `PATH`, so HQ asks your login shell for the real
  one at start (and adds `~/.local/bin`, `~/.claude/local`, Homebrew and npm/bun folders).
  `claude` and `hermes` are found from there; Settings still lets you point to either.
- Closing a cell (or quitting HQ) ends its `claude` and everything it started, through a
  process group in place of the Windows job object. If HQ itself is killed, they are not ended.
- The dev copy has no DevTools port. Use Safari's Develop menu to inspect it.

Build on a Mac with `./build-mac.sh` (universal build, output under
`target/universal-apple-darwin/release/bundle`) and run a dev copy with `./dev-mac.sh`. The
`Build HQ for macOS` GitHub workflow runs the same build; publish its output with
`.\release.ps1 <version> -MacArtifacts <folder>` (see the script's header). The workflow needs
the repository secrets `TAURI_SIGNING_PRIVATE_KEY` (the contents of the updater key) and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.

## Using it

- **New session** (Ctrl+N) lists recent folders. You can also browse to any folder on any drive.
  A new cell starts `claude` only when you send its first message, so empty cells cost nothing.
  - Esc (or the stop button) while Claude is still starting takes the prompt back into the
    composer, and nothing is sent. If Claude cannot start, the prompt comes back too, and its
    bubble is marked **Not sent**.
  - From a script or another session: `hq.exe --new-session <folder> [--model m] [--effort e]
    [--permission-mode p] [--prompt text]` opens a cell in that folder and sends the prompt
    (`--permission-mode` takes default, acceptEdits, plan or auto; never bypassPermissions). If
    HQ is running, the running copy of that profile opens it; otherwise HQ starts first. A
    missing folder keeps the prompt as a draft. `/full-send` uses this to hand a ticket to Sonnet.
  - `claude update` runs in the background, at most once every 6 hours (the time of the last run
    is kept in the board), and a first message does not wait for it. If a start fails while the
    update replaces the program, HQ waits for the update and tries once more.
- **Resume** (Ctrl+O) continues a session started elsewhere, such as a VS Code tab, a terminal
  or the desktop app:
  - It lists the 80 most recent sessions from Claude's own transcripts, newest first.
  - Search by title, prompt or folder, or filter by where the session was last used.
  - Each session shows the folder it started in (Claude files the transcript there, so HQ
    resumes it from there) and when it started. If Claude later worked in another folder, the
    row says where it was last used.
  - The folder list narrows it to one folder; clicking a session's folder does the same. It
    then lists that folder's 80 most recent sessions, older ones included, not only those among
    the 80 newest overall. A session matches both the folder it started in and its last one.
  - **In text** searches inside the conversations, not only titles and prompts. Matches stream
    in with a snippet, newest session first (up to 5 per session, 100 in all).
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
- **Context window:** a small ring in the composer bar (it appears after the first reply) shows
  how full the session's context is.
  - The ring fills toward the auto-compact point and turns from green to red as it nears it
    (toward the end of the window when auto-compact is off). The number is the share of the
    whole window, the same one `/context` shows.
  - Hover it for the tokens used, the auto-compact point, and what the tokens are made of
    (system prompt, tools, skills, messages…). HQ asks Claude for that breakdown on hover
    only, and keeps the answer for 30 seconds.
  - Click it to run `/compact`. Shift+click adds instructions for the summary. While a turn is
    running the button waits. The chat shows "Compacting the conversation…" and then
    "Conversation compacted" (or why it could not).
  - After a restart the ring shows the token count from the restored transcript. The window
    size is learned from the first reply.
  - The cell's meta line, and the hover, also show the cost Claude Code reported in this HQ
    run (a total of what `result` messages say; it starts again from 0 when HQ restarts).
- **Remote Control:** the broadcast button in the composer bar, next to the MCP button, lets you continue the session
  from claude.ai/code or the Claude mobile app. Prompts and permission answers from there show
  up in the cell.
  - It needs claude.ai subscription sign-in. The button is hidden when Claude reports that
    Remote Control is not available, or when your organization turned it off.
  - Click it for **Open on claude.ai**, **Copy link** and **Turn off**, or to turn it on.
    **Start with Remote Control on** sets the default for new sessions. The first time, it
    follows your account's own auto-enable setting.
  - For a new session, HQ turns it on right after the first prompt, so that claude.ai lists
    the session under its title. HQ sets the remote name only when you renamed the session or
    Claude gave it a title (a name locks the title). Otherwise Claude Code derives one from
    your prompt and HQ's rename and Claude's title update it. Sessions that would have no name
    at all get `HQ-<folder>`, never `<hostname>-<random>`.
  - It is saved per cell. A restored cell turns it on again when Claude starts for it, which
    happens when you send a message (or choose **Start Claude now** in the menu). HQ asks to
    reattach to the same claude.ai session, and that works across a restart.
- Each project folder gets its own pastel color. The cell header and its entry in the sessions
  list use it, so sessions in the same folder are easy to spot. Click the project name in the
  cell header for another color; the pick is kept per folder.
- Status cues on each cell:
  - A band of light sweeps across the project name in the cell header: Claude is working.
    Above the prompt box, a star cycles through the CLI spinner's glyphs next to a phrase
    ("Letting it cook…") picked for each turn. The chat leaves room for it, so it never covers
    the last message.
  - Pulsing amber glow: Claude is waiting on you (a permission, a question or a plan).
  - Green glow: the session finished while you were looking elsewhere.
  - Spinner badge with a count, and a hollow status dot: background agents or shells that
    Claude started are still running. Hover the badge to see them. Closing the cell, or an
    update restart, would stop them, so HQ treats the cell as busy until they finish.
  - The top bar shows the totals. The taskbar button flashes when HQ is in the background.
- **Session names:** the cell header shows the session's name after the project name.
  - Claude names a new session a few seconds after its first prompt, as the VS Code extension
    does, even while that first turn is still running.
  - Click the name (or press F2) to rename it. The new name is saved in Claude's own
    transcript, so VS Code and `claude --resume` show it too.
- **Detail levels:** the eye button in the cell header cycles through three levels:
  1. Everything: replies, thinking and tool calls.
  2. Replies, thinking and narration, with tool calls hidden. New cells start here.
  3. Replies only: your prompts and Claude's replies. Thinking and the grey narration between
     steps are hidden too; the last words of a stopped turn stay.

  The level is set per cell and kept across restarts.
- **Progress view:** the checklist button next to the eye opens a small card in the top-right
  corner of the chat, which stays readable and scrollable beside it, showing how far Claude
  has got. Close it with its X or the button. It is set per cell and kept across restarts.
  The card shows:
  - Whether Claude is working, how long the turn has run, how many tool calls it has made, and
    the tool running now.
  - Claude's task list with a percentage bar, when the model keeps one. In HQ sessions Claude
    Code only gives task tools to older models (4.6 and earlier, and Haiku 4.5). Sonnet 5.5 and
    Opus 5.5 work without a list, and HQ does not force one on.
  - Background agents and shells, with each agent's latest tool, tool count and summary. The
    stop button next to one stops just that agent or shell (`stop_task`); the turn goes on.
  - **Sitrep:** asks, beside the running turn (like `/btw`), what Claude is doing and what
    is left. Claude answers without stopping, and its rough percentage fills the bar. Each
    click is one extra request that reads the session's context from the cache.

  While a task list is open, a thin bar along the bottom of the cell header shows its
  percentage, so it is visible in the grid without opening the view.
- Claude's text comes in two styles:
  - **Narration:** muted grey text between tool calls, where Claude says what it is doing.
  - **Reply:** the answer that ends a turn, shown in a card with an orange edge.

  A reply streams in as plain text and becomes a card when the turn finishes. Prompts and
  replies show the time they were sent in the top-right corner (with the date once it is not
  today).
- **Edits** show a real line diff with a few lines of context (and the line numbers Claude Code
  reports once the edit is done). Tool input and output are built when you open the tool, and
  very long ones are cut with a **Show all** button.
- **Colors:** replies color headings, bold, italics, list markers and inline code, and code
  blocks tagged with a language are syntax highlighted (highlight.js, about 35 common
  languages). Untagged blocks stay plain.
- **Copy:** a **Copy** button under each reply (on hover) copies it as Markdown, and each code
  block has its own copy button.
- **Handoff:** the note button in the cell header asks Claude, beside the conversation (no extra
  turn), for a Markdown handoff (goal, state, decisions, files, gotchas, next steps) that a
  fresh session can continue from, then asks where to save it (`HANDOFF.md` in the session's
  folder by default). It replaces the old export button.
- **Drafts:** text you have typed but not sent is kept in the board, and is back after a restart.
- **Slash commands:** type `/` at the start of the composer, or after a space anywhere in the
  text, to list the commands Claude reports (built-in ones, skills and custom commands). Arrow
  keys pick, Tab or Enter completes, Esc closes. A command that is already complete is sent by
  Enter. After text, Enter completes only once an arrow key picked a command; otherwise it
  sends the prompt, so a prompt ending in a path is not changed.
- **/clear** (or `/reset`, `/new`, optionally followed by a name) starts the cell over as a new
  session in the same folder. HQ handles it itself; the previous session stays on disk and
  Resume can open it. Stop Claude first if it is working.
- **Notifications:** while HQ is not the active window, a Windows notification names the session
  and shows the first line of its reply when a turn finishes, or says that Claude needs you.
  Coming back to HQ soon after one opens that cell. Turn them off in Settings.
- **Settings** (the gear in the top bar, or Ctrl+,): theme (auto, dark or light), notifications,
  Remote Control for new sessions, the default model, automatic HQ updates, and the path of the
  `claude` program.
- **Questions:** when Claude asks you something, the chat keeps a one-line record of the
  question and your answer. Click it to see every option and which ones you picked. No detail
  level hides it.
- **Collapse** (the minimize button in the cell header, or Ctrl+Shift+M) hides a cell's chat and
  moves its header to a strip under the grid. The other cells take its space.
  - The header keeps every status cue (dot, sweep, glow, background badge, task bar) and says
    the state in words: Working, Waiting on you, Finished, and so on.
  - A reply that finishes while collapsed gets the green glow, even if the cell had focus.
  - Click the header to expand it. Ctrl+1…9, the sessions list and maximize expand it too.
  - Keys 1/2/3 and Esc never act on a collapsed cell, since its prompts are not on screen.
- **Sessions list** (Ctrl+B, or the button at the top left) shows every session in one column:
  - Each row has its status, first prompt and latest reply.
  - Type to search by folder, path, prompt or reply. Press Enter to jump to the first match.
  - Click a session to focus it and flash it. If another cell is maximized, the clicked
    session takes its place.
  - Hover a session to highlight its cell.
- **Images:** paste them (Ctrl+V), drop them on a cell, or use the image button. Images larger
  than 2000 px or 3.75 MB are scaled down before they are sent. Click an image in the chat or
  the composer to see it full size; click or Esc closes it.
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
  - Keys 1…9 answer the top card. With several questions, they answer the highlighted one,
    and move on to the next after a single-choice answer. Click a question to highlight it.
- Permission mode selector per cell: ask before edits, edit automatically, plan, auto, bypass.
  If Claude rejects a switch, the cell shows the reason and keeps the old mode. For example,
  bypass can't be turned on mid-session, and auto isn't available on every model.
- Keys:
  - Esc: stop the focused session. If Claude has not stopped 10 seconds later, the chat offers
    **Force stop**, which ends the process; the session resumes when you next send a message.
  - Ctrl+,: settings.
  - Ctrl+1…9: focus a cell.
  - Ctrl+M: maximize the focused cell. You can also double-click its header.
  - Ctrl+Shift+M: collapse or expand the focused cell.
  - Ctrl+W: close the focused cell.
  - Ctrl+B: show or hide the sessions list.
  - Ctrl+O: resume a session from VS Code or the terminal.
  - F2: rename the focused session.
- The board is saved to `%APPDATA%\dev.brydon.hq\board.json` (`dev.brydon.hq.<profile>` for
  another profile):
  - It stores each cell's folder, session, mode, model, effort, name, view (tool calls shown
    or hidden) and whether it is collapsed, plus the VS Code workspace picked for each
    folder.
  - On restart, each cell shows its history from Claude's own transcript, newest part first
    (up to 1500 messages or about 4 MB). **Load earlier** at the top fetches more. Images in
    restored history are left out ("[image omitted]"). Collapsed cells draw their history
    when first opened. If the transcript ends in the middle of a turn, the chat says that
    the last turn was interrupted when HQ closed.
  - A cell whose folder is missing at startup stays on the board and in `board.json`, marked
    **Folder missing**, with its history and the composer off. **Locate folder…** points it at
    another folder; **Close** removes it. Rename the folder back and restart, and the cell
    works again as before.
  - Claude starts again (`--resume`) only when you send a message, so restored cells use no
    memory until you use them.
- Header buttons open the cell's folder in VS Code, in a terminal or in Explorer. The terminal is
  Windows Terminal (a console window if it is missing); the larger terminal button in the top bar
  opens one in its default folder.
  - Right-click the VS Code button to pick a `.code-workspace` file for the folder. The button
    then opens that workspace instead, and a small dot marks it. The choice applies to every
    session in the folder.
  - The menu lists workspace files in the folder, its parent, and the folders of workspaces
    picked before. Workspaces that include the folder come first. **Choose a workspace
    file…** browses for any other.
  - If the workspace file is gone, the button opens the folder and says so.

### Hermes Agent cells

A cell can run [Hermes Agent](https://hermes-agent.nousresearch.com) instead of Claude Code.
Pick **Hermes** in the switch at the top of the new-session picker; the switch stays where you
left it. A Hermes cell has a **Hermes** tag in its header.

- HQ starts `hermes acp` (Hermes' Agent Client Protocol server) for the cell, and
  `src/hermes.rs` translates between it and the chat. Nothing starts until the cell's first
  message, and closing the cell ends Hermes, so with no Hermes cell open there is no cost.
- It is the same Hermes agent as the CLI: its memory, skills, background self-improvement
  review, context compression and session store all work as configured in Hermes' own
  `config.yaml`. HQ changes none of Hermes' settings.
- Approvals show as the usual permission cards. **Yes, and don't ask again** allows that kind
  of action for the rest of the session. When Hermes' own `approvals.timeout` runs out, Hermes
  denies the request and the card goes away.
- The model menu lists the models Hermes offers. Permission modes are Ask before edits, Edit
  automatically and Don't ask (still asks for sensitive paths); Hermes has no plan mode.
- The compact button sends `/compress`. Typing `/` lists Hermes' own commands.
- Handoff and the progress button ask in a throwaway fork of the session, which stays in
  Hermes' session list.
- Restored cells draw their history from HQ's own copy of the conversation, kept in a
  `hermes` folder next to `board.json`, so they start nothing until you send a message.
- Once a Hermes cell has run, the Resume dialog also lists Hermes' sessions (it starts a
  short-lived Hermes to ask). Searching inside conversations covers Claude sessions only.
- Not available in Hermes cells: Remote Control, the MCP menu (set up Hermes' MCP servers in
  its config) and effort levels. Plan usage in the sidebar is Claude's.

Environment overrides:

- `HQ_CLAUDE`: path to the `claude` binary. The **Claude command** setting overrides it.
- `HQ_MODEL`: default model for HQ sessions that have no model picked.
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
src/hermes.rs    Hermes cells: runs `hermes acp` and translates ACP to and from the stream-json
                 lines the UI speaks; HQ's copy of each Hermes conversation. Built with the
                 `hermes` Cargo feature (on by default; `--no-default-features` leaves it out)
src/update.rs    self-update: checks and downloads signed releases; installs when told to
src/main.rs      Tauri commands, profiles, single-instance lock
src/dev.rs       debug builds only: UI read from disk, DevTools port (Windows)
src/macos.rs     macOS only: login-shell PATH, Terminal and VS Code launchers, installed-app check
ui/app.js        cells, protocol handling, prompts, menus, sessions list, grid layout, persistence,
                 context indicator, Remote Control, settings
ui/hermes.js     what Hermes cells do differently: their model list, commands, defaults, modes
ui/render.js     markdown (raw HTML escaped, http(s) links only; incremental while streaming),
                 tool rows, line diffs, images, project colors
ui/style.css     slate and pastel look, and a light theme
release.ps1      builds, signs and publishes a release
dev.cmd          rebuilds and restarts the dev copy
```

## Known limits

- Sessions run in `-p` (print) mode, which skips Claude's workspace-trust dialog. Only open
  folders you trust.
- Sessions run headless, so commands that need the interactive terminal either print a short
  answer (`/mcp`, `/agents`) or are not available (`/focus` is hidden from the list). Use the
  composer menus for model, effort and MCP servers. Skills, custom commands, `/compact` and
  `/context` work.
- Remote Control: whether a prompt typed on the phone shows in HQ was not tested (it needs the
  Claude app). HQ draws it when the CLI reports it. Whether claude.ai shows the title HQ expects
  was not tested either.
- @-file mentions are not implemented yet.
