'use strict';

// ======================================================================================
// HQ's MCP server: tools that let a session see, start and steer the others
// ======================================================================================
//
// mcp.rs runs the server and checks who is calling. Each tool call arrives here as an `mcp-call`
// event naming the calling process (0 for an agent outside HQ, through the machine-wide install),
// and is answered with `mcp_reply`. The tool list itself lives in mcp.rs.
//
// Guard rails: a session started by an agent can start one more level of sessions and no deeper,
// at most `mcpMaxSpawned` agent-started sessions are open at once (Settings), permissions can never
// be bypassed, and a session may only stop, close or reconfigure sessions it started (directly or
// through others). Starting, messaging and stopping sessions go through Claude's permission
// prompts like any other tool (mcp.rs allows only the harmless tools up front).

const MCP_PREFIX = '(HQ:';              // starts every message HQ sends between sessions
const MCP_MAX_DEPTH = 2;                // the user's sessions are depth 0; theirs 1; those sessions' 2
const MCP_DEFAULT_MAX_SPAWNED = 6;
const MCP_MAX_SPAWNED_CAP = 20;
const MCP_REPLY_MAX = 20000;            // characters of one reply kept for read_session
const MCP_REPLIES_KEPT = 10;
const MCP_NOTE_MAX = 20000;
const MCP_NOTES_MAX = 200;
const MCP_SCHEDULE_MAX = 50;
const MCP_SETTLE_MS = 3000;             // a session must stay idle this long to count as done (a queued message may start a new turn)
const MCP_POLL_MS = 1000;
const MCP_SCHEDULE_TICK_MS = 20 * 1000;
const MCP_ASK_TIMEOUT_MS = 30 * 60 * 1000;
const MCP_EXTERNAL = 'outside';         // the id of an agent that is not one of HQ's sessions

const mcp = {
  notes: {},          // key -> { value, by, at }: shared notes, saved in the board
  schedule: [],       // { id, to, from, text, at }: messages to send later, saved in the board
  claims: new Map(),  // path key -> { path, by, note, at }: advisory file claims, held while the claimant is open
  cancels: new Map(), // call id -> what ends a waiting call when its client goes away
  timer: 0,
};

class McpError extends Error {}
const fail = text => { throw new McpError(text); };

/** A short id for a cell, unique on the board: what the tools call a session. */
function newHqId() {
  for (;;) {
    const id = 's' + Math.random().toString(36).slice(2, 6);
    if (id.length === 5 && !app.cells.some(c => c.hqId === id)) return id;
  }
}

function mcpCell(id) {
  return app.cells.find(c => c.hqId === id) || null;
}

/** The session `ref` names, or a helpful error. */
function mcpTarget(ref, caller) {
  const id = String(ref || '').trim();
  if (!id) fail('Name a session by its id (see list_sessions).');
  const cell = mcpCell(id);
  if (!cell) fail(`No session "${id}" is open in HQ. Open sessions: ${app.cells.map(c => c.hqId).join(', ') || 'none'}.`);
  if (cell === caller) fail('That is your own session.');
  return cell;
}

/** Whether `caller` started `cell`, directly or through sessions it started. */
function mcpOwns(caller, cell) {
  const seen = new Set();
  for (let c = cell; c && c.spawnedBy && !seen.has(c); c = mcpCell(c.spawnedBy)) {
    seen.add(c);
    if (c.spawnedBy === caller.hqId) return true;
  }
  return false;
}

function mcpRequireOwned(caller, cell) {
  if (!mcpOwns(caller, cell)) fail(`Session ${cell.hqId} was not started by you, so only the user can do that.`);
}

function mcpName(cell) {
  return cell.displayTitle() || baseName(cell.cwd) || cell.hqId;
}

/** True while the session has work in hand that will end by itself. */
function mcpWorking(cell) {
  if (cell.missing || cell.pending.size || cell.status === 'exited') return false;
  return cell.status === 'running' || !!cell.spawning || !!cell.mcpExpect;
}

