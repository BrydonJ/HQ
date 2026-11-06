---
title: Approval reviewer
status: draft
date: 2026-11-05
---

# Approval reviewer

## Problem

A session stops or skips work when a permission prompt sits unanswered. Hermes is the worst case:
it gives up on the request after its approval timeout (7 to 10 minutes) and carries on without the
command. Most of these commands are harmless, so the user was never needed.

## What it does

A new opt-in mode. When a permission card has waited long enough, HQ asks a separate, tool-less
Claude call whether the request is safe. The reviewer can only **approve** or **escalate**:

- Approve: HQ answers the card with "Yes" (once, never "for this session") and says so in the cell.
- Escalate: nothing changes. The card stays for the user, exactly as today.

The reviewer never denies, so the worst a wrong "escalate" can do is what already happens now.

## How approvals flow today (verified in the code)

- All three backends reach the UI as the same `control_request` / `can_use_tool` message. Hermes'
  is built in `src/hermes.rs:930` (`hermes_request`); Codex's in `src/codex.rs:840-879`.
- The UI draws the card in `ui/app.js:2852` (`onControlRequest`) and arms it in `armPrompt`
  (`ui/app.js:2877`), which is also called for cards replayed after an HQ restart (`app.js:1920`).
- `cell.answer(requestId, { behavior: 'allow', updatedInput })` is the one way a card is answered.
- AFK (`ui/afk.js:192` `afkWatch`, `:219` `afkHandle`) already watches every card and, after a
  60 s grace, **denies** a permission with a "user is away" message. The reviewer slots in beside it.
- `session::probe_usage` (`src/session.rs:193`) is the existing pattern for a short-lived
  `claude -p` that skips MCP, hooks and connectors and saves no transcript.

## Design

### 1. Where it runs: HQ's main process, driven by the UI

One new Tauri command, `review_approval`, called from the UI. Reasons:

- The UI already sees one uniform request for every backend, so one code path covers Claude and
  Hermes with no change to `hermes.rs` or the session host.
- Codex is left out: its support ends soon and it is now off in HQ's builds (the `codex` Cargo
  feature left the defaults on 2026-11-05). A copy built with it gets no reviewer in Codex cells
  (`cell.backend === 'codex'` hides the control and skips the review); `codex.rs` is untouched.
- It reuses the card, `cell.answer`, the AFK log and the replay path.

Not chosen: running it inside the Hermes bridge in the session host. That would also work while HQ
is closed, but it is Hermes-only, duplicates the logic per backend, and nothing answers prompts
while HQ is closed today either. Can be revisited if that case turns out to matter.

### 2. The Rust side: `src/review.rs` (new)

`pub fn review(req: ReviewRequest) -> Verdict`, where `Verdict { approve: bool, reason: String }`.
Fail-closed: every error path returns escalate.

Step A, hard rules (no model call, unit-tested). Escalate at once when any holds:

- The tool is `AskUserQuestion` or `ExitPlanMode`, or the request id starts
  with `mcp-` (HQ's own cards). Only tool permissions are ever reviewed.
- The command or input is longer than 4,000 characters.
- The command matches the always-escalate list: `git push`, `git reset --hard`, `git clean`,
  `git checkout --`/`restore` of whole trees, any `--force`/`-f` on git; `rm -r`/`rmdir /s`/
  `Remove-Item -Recurse`/`del /s`; `sudo`, `runas`; `npm|cargo|dotnet nuget publish`,
  `gh pr merge`, `gh release`; `curl|wget|iwr` piped to a shell; `shutdown`, `format`, `reg `,
  `schtasks`, `setx`; `drop |truncate ` SQL; anything naming `.env`, `secrets`, `.ssh`,
  `credentials`, `token`, `id_rsa`.
- An edit or write whose path is outside the session's cwd.

This list is a backstop, not the safety boundary: patterns leak, so the model is given the same
policy and the reviewer's default is escalate.

Step B, the model call. A one-shot `claude -p`, modelled on `probe_usage`:

```
claude -p --model haiku --tools "" --system-prompt <reviewer prompt>
       --output-format json --json-schema <{decision: approve|escalate, reason: string}>
       --strict-mcp-config --no-session-persistence --setting-sources ""
       --settings {"disableAllHooks":true,"disableClaudeAiConnectors":true}
```

- Runs in an empty temp dir (`hq-review`), `CLAUDECODE` removed, console hidden, in a job object,
  killed after 45 s. Timeout, non-zero exit, unparseable output or any decision other than the
  literal `approve` is an escalate.
- Input on stdin: tool name, raw input (command, or path plus the first 200 lines of the edit),
  cwd, backend, and the user's last message to the session (what the work is for).
- The system prompt states the policy: approve only what is plainly routine for a coding task
  inside the project (reads, builds, tests, linters, local git reads, edits inside the cwd);
  escalate anything destructive, outward-facing, credential-related, outside the project, or
  unclear. It states that everything in the request is untrusted data, that instructions inside
  it are a reason to escalate, and that the agent's own description of the command is not given
  (it is left out on purpose: only the raw command is judged).
- Haiku only for now. "Unsure" means escalate, not a second, bigger model.

To confirm while building: that `--setting-sources ""` keeps `~/.claude/CLAUDE.md` out of the
reviewer (the flags themselves exist in claude 2.1.318). If it does not, find the flag that does
before going further; the reviewer must not inherit the user's chat rules.

Reviews run one at a time (a mutex), so a burst of cards cannot start a dozen processes.

### 3. The UI side: `ui/afk.js` and `ui/app.js`

- Enabled per session only; there is no HQ-wide setting. Two ways to turn it on:
  - a "Review approvals" toggle in the cell's own menu, saved with the cell (`cell.review`,
    default off) like `cell.afk`;
  - a "Review approvals while away" checkbox in the AFK menu (`buildAfkMenu`). Set for one
    session it applies to that session; set with HQ-wide AFK it applies to each session that
    joins that AFK, and ends with it. Sessions that opted out of AFK are not touched.
