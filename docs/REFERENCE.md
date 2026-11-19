# HQ feature reference

Everything HQ does, in detail. New to HQ? Start with the [README](../README.md).

## Sessions and the board

- **First run:** if Claude Code is missing, HQ offers to install it. One click runs Anthropic's
  own installer (`install.ps1` on Windows, `install.sh` on a Mac) in the background, then
  opens Claude's sign-in (`claude auth login`) in a small window, which opens the browser.
  Once you approve there, HQ notices (it polls `claude auth status`), closes that window and
  carries on. No restart is needed: when `claude` is not on HQ's PATH, HQ uses the installer's
  location (`~/.local/bin`) directly. HQ asks again when Claude is signed out, or when a
  session reports it is not logged in; **Not now** hides it until then. HQ never sees the
  credentials: Claude stores them itself.
- **New session** (Ctrl+N) lists recent folders, each in its project colour. You can also browse to any folder on any drive.
  A new cell starts `claude` only when you send its first message, so empty cells cost nothing.
  - Esc (or the stop button) while Claude is still starting takes the prompt back into the
    composer, and nothing is sent. If Claude cannot start, the prompt comes back too, and its
    bubble is marked **Not sent**.
  - From a script or another session: `hq.exe --new-session <folder> [--model m] [--effort e]
    [--permission-mode p] [--prompt text]` opens a cell in that folder and sends the prompt
    (`--permission-mode` takes default, acceptEdits, plan or auto; never bypassPermissions;
    without it the session starts in auto). If
    HQ is running, the running copy of that profile opens it; otherwise HQ starts first. A
    missing folder keeps the prompt as a draft. `/full-send` uses this to hand a ticket to Sonnet.
  - From Explorer (Windows, the installed app): turn on **Open in HQ in Explorer** in Settings,
    then right-click a folder, or the empty space inside one, and pick **Open in HQ**. It runs the
    command line above with no mode given, so the session starts in auto, and AFK leaves it alone
    until you turn it on in it. The menu lives under HKCU and needs no admin.
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
  - Below the limits, **Credits** lists what has money left: dollar allowances such as cloud
    credits (amount left, of how much, and when they reset), a prepaid usage-credit balance,
    and what sessions spent on HQ's API key in this run. Each shows only when above zero. A
    Console key's own balance is not readable from HQ; the Console shows it.
  - It refreshes after each finished turn, and every 5 minutes (15 when no session is running).
  - Claude chats that hit the plan's usage limit continue automatically when usage returns.
    Uncheck **Continue when usage is back** on that chat's usage error to keep it stopped;
    the choice lasts only while that chat is open and does not affect other chats.
- **Token usage** (the graph button in the top bar, or Ctrl+U) shows where the tokens went:
  - Today, the last 7 or 30 days, or all time: the total, a bar per hour, day or month, and the
    split between input, output, cache writes, cache reads and subagents.
  - One line per session, folder or model, with its tokens, its cost and its share of the total.
    Click a session to open it; click a folder to see only its sessions.
  - It reads the `usage` Claude Code saves with every reply in its own transcripts, subagents'
    included, and counts each reply once. Nothing is asked of Claude, so looking costs no tokens.
    Hermes sessions are included too, read from Hermes' own state database and tagged Hermes.
    A transcript is read again only once it has changed, and one last changed before the period
    is not read at all.
  - Costs are estimates at the Claude API's list prices (`src/usage.rs` has the table, with cache
    write lifetimes, fast mode, US-only inference and web searches). A Pro or Max plan is not billed
    these amounts; it shows what the work would cost on an API key. A model the table does not
    know shows its tokens without a cost. Codex and Hermes cells are not counted.