function mcpState(cell) {
  if (cell.missing) return 'folder missing';
  if (cell.pending.size) return 'waiting on the user';
  if (mcpWorking(cell)) return 'working';
  if (cell.status === 'exited') return 'stopped';
  if (cell.bgTasks.length) return 'idle, with background tasks running';
  if (!cell.sessionId && cell.proc == null) return 'new';
  return 'idle';
}

function mcpBrief(cell, caller) {
  const info = { id: cell.hqId, agent: cell.backend, folder: cell.cwd, title: cell.displayTitle() || null, state: mcpState(cell) };
  if (cell.sideTaskLine) info.tasks = cell.sideTaskLine;
  if (cell.spawnedBy) info.startedBy = cell.spawnedBy;
  if (cell.collapsed) info.collapsed = true;
  if (cell === caller) info.you = true;
  return info;
}

function mcpLatest(cell, n) {
  const replies = cell.replyLog.slice(-n);
  return replies.length ? replies : cell.lastReply ? [cell.lastReply] : [];
}

/** Sends HQ's own prompt to a cell and marks it as owing an answer until its turn ends. */
function mcpSend(cell, text) {
  cell.mcpExpect = true;
  cell.submit(text);
}

/** Called by the cell when a turn ends or its process stops. */
function mcpTurnEnded(cell) {
  cell.mcpExpect = false;
}

/** Called as a cell closes: its claims go, and so does what it scheduled for itself. */
function mcpCellClosed(cell) {
  for (const [key, claim] of mcp.claims) if (claim.by === cell.hqId) mcp.claims.delete(key);
  const before = mcp.schedule.length;
  mcp.schedule = mcp.schedule.filter(s => s.to !== cell.hqId);
  if (mcp.schedule.length !== before) saveState();
}

/** Keeps the text of a finished reply for read_session. */
function mcpNoteReply(cell, text) {
  if (!text) return;
  cell.replyLog.push(text.length > MCP_REPLY_MAX ? text.slice(0, MCP_REPLY_MAX) + '\n… (cut)' : text);
  if (cell.replyLog.length > MCP_REPLIES_KEPT) cell.replyLog.shift();
}

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const isoTime = ms => (ms ? new Date(ms).toISOString() : null);

function mcpString(v, name, max = 100000) {
  if (typeof v !== 'string' || !v.trim()) fail(`${name} is required.`);
  if (v.length > max) fail(`${name} is too long (at most ${max} characters).`);
  return v;
}