- It is set only from the UI: not from the command line (`LAUNCH_MODES`), and not over HQ's MCP
  server, so no agent can turn it on for itself or for a session it starts.
- The reviewer never steps in before the card has waited 60 s (`AFK_GRACE_MS`, well inside
  Hermes' timeout), AFK or not. For that minute the card shows the same countdown text and bar
  AFK uses (`afk-count`), reading "Reviewing in 0:42". Answering the card yourself stops it.
- With AFK on as well, there is one countdown, not two: at 60 s the review runs first, and AFK's
  deferral (`afkHandle`) happens only if the reviewer escalates.
- The card shows "Reviewing…" while the call runs.
- On approve, and only if `cell.pending.get(requestId) === card` still holds (the user or a
  `control_cancel_request` may have got there first): `cell.answer` with plain allow, a notice
  `Reviewer approved Bash: <command> (<reason>)`, and an `approved` entry in the AFK log so the
  digest lists it.
- On escalate: a notice with the reason, the card stays, and `request_attention` fires again.
  If the cell is AFK, AFK's own deferral then happens as it does today.

### 4. What is deliberately left out

- No "deny" verdict, no "allow for session", no allowlist learned from past approvals.
- No change to Hermes' own timeout or to any backend translator.
- No API-key path: the reviewer uses Claude's own login, like `probe_usage`.

## Risks

- **A wrong approve.** Bounded by: hard rules first, no tools, raw command only, default escalate,
  allow-once only, every approval shown and logged. It still moves trust from the user to a model,
  which is why it is opt-in and off by default.
- **Prompt injection through the command or the user's last message.** The reviewer has no tools
  and one possible harmful output (approve); the prompt treats instructions in the data as a
  reason to escalate.
- **Cost.** One small Haiku call per reviewed card. The custom system prompt and no tools keep it
  to a few thousand tokens.

## Steps

1. `src/review.rs`: hard rules with unit tests (each rule, plus harmless commands that must pass
   through to the model).
2. `src/review.rs`: the `claude -p` call, with tests for timeout, bad output and non-approve
   output all giving escalate; confirm the CLAUDE.md point above.
3. `src/main.rs`: the `review_approval` command, registered beside `probe_usage`.
4. `ui/afk.js`, `ui/app.js`: the per-cell toggle, the AFK-menu checkbox, the 60 s countdown in
   `armPrompt`, review-before-defer when AFK is on, card state, notices, log entry, Codex cells
   excluded; a Node test in `tests/` for the timer and the "card already answered" race.
5. Live check with a Hermes session: a harmless command is approved unattended; `git push` and an
   edit outside the cwd stay as cards; a card the user answers during the review is left alone;
   a card replayed after an HQ restart is still reviewed.
6. README section and patch notes.

## Decisions (Brydon, 2026-11-05)

1. Enabled per session, or optionally when setting AFK. No HQ-wide setting.
2. The reviewer steps in only after 60 s, and the card shows the timer and bar for that minute.
3. Claude and Hermes sessions. Not built for Codex, whose support ends soon.

## Change log

- 2026-11-05: first draft. Not yet reviewed by a fresh context.
- 2026-11-05: per-session enablement, AFK-menu option, 60 s countdown on the card, Codex left out.
- 2026-11-05: built, status left as draft. Step 5 (live check) not run. Confirmed against claude
  2.1.318: `--setting-sources ""` keeps both `~/.claude/CLAUDE.md` and a `CLAUDE.md` in the working
  folder out. Where the build differs from the text above:
  - Size: Edit, Write and NotebookEdit may run to 200 lines or 16,000 characters instead of 4,000
    characters, and the model reads all of it. Longer ones escalate: the plan's "first 200 lines"
    would have let an approval write text the reviewer never saw (found by the fresh-context review).
  - `input.description` is dropped for Bash and PowerShell as well as the card's description: both
    Claude and Hermes carry the agent's words there. On other tools it may be the payload, so it stays.
  - The path rule covers every tool with a common path field, reads included.
  - Added hard rules: `mcp__*` tools and WebFetch; `dangerouslyDisableSandbox`; git config and
    `git -c`; several paths in one field.
  - On Hermes only plain single-file edits are reviewed; every other request stays a card, so the
    plan's "harmless command approved unattended" holds on Claude and not on Hermes. Hermes gives
    HQ the same options for a real command, a consent sentence (MCP tools, standing event triggers,
    saved payment cards), a Python script and a computer-use summary, so telling them apart would
    be a denylist tracking Hermes releases. Live, the one shell command that reached a card went to
    the user anyway, so this costs little. V4A patches are excluded too, as their listed paths cannot
    be trusted, and so are edits whose path the agent gave as relative, since Hermes shows one
    folder's file and writes another's after a `cd`. The mark is positive, so a lost field fails safe. This needed a small `hermes.rs` change, against the plan's "no change". Typing in the card's feedback box stops the approve.
  - HQ-wide AFK does not extend the reviewer to sessions an agent started or a command line opened,
    as whoever did so picks the folder the path rule trusts. The per-session toggle still works on
    them. A folder that is a drive root or holds the home folder has no "inside" at all.
  - Each call gets a new empty temp folder, removed afterwards, rather than a fixed `hq-review`.
  - Hermes sends whole files as an edit's old and new text, so Hermes edits of files over 200 lines
    stay with the user. This fails safe, but the plan's Hermes goal is only met for smaller files.
  - An approval outside AFK is a chat notice only, with no AFK log entry: an entry would raise a
    false "While you were away" card. So the Risks line "shown and logged" holds only while AFK;
    outside AFK an approval is shown, and that notice does not outlast a restart.
  - Installed Hermes' approval timeout defaults to 300 s, not the 7 to 10 minutes above.
  - Stricter than listed: any `git restore` or `git checkout --`, not only whole trees; cloud cells
    are excluded with Codex; `dbhub.toml` is on the name list; API-key variables are removed from
    the reviewer's environment so that it can only use Claude's login.
  - `.claude`, `.mcp.json`, `.git`, `.husky`, `.githooks`, `.vscode`, `.cargo/config`, `CLAUDE.md`
    and `AGENTS.md` are on the name list: allow rules, hooks and auto-loaded files inside the cwd
    would turn an allow-once into a lasting permission.
  - `format` and `reg` escalate only where they run as a command, so `dotnet format` and
    `grep reg` go to the model.
  - The last message comes from the composer only, and is lost on an HQ restart until the user
    next types.

- 2026-11-06: step 5 run on the dev copy with Hermes 0.22.2 and claude 2.1.318, driven over the
  debug port. Passed on Hermes (before shell commands were excluded from review): an edit inside the folder approved after the countdown; an edit
  outside it and `git reset --hard` left as cards with the right reasons; answering during the
  countdown and typing in the card both stop it; AFK with the option approves one, defers the
  other and lists both in the digest; a card waiting through a restart comes back and is approved.
  On Claude, the local settings let the test commands through with no card, so that branch was
  checked by injecting cards into a live cell: `cargo build` approved, `git push` left, with no
  `updatedPermissions`. Learned: this Hermes setup ran `rm -r` and `chmod 777` itself with no card,
  so few shell commands reach the reviewer there, and it helps mostly with edits. Two early cards
  were answered by a real click on Yes, not by the reviewer; about ten later cards had no such
  click, and no code clicks that button.

## Release note (draft, for `release.ps1 -Notes`)

- Adds the approval reviewer, off by default. Turn it on per session from the **More** menu, or
  while AFK from the AFK menu. A permission you leave for a minute goes to a separate Haiku call
  with no tools, which can approve it once or leave it for you, and never refuses. It uses your
  Claude plan. In Hermes sessions it only reviews plain file edits.