- **API credits** let a session carry on past your plan's usage limit on a Claude Console API
  key. Max and Team plans include monthly API credits, which pay only for a key from the
  Console organization linked in claude.ai's billing settings; any other key is billed as usual.
  - Save the key under Settings > API key for API credits. It is kept in the system keychain
    (Credential Manager on Windows, the login Keychain on macOS), never on disk in the clear or
    in the board, and the UI only ever sees `sk-ant-…` and its last four characters. A key that
    v0.17.0 left in the `api-key` file beside the board moves to the keychain on the next start.
  - HQ reads the key from the keychain only when a session switches to it, once per run. On
    macOS the ad hoc signature changes with every update, so the first switch after an update
    asks to let HQ use the Keychain item: click **Always Allow** (Deny just fails that start).
  - Nothing switches by itself. A session that hits the limit shows **Continue on API credits**
    on its limit card. Clicking it sends nothing: from your next message, that session's `claude`
    restarts with `ANTHROPIC_API_KEY` (and without `ANTHROPIC_AUTH_TOKEN` or cloud provider
    variables, which would outrank it) and resumes the conversation.
  - The first request on the key re-reads the whole conversation, since the plan's prompt cache
    does not carry over. With no model picked in the session, Claude uses the API's default.
  - The **API** chip in the composer bar marks a session on the key and shows what it has spent.
    Its menu goes back to the plan at once.
  - The session goes back to the plan by itself, restarting before the next message, once the
    plan's usage is back (from the refused request's reset time and the usage figures).
  - Claude reports the credential it uses at the start of each turn; if a session meant for the
    key is not on it (an `env` block in Claude's settings wins over HQ), HQ says so.
  - Sessions on the key leave plan usage, the model list and Remote Control alone: those follow
    the plan.
  - To run one session on the key from the start, click the key button next to the MCP button
    in a new Claude session before sending its first message (click again to undo). Only that
    session runs on the key, for its whole life; other sessions stay on your plan. The button
    goes once the first message is sent, and the API chip shows the session is on the key.
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
- **Remote Control** (Settings) lets you continue Claude sessions from claude.ai/code or the
  Claude mobile app. Prompts and permission answers from there show up in the cell.
  - It needs claude.ai subscription sign-in. The setting is hidden when Claude reports that
    Remote Control is not available, or when your organization turned it off.
  - The setting applies to new sessions and to every open one not set on its own. The first
    time, it follows your account's own auto-enable setting.
  - Right-click a session's header (or its row in the sessions list) to turn it off or on for
    that session alone, and, while it is connected, for **Open on claude.ai** and **Copy link**.
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
    Claude started are still running. Hover the badge to see them. Closing the cell would stop
    them; an update restart does not (see the session host in [DEVELOPMENT.md](DEVELOPMENT.md#releases-and-updates)).
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

  The level is set per cell and kept across restarts. The button is pinned to the header by
  default, and existing installs got it pinned once; unpin it and it stays unpinned.
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
  Remote Control, the default model, **Offer waiting mode** (General), automatic HQ updates,
  the Explorer menu, and the path of the `claude` program. There is no global permission mode.
- **Steer or Queue** (Claude and Hermes): the toggle in the composer bar sets what a message
  sent while the agent works does in that chat. **Steer** reaches the running turn: Claude reads
  it at its next step; Hermes stops its turn and the message goes next. **Queue** holds it (marked
  Queued, with an × to take it back) and sends it when the turn ends, several at once as one
  prompt. Claude starts on Steer and Hermes on Queue, as before. A held message is not kept
  across a closed HQ, though an update's restart sends it on.
- **Session menu:** right-click a session's header, or its row in the sessions list, for what
  that one session does differently from Settings: its permission mode, Remote Control and
  whether it is your deputy. The header's "…" menu has the same choices (a Permissions section),
  and **Session settings…** there opens the menu too.
- **Bundled skills:** HQ ships the `one-sentence` Claude Code skill and its `/1s` command
  (`skills/`) and writes each to `~/.claude` (or `CLAUDE_CONFIG_DIR`) at each start when it is
  missing. A copy already there is never overwritten, so your edits stay. `/1s` and
  `/one-sentence` also work in Hermes sessions: HQ sends the skill's rules to Hermes and trails
  each later prompt with a short reminder (hidden in the chat) until `/1s off`. Both show in a
  Hermes cell's slash menu.
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
- **Waiting mode:** the hourglass button in the cell header (pinned by default, and existing
  installs got it pinned once) hides the chat behind a waiting view that shows the task list
  and a loader. Turning it on while a session is idle but still has open tasks (or background
  tasks) shows the view straight away.
  - When a turn has run 45 seconds without you clicking or typing in that session, a slim bar
    above the composer offers waiting mode (**Yes** or **No**), once per turn. It goes away
    when the turn ends.
  - **Offer waiting mode** in Settings > General turns the offer off. Three **No**s in a row
    turn it off by themselves.
- **Orchestrator columns:** the pin button in the cell header pins a session as an
  orchestrator. Each pinned session gets a full-height column, side by side, and the other
  sessions stack in a strip on the right (drag the gaps to resize). Pinned columns take the
  place of the spotlight while any is open. Above each column, a bar shows how many of the
  sessions it started are done, and turns amber while one of them waits on you; the × there
  unpins it. Pinning is your choice and is kept across restarts: starting sessions never pins
  one.
- **Closing HQ** while sessions are working (a turn, a question or permission, background
  tasks) asks first, and lists them. Esc or **Keep HQ open** keeps it. If the page does not
  answer, a second close a few seconds later goes through.
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
- **MCP servers:** the MCP button lists the servers for that session with their status.
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
  - A lone card fits the space under the chat with one scroll area (the command or diff), its
    choices always in view, and long commands wrap. The expand button in the card's title gives
    the card the whole cell.
- **Permissions:** every new session, whether you or an agent started it, starts in Claude's
  **auto** mode. Hermes has no auto mode, so new Hermes sessions start in **Edit
  automatically**: edits in the folder go ahead and commands follow Hermes' own approval
  settings. Settings has no global permission choice, and there is no "Follow Settings".
  - A session's own mode is set from the Permissions section of the header's "…" menu (the
    right-click header menu has it too). The composer bar has no permission button.
  - Boards saved before this move their open sessions that were in the old default modes to
    auto, once. Sessions set on their own, started by an agent or the command line, or still
    running in a session host are left alone.
  - If Claude rejects a switch, the cell shows the reason and keeps the old mode. For example,
    bypass can't be turned on mid-session, and auto isn't available on every model.
- **AFK:** going away for a while? The coffee button in the top bar tells every session you are
  AFK for 30 minutes to 8 hours, until a time you pick, or until you are back. Sessions then carry
  on with what they can do without you, instead of stopping at the first question.
  - A session that is working is told at once, mid-turn: Claude reads it at its next tool call
    and Codex is steered with it. Hermes can only read a message between turns, so it reads it
    when its current turn ends.
  - Questions are answered with "go with the option you would recommend and note the
    assumption". Plans are held for you. Any other permission request is declined as
    **deferred, not refused**: the agent is told not to look for another way to do the same thing,
    to note it as pending and to carry on with the rest. Each card is left for a minute first,
    in case you are still there, and counts it down ("AFK: Deferring in 0:42", with a draining bar).
  - Only Claude reads the reason given with a declined request. Codex and Hermes learn from the
    AFK message that a decline now means "later"; HQ sends it before their first decline.
  - When a turn ends, the session is asked to carry on, or to list what is waiting for you if
    nothing is left. A nudge answered without any tool call ends the nudging, a held plan stops
    it, and it stops after 5 nudges per AFK period.
  - A session that hits the plan's usage limit carries on by itself once usage is back.
  - Each thing HQ did shows as an "AFK: …" line in the chat. Once you are back (**I'm back**, or
    the time runs out), each session gets a **While you were away** card listing what was
    deferred, answered and held. Tell the session to go ahead with the deferred steps if you agree
    with them.
  - The coffee button in a cell's header sets AFK for that session only, or **Never AFK** keeps
    that session waiting for you even while the rest of HQ is AFK.
  - AFK, and what was done while it lasted, is kept in the board, so a restart (or an update)
    loses neither.
- **Deputy:** a session that answers for you while you are away. Pick one with **Answer
  prompts for me** in a session's menu (right-click its header; Claude and Codex sessions), or
  tick **Make a deputy to answer approvals and questions while I'm away** in the AFK menu.
  - A permission, question or plan that waits in another session goes to the deputy after a
    minute. It allows it once, denies it with a reason, or leaves it for you. It works beside
    AFK: what it leaves falls to AFK's deferral. It never answers for its own prompts, sessions
    tied to it, cloud cells or sessions a command line opened.
  - The AFK menu option uses an existing deputy as it is. If there is none, it starts a new
    Claude session named Deputy in an empty folder of HQ's own (never a project folder, so no
    project `CLAUDE.md`, settings or hooks reach it), set to Never AFK, and makes it the deputy.
    When no AFK that asked for it is left, it stops being the deputy; the session stays open.
    A deputy you picked yourself is kept and never ended by AFK.
  - The deputy sees long commands, edits and plans in full. Only extremely large inputs (over
    60,000 characters, or 100,000 for plans) are still too long to allow; it can only deny or
    leave those.
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
  - Ctrl+U: token usage.
  - F2: rename the focused session.
- The board is saved to `%APPDATA%\dev.brydon.hq\board.json` (`dev.brydon.hq.<profile>` for
  another profile):
  - It stores each cell's folder, session, mode, model, effort, name, view (tool calls shown
    or hidden) and whether it is collapsed, plus the VS Code workspace picked for each
    folder, and AFK (HQ's and each session's own) with what it did so far.
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

### Cloud cells

A cloud cell runs on Anthropic's Managed Agents instead of this PC, so it uses none of its CPU or
memory, and it is billed to the API key saved under Settings > API key for API credits (never to
a Claude plan). Pick **Cloud** in the new-session switch before choosing a folder.

- The session is created with the first message you send; a cell that never sends one costs
  nothing. HQ makes one environment and one agent per model in your Console organization
  (named "HQ cloud") and reuses them; the sessions show in the Console's Managed Agents list.
- The container starts **empty**: it has none of the folder the cell was opened on, so a cloud
  cell suits questions, research and work that needs no local files. Its network is unrestricted
  and its tools (shell, files, web search and fetch) run without asking.
- Models: Sonnet 5.5 (the default), Opus 5.5, Haiku 5.5 and Fable 5.1, picked before the first
  message. Closing the cell ends HQ's connection but leaves the session in the Console. A cloud
  session cannot be resumed in HQ afterwards.

### Codex cells

Codex is switched off in HQ's builds since 2026-11-05, because its support is ending: the picker
offers no Codex, and HQ refuses to start a Codex session. What follows applies only to a copy
built with `--features codex`.

A cell can run [Codex](https://developers.openai.com/codex) (OpenAI's coding agent) instead of
Claude Code. Pick **Codex** in the switch at the top of the new-session picker; the switch stays
where you left it. A Codex cell has a **Codex** tag in its header.

- Install Codex once (`npm install -g @openai/codex`, or any of its installers) and sign in with
  `codex login` in a terminal (a ChatGPT plan or an API key). HQ never sees your credentials:
  Codex keeps them in its own folder (`~/.codex`).
- HQ starts `codex app-server` for the cell: the JSON-RPC protocol the Codex VS Code extension
  uses, built into the `codex` CLI. `src/codex.rs` translates between it and the chat. Nothing
  starts until the cell's first message, and closing the cell ends Codex, so with no Codex cell
  open there is no cost. Only the protocol's stable part is used, and unknown messages are
  ignored, so a Codex update adds what HQ does not know about instead of breaking it.
- Codex stays current by itself: HQ runs `codex update` soon after it starts and then at most
  every 6 hours, but only while no Codex cell is running (the update replaces the binary). A cell
  that starts during an update waits for it.
- The model menu lists the models Codex reports, each with the effort levels it supports, so new
  models show up without an HQ update. **Default** is the model in Codex's own `config.toml`.
- Permission modes are Codex's three presets: **Read only** (asks before it edits a file or
  runs anything that needs more), **Edit automatically** (Codex's default: edits and commands
  inside the folder, asking for anything beyond it) and **Full access** (never asks).
  **Yes, and don't ask again** allows that command for the rest of the session (or, when Codex
  offers only that, adds Codex's own rule for it); for edits it switches to Edit automatically.
- A message sent while Codex works goes into the running turn, as with Claude. Stop interrupts
  the turn. The compact button (and `/compact`) asks Codex to compact the conversation.
- Handoff and the progress button ask in a throwaway, read-only fork that is never saved.
- Codex keeps the conversations. A restored cell draws its history from Codex's own copy (through
  a short-lived helper shared by all cells), so turns taken in the Codex CLI or VS Code since show
  too. Once a Codex cell has run, the Resume dialog also lists Codex's sessions.