/** A path as a claim key: absolute, one kind of slash, no trailing slash, lower case (Windows and macOS ignore case). */
function mcpPathKey(path, base) {
  let p = String(path).trim().replace(/\\/g, '/');
  if (!/^([a-z]:)?\//i.test(p) && base) p = `${base.replace(/\\/g, '/').replace(/\/+$/, '')}/${p}`;
  const parts = [];
  for (const part of p.split('/')) {
    if (part === '..') parts.pop();
    else if (part !== '.' && (part !== '' || !parts.length)) parts.push(part);
  }
  return parts.join('/').replace(/\/+$/, '').toLowerCase();
}

/** One path inside or equal to the other. */
function mcpOverlap(a, b) {
  return a === b || a.startsWith(b + '/') || b.startsWith(a + '/');
}

// ---------- the tools ----------

const MCP_TOOLS = {
  list_sessions(caller) {
    return { you: caller.hqId, sessions: app.cells.map(c => mcpBrief(c, caller)) };
  },

  async spawn_session(caller, a) {
    if (app.settings.mcpSpawn === false) fail('The user has turned off starting sessions from agents in HQ\'s settings.');
    const depth = caller.depth + 1;
    if (depth > MCP_MAX_DEPTH) fail(`Sessions started by other sessions cannot start more (at most ${MCP_MAX_DEPTH} levels). Do this part yourself, or tell the session that started you.`);
    const max = app.settings.mcpMaxSpawned;
    const open = app.cells.filter(c => c.spawnedBy).length;
    if (open >= max) fail(`HQ already has ${open} sessions started by agents, the most the user allows (Settings). Close one you no longer need with close_session, or wait.`);
    const prompt = mcpString(a.prompt, 'prompt');
    const agent = a.agent == null ? 'claude' : String(a.agent);
    if (!availableBackends().includes(agent)) fail(`${AGENT_NAMES[agent] || agent} is not available in this HQ. Available: ${availableBackends().join(', ')}.`);
    let folder = a.folder == null ? caller.cwd : String(a.folder).trim();
    if (!folder) fail('folder is required for an agent outside HQ.');
    if (!(await invoke('folder_exists', { path: folder }).catch(() => false))) fail(`Folder does not exist: ${folder}`);
    if (a.worktree != null && String(a.worktree).trim()) {
      try {
        folder = await invoke('create_worktree', { folder, branch: String(a.worktree).trim() });
      } catch (err) {
        fail(String(err));
      }
    }
    if (a.effort != null && !ALL_EFFORTS.includes(a.effort)) fail(`effort must be one of ${ALL_EFFORTS.join(', ')}.`);
    const opts = { cwd: folder, backend: agent, spawnedBy: caller.hqId, depth, collapsed: a.collapsed === true };
    if (typeof a.model === 'string' && a.model.trim()) opts.model = a.model.trim();
    if (a.effort) opts.effort = a.effort;
    if (agent === 'claude') {
      // Never bypassPermissions: an agent must not lift the prompts for another.
      const mode = a.permission_mode || caller.permissionMode;
      opts.permissionMode = LAUNCH_MODES.includes(mode) ? mode : 'default';
    }
    if (typeof a.name === 'string' && a.name.trim()) opts.customTitle = a.name.trim().slice(0, 80);
    const cell = addNewCell(opts, { background: true });
    const from = caller.external ? 'an agent outside HQ' : `session "${mcpName(caller)}" (${caller.hqId})`;
    mcpSend(cell, `${MCP_PREFIX} you were started by ${from}, which reads your final reply. You are session ${cell.hqId}; HQ's tools (mcp__hq__*) let you coordinate with other sessions.)\n\n${prompt}`);
    return { session: cell.hqId, agent, folder, note: 'Started. Call wait_for_sessions, then read_session, to get its result.' };
  },

  send_message(caller, a) {
    const cell = mcpTarget(a.session, caller);
    if (cell.missing) fail(`Session ${cell.hqId}'s folder is missing, so it cannot run.`);
    const text = mcpString(a.message, 'message');
    const from = caller.external ? 'an agent outside HQ' : `session "${mcpName(caller)}" (${caller.hqId})`;
    const state = mcpState(cell);
    mcpSend(cell, `${MCP_PREFIX} message from ${from}, which can read your reply.)\n\n${text}`);
    return `Sent to ${cell.hqId} (${state}${state === 'working' ? '; it reads the message at its next step' : ''}). Use wait_for_sessions, then read_session, for its answer.`;
  },

  read_session(caller, a) {
    const cell = mcpTarget(a.session, caller);
    const n = Math.min(MCP_REPLIES_KEPT, Math.max(1, Math.round(Number(a.replies) || 1)));
    const info = mcpBrief(cell, caller);
    info.model = cell.resolvedModel || cell.modelChoice || 'default';
    const tasks = cell.taskItems().filter(t => t && t.subject);
    if (tasks.length) info.taskList = tasks.map(t => ({ task: t.subject, status: t.status || 'pending' }));
    if (cell.currentTool && mcpWorking(cell)) info.currentTool = cell.currentTool.name;
    info.replies = mcpLatest(cell, n);
    return info;
  },

  async wait_for_sessions(caller, a, callId) {
    const ids = Array.isArray(a.sessions) ? a.sessions : [a.sessions];
    if (!ids.length || !ids[0]) fail('Name at least one session.');
    const cells = [...new Set(ids.map(id => mcpTarget(id, caller)))];
    const any = a.mode === 'any';
    const timeout = Math.min(3600, Math.max(1, Number(a.timeout_seconds) || 600)) * 1000;
    const started = Date.now();
    let settledAt = 0;
    let cancelled = false;
    let timedOut = false;
    mcp.cancels.set(callId, () => { cancelled = true; });
    try {
      for (;;) {
        const working = cells.filter(c => app.cells.includes(c) && mcpWorking(c));
        const done = any ? working.length < cells.length : !working.length;
        settledAt = done ? settledAt || Date.now() : 0;
        if (done && Date.now() - settledAt >= MCP_SETTLE_MS) break;
        if (cancelled) return 'Cancelled.';
        if (Date.now() - started > timeout) { timedOut = true; break; }
        await sleep(MCP_POLL_MS);
      }
    } finally {
      mcp.cancels.delete(callId);
    }
    const sessions = cells.map(c => {
      if (!app.cells.includes(c)) return { id: c.hqId, state: 'closed' };
      const info = mcpBrief(c, caller);
      const [reply] = mcpLatest(c, 1);
      if (reply) info.lastReply = reply;
      return info;
    });
    return { timedOut, sessions };
  },

  interrupt_session(caller, a) {
    const cell = mcpTarget(a.session, caller);
    mcpRequireOwned(caller, cell);
    if (!mcpWorking(cell) && !cell.pending.size) return `${cell.hqId} is not working.`;
    cell.mcpExpect = false;
    cell.interrupt();
    return `Asked ${cell.hqId} to stop.`;
  },

  close_session(caller, a) {
    const cell = mcpTarget(a.session, caller);
    mcpRequireOwned(caller, cell);
    closeCell(cell);
    renderSidebar();
    return `Closed ${cell.hqId}. Its conversation stays on disk; the user can resume it.`;
  },

  async set_session_model(caller, a) {
    const cell = mcpTarget(a.session, caller);
    mcpRequireOwned(caller, cell);
    if (a.model == null && a.effort == null) fail('Give a model, an effort, or both.');
    if (a.effort != null && !ALL_EFFORTS.includes(a.effort)) fail(`effort must be one of ${ALL_EFFORTS.join(', ')}.`);
    if (a.model != null) await cell.setModel(String(a.model));
    if (a.effort != null) await cell.setEffort(a.effort);
    return `${cell.hqId} now runs ${cell.modelLabel()}.`;
  },

  collapse_session(caller, a) {
    const cell = mcpTarget(a.session, caller);
    mcpRequireOwned(caller, cell);
    const on = a.collapsed !== false;
    if (on) collapseCell(cell, true);
    else cell.setCollapsed(false); // opened without taking the user's focus
    return `${cell.hqId} is ${on ? 'collapsed' : 'open'}.`;
  },

  schedule_message(caller, a) {
    const text = mcpString(a.message, 'message', MCP_NOTE_MAX);
    const target = a.session == null ? caller : mcpCell(String(a.session));
    if (!target || target.external) fail(a.session == null ? 'Name the session to send it to.' : `No session "${a.session}" is open in HQ.`);
    let at = 0;
    if (a.after_usage_reset) {
      at = planLimitedUntil() || usageResetAt();
      if (!at) fail('None of the plan\'s limits is used up now, so there is no reset to wait for. Use delay_minutes or at instead.');
      at += 60 * 1000; // a minute's grace, so the limit has really lifted
    } else if (a.delay_minutes != null) {
      const m = Number(a.delay_minutes);
      if (!(m >= 1)) fail('delay_minutes must be at least 1.');
      at = Date.now() + m * 60 * 1000;
    } else if (a.at != null) {
      at = Date.parse(String(a.at));
      if (!Number.isFinite(at)) fail('at must be an ISO 8601 time, like 2026-10-29T17:30:00Z.');
      if (at <= Date.now()) fail('at is in the past.');
    } else {
      fail('Give delay_minutes, at, or after_usage_reset.');
    }
    if (mcp.schedule.length >= MCP_SCHEDULE_MAX) fail(`${MCP_SCHEDULE_MAX} messages are already scheduled. Cancel some first.`);
    const id = 'm' + Math.random().toString(36).slice(2, 7);
    mcp.schedule.push({ id, to: target.hqId, from: caller.hqId, text, at });
    saveState();
    return { id, session: target.hqId, sendsAt: isoTime(at) };
  },

  list_scheduled() {
    return { scheduled: mcp.schedule.map(s => ({ id: s.id, session: s.to, from: s.from, sendsAt: isoTime(s.at), message: firstLine(s.text, 200) })) };
  },

  cancel_scheduled(caller, a) {
    const before = mcp.schedule.length;
    mcp.schedule = mcp.schedule.filter(s => s.id !== a.id);
    if (mcp.schedule.length === before) fail(`No scheduled message "${a.id}".`);
    saveState();
    return `Cancelled ${a.id}.`;
  },

  notify_user(caller, a) {
    const text = mcpString(a.message, 'message', 2000);
    if (!app.settings.notify) return 'Notifications are off in HQ\'s settings, so nothing was shown.';
    if (caller.external) invoke('plugin:notification|notify', { options: { title: 'HQ', body: text.slice(0, 300) } }).catch(() => {});
    else notify(caller, text.slice(0, 300));
    invoke('request_attention').catch(() => {});
    return 'Shown.';
  },

  ask_user(caller, a, callId) {
    if (caller.external) fail('ask_user works only in sessions inside HQ.');
    const question = mcpString(a.question, 'question', 4000);
    if (caller.afkOn) return 'The user is away (AFK) and cannot answer now. Go with what you would recommend, and say so in your reply.';
    const options = (Array.isArray(a.options) ? a.options : []).filter(o => typeof o === 'string' && o.trim()).slice(0, 9);
    const timeout = Math.min(3600, Math.max(10, Number(a.timeout_seconds) || MCP_ASK_TIMEOUT_MS / 1000)) * 1000;
    const requestId = `mcp-ask-${callId}`;
    return new Promise(resolve => {
      let card = null;
      const finish = text => {
        if (!card) return;
        const c = card;
        card = null;
        clearTimeout(timer);
        mcp.cancels.delete(callId);
        c.onDrop = null;
        caller.dropPrompt(requestId);
        resolve(text);
      };
      const timer = setTimeout(() => finish('The user did not answer in time.'), timeout);
      const other = h('input', { type: 'text', placeholder: options.length ? 'Other…' : 'Your answer, then Enter' });
      other.addEventListener('keydown', e => {
        if (e.key === 'Enter' && other.value.trim()) { e.preventDefault(); finish(`The user answered: ${other.value.trim()}`); }
      });
      const choices = options.map((o, i) => choice(String(i + 1), o, null, () => finish(`The user answered: ${o}`)));
      card = h('div', { class: 'prompt' },
        h('div', { class: 'prompt-title', text: question }),
        choices.length ? h('div', { class: 'choices' }, choices) : null,
        other,
        h('div', { class: 'actions' }, h('button', { class: 'btn', onclick: () => finish('The user declined to answer.') }, 'Skip')));
      card.hotkeys = choices.map(c => c.run);
      card.dataset.kind = 'question';
      card.onDrop = () => finish('The question was withdrawn (the turn ended).');
      mcp.cancels.set(callId, () => finish('Cancelled.'));
      caller.pending.set(requestId, card);
      caller.prompts.append(card);
      caller.syncPromptsBar();
      caller.unread = false;
      caller.refreshState();
      invoke('request_attention').catch(() => {});
      notifyWaiting(caller, { display_name: 'a question' });
    });
  },

  async get_usage() {
    if (!usage.data || Date.now() - usage.fetchedAt > USAGE_STALE_MS) await withTimeout(refreshUsage(), 20000);
    if (!usage.data) fail(`HQ could not read the plan's usage${usage.error ? `: ${usage.error}` : ''}.`);
    const out = planLimitedUntil();
    return {
      plan: usage.data.plan,
      limits: usage.data.rows.map(r => ({ limit: r.label, percentUsed: r.percent, resetsAt: isoTime(r.resetsAt) })),
      outOfUsageUntil: isoTime(out),
      readAt: isoTime(usage.fetchedAt),
    };
  },

  claim_files(caller, a) {
    const paths = (Array.isArray(a.paths) ? a.paths : []).filter(p => typeof p === 'string' && p.trim());
    if (!paths.length) fail('Give at least one path.');
    const note = typeof a.note === 'string' ? a.note.slice(0, 300) : '';
    // A claim whose holder has closed is gone.
    for (const [key, claim] of mcp.claims) if (claim.by !== MCP_EXTERNAL && !mcpCell(claim.by)) mcp.claims.delete(key);
    const wanted = paths.map(p => ({ path: p, key: mcpPathKey(p, caller.cwd) }));
    const clashes = [];
    for (const w of wanted) {
      for (const claim of mcp.claims.values()) {
        if (claim.by !== caller.hqId && mcpOverlap(w.key, claim.key)) {
          const holder = mcpCell(claim.by);
          clashes.push(`${w.path}: held by ${claim.by}${holder ? ` ("${mcpName(holder)}")` : ''} as ${claim.path}${claim.note ? ` (${claim.note})` : ''}`);
        }
      }
    }
    if (clashes.length) fail(`Nothing was claimed; other sessions hold some of these:\n${clashes.join('\n')}`);
    for (const w of wanted) mcp.claims.set(w.key, { key: w.key, path: w.path, by: caller.hqId, note, at: Date.now() });
    return `Claimed ${wanted.length} path${wanted.length === 1 ? '' : 's'}. Release them with release_files when done.`;
  },

  release_files(caller, a) {
    const paths = (Array.isArray(a.paths) ? a.paths : []).filter(p => typeof p === 'string' && p.trim());
    const keys = new Set(paths.map(p => mcpPathKey(p, caller.cwd)));
    let n = 0;
    for (const [key, claim] of mcp.claims) {
      if (claim.by === caller.hqId && (!keys.size || keys.has(key))) {
        mcp.claims.delete(key);
        n++;
      }
    }
    return `Released ${n} claim${n === 1 ? '' : 's'}.`;
  },

  list_claims() {
    return { claims: [...mcp.claims.values()].map(c => ({ path: c.path, by: c.by, note: c.note || undefined, since: isoTime(c.at) })) };
  },

  note_set(caller, a) {
    const key = mcpString(a.key, 'key', 100).trim();
    const value = typeof a.value === 'string' ? a.value : fail('value is required.');
    if (value.length > MCP_NOTE_MAX) fail(`value is too long (at most ${MCP_NOTE_MAX} characters).`);
    if (!(key in mcp.notes) && Object.keys(mcp.notes).length >= MCP_NOTES_MAX) fail(`There are already ${MCP_NOTES_MAX} notes. Delete some first.`);
    mcp.notes[key] = { value, by: caller.hqId, at: Date.now() };
    saveState();
    return `Saved "${key}".`;
  },

  note_get(caller, a) {
    if (a.key == null || a.key === '') {
      return { notes: Object.entries(mcp.notes).map(([key, n]) => ({ key, by: n.by, at: isoTime(n.at), length: n.value.length })) };
    }
    const n = mcp.notes[String(a.key).trim()];
    if (!n) fail(`No note "${a.key}". Call note_get without a key to list them.`);
    return { key: String(a.key).trim(), value: n.value, by: n.by, at: isoTime(n.at) };
  },

  note_delete(caller, a) {
    const key = String(a.key || '').trim();
    if (!(key in mcp.notes)) fail(`No note "${key}".`);
    delete mcp.notes[key];
    saveState();
    return `Deleted "${key}".`;
  },
};

/** An agent outside HQ (the machine-wide install) acts as a caller of its own, with no cell. */
const mcpExternalCaller = { hqId: MCP_EXTERNAL, external: true, depth: 0, cwd: '', permissionMode: 'default', afkOn: false };

async function onMcpCall({ payload }) {
  const { id, proc, tool, args } = payload || {};
  let text;
  let isError = false;
  try {
    if (app.settings.mcp === false) fail('The user has turned HQ\'s tools off in HQ\'s settings.');
    if (proc === 0 && !app.settings.mcpExternal) fail('HQ\'s tools are not turned on for agents outside HQ. The user can install them machine-wide in HQ\'s settings.');
    const caller = proc === 0 ? mcpExternalCaller : app.cells.find(c => c.proc === proc);
    if (!caller) fail('Your session is no longer open in HQ.');
    const handler = MCP_TOOLS[tool];
    if (!handler) fail(`HQ does not know the tool ${tool}.`);
    const result = await handler(caller, args && typeof args === 'object' ? args : {}, id);
    text = typeof result === 'string' ? result : JSON.stringify(result, null, 2);
  } catch (err) {
    isError = true;
    text = err instanceof McpError ? err.message : `HQ hit an error: ${err && err.message || err}`;
    if (!(err instanceof McpError)) console.error('mcp tool failed', tool, err);
  }
  invoke('mcp_reply', { id, text, isError }).catch(() => {});
}

/** Sends the scheduled messages that are due. */
function mcpTick() {
  const now = Date.now();
  const due = mcp.schedule.filter(s => s.at <= now);
  if (!due.length) return;
  mcp.schedule = mcp.schedule.filter(s => s.at > now);
  for (const s of due) {
    const cell = mcpCell(s.to);
    if (!cell || cell.missing) continue;
    const from = s.from === s.to ? 'you scheduled earlier' : s.from === MCP_EXTERNAL ? 'an agent outside HQ scheduled' : `session ${s.from} scheduled`;
    mcpSend(cell, `${MCP_PREFIX} a message ${from}.)\n\n${s.text}`);
  }
  saveState();
}

function mcpSaved() {
  const notes = Object.keys(mcp.notes).length ? mcp.notes : undefined;
  const schedule = mcp.schedule.length ? mcp.schedule : undefined;
  return notes || schedule ? { notes, schedule } : undefined;
}

/** Restores the notes and schedule from the board, keeping only well-formed entries. */
function mcpRestore(saved) {
  const s = saved && typeof saved === 'object' ? saved : {};
  mcp.notes = {};
  for (const [key, n] of Object.entries(s.notes && typeof s.notes === 'object' ? s.notes : {}).slice(0, MCP_NOTES_MAX)) {
    if (n && typeof n.value === 'string') mcp.notes[key.slice(0, 100)] = { value: n.value.slice(0, MCP_NOTE_MAX), by: String(n.by || ''), at: Number(n.at) || 0 };
  }
  mcp.schedule = (Array.isArray(s.schedule) ? s.schedule : [])
    .filter(x => x && typeof x.id === 'string' && typeof x.to === 'string' && typeof x.text === 'string' && Number.isFinite(x.at))
    .slice(0, MCP_SCHEDULE_MAX)
    .map(x => ({ id: x.id, to: x.to, from: String(x.from || ''), text: x.text, at: x.at }));
  if (!mcp.timer) mcp.timer = setInterval(mcpTick, MCP_SCHEDULE_TICK_MS);
}

window.__TAURI__.event.listen('mcp-call', onMcpCall).catch(err => console.error('mcp listener failed', err));
window.__TAURI__.event.listen('mcp-cancel', ({ payload }) => {
  const cancel = mcp.cancels.get(payload && payload.id);
  if (cancel) cancel();
}).catch(() => {});