- Not available in Codex cells: Remote Control, the MCP menu (set up Codex's MCP servers with
  `codex mcp` or its config), plan mode and automatic task lists (newer Codex models plan
  without a todo tool). Plan usage in the sidebar is Claude's.

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
  automatically and Don't ask (still asks for sensitive paths); Hermes has no plan mode or auto
  mode, so new Hermes sessions start in Edit automatically.
- The compact button sends `/compress`. Typing `/` lists Hermes' own commands, plus `/1s` and `/one-sentence`.
- Handoff and the progress button ask in a throwaway fork of the session, which stays in
  Hermes' session list.
- Restored cells draw their history from HQ's own copy of the conversation, kept in a
  `hermes` folder next to `board.json`, so they start nothing until you send a message.
- Once a Hermes cell has run, the Resume dialog also lists Hermes' sessions (it starts a
  short-lived Hermes to ask). Searching inside conversations covers Claude sessions only.
- Not available in Hermes cells: Remote Control, the MCP menu (set up Hermes' MCP servers in
  its config) and effort levels. Plan usage in the sidebar is Claude's.

### HQ tools (MCP)

Every session HQ starts gets HQ's own MCP server, `hq`, so an agent can work with the rest of
the board: start a Codex session for one part of a job, wait for it, read its answer.

- **Sessions:** `list_sessions`, `spawn_session` (Claude, Codex or Hermes, in any folder,
  optionally in a new git worktree beside the repository), `send_message`, `read_session` (the
  full text of its latest replies and its task list), `wait_for_sessions`, `interrupt_session`,
  `close_session`, `set_session_model` and `collapse_session`. Sessions inside HQ message each
  other with no approval, even a session that runs with fewer prompts than the sender; an agent
  outside HQ still needs your yes for a session it did not start.
- **Started sessions:** a session an agent started joins its starter's group and wears the
  starter's project colour and name, even in a worktree. Its header links back to the starter
  ("↳ name"); the starter's header counts what it started, pulses while one waits on you, and
  lists them. **Sessions agents start** in Settings opens them docked (the default), full size
  or small (half width, smaller text).
  - **The tray:** docked sessions sit in a tray inside their starter, above its composer (or as
    a card in its waiting view), one row each with its state. A question or permission one asks
    opens in its row, to answer there without opening it; meanwhile the starter's border turns
    amber. Click a row to open that session in the grid; **Open all** and **Dock all** move
    them together. When the starter is collapsed or closed, they go back to the dock. A
    collapsed session that another open session started sits in that session's tray instead of
    the dock.
- **Coordination:** shared notes (`note_set`, `note_get`, `note_delete`) kept across restarts;
  advisory file claims (`claim_files`, `release_files`, `list_claims`) held until released or
  the session's cell closes; `schedule_message` (after a delay, at a time, or once the plan's
  usage limit resets), `list_scheduled` and `cancel_scheduled`.
- **The user:** `notify_user` (a desktop notification), `ask_user` (a question card in the
  asking session's cell; answered at once while AFK), and `get_usage` (the plan's limits).
- A session started by an agent opens as a cell marked ↳, without taking the keyboard, and gets a
  first line saying who started it.

Guard rails:

- Read-only tools, notes, claims and notifications are allowed up front. Starting, messaging,
  stopping and closing sessions ask for permission in Claude sessions like any other tool, unless
  the session's permission mode or rules already allow them.
- A session can start sessions, and those one more level, but no deeper. At most 6 sessions
  started by agents are open at once (Settings > **Sessions agents can open**).
- A session started by an agent never bypasses permissions and is never freer than the
  starter's own mode. It defaults to auto for Claude and Edit automatically (acceptEdits) for
  Hermes.
- An agent may only interrupt, close or reconfigure sessions it started, directly or through
  sessions it started.
- Settings > **HQ tools for sessions** turns the server off for sessions started from then on;
  **Let agents start sessions** turns off only `spawn_session`.

How it connects: HQ listens on 127.0.0.1 at a port the system picks, and refuses requests that
come from a browser page or carry no session token. Claude gets the server (and its token)
through `--mcp-config` with a file only you can read. Codex (`-c mcp_servers.hq...`) and Hermes
(ACP `mcpServers`) run `hq --mcp-stdio`, a small bridge that forwards to the same server, with
the token in its environment. Waiting tools stream keep-alives, so a long wait does not time out.

**HQ tools everywhere** (Settings) installs the bridge as `hq` for every Claude Code (user
scope, through `claude mcp add`), Codex (`~/.codex/config.toml`) and Hermes
(`config.yaml` in `$HERMES_HOME`, `%LOCALAPPDATA%\hermes` on Windows, else `~/.hermes`) on this computer, so agents started in a terminal or an editor can use
the tools too while HQ is open. They act as "outside" (`ask_user` is not available to them). The
bridge finds the running HQ through `~/.hq/mcp.json`, which HQ writes while it runs (readable
only by you). Each config file edited gets a `.hq-backup` copy first. **Remove** takes it out
again. Inside HQ, each session's own `hq` takes the place of the machine-wide one.

Not yet tested: Hermes with the `mcpServers` HQ passes over ACP, and whether Hermes shows two
`hq` servers when the machine-wide one is installed too.


## Known limits

- The Explorer **Open in HQ** menu is kept in HKCU and the uninstaller does not remove it: turn
  it off in Settings before you uninstall.
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
