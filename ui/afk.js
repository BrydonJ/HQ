'use strict';

// ======================================================================================
// AFK mode
// ======================================================================================
//
// "I'm away until X: do what you can without me." Set for all of HQ from the top bar, or for one
// session from its header, which can also keep a session out of it. While a session is AFK:
// - A session that is working is told at once. Claude reads a message sent during a turn at its
//   next tool call and Codex is steered with it; Hermes can only read it once its turn ends.
// - A question is answered with "go with what you would recommend", a plan is held, and any other
//   permission is declined as deferred (not refused). Each waits AFK_GRACE_MS first, in case the
//   user is still there. Only Claude reads the reason given with a refusal; Codex and Hermes learn
//   from the note above that a refusal now means "later".
// - A turn that ends is nudged to carry on, until a nudge is answered without any tool call.
// - A session that hits the plan's usage limit carries on once usage is back.
// Each thing HQ did is noted in the chat, and listed in a card once the session is no longer AFK.
// Every agent's prompts arrive as Claude's can_use_tool (codex.rs and hermes.rs translate them),
// so nothing here depends on the backend except where noted.

const AFK_PREFIX = '(HQ: AFK';           // starts every message HQ sends for AFK, so a transcript can leave them out
const AFK_GRACE_MS = 60 * 1000;          // a prompt is left this long for the user first
const AFK_NUDGE_DELAY_MS = 3000;         // a queued message can start the next turn by itself in this time
const AFK_MAX_NUDGES = 5;                // per AFK period, so a session that keeps finding work does not run on unchecked
const AFK_TICK_MS = 15 * 1000;
const AFK_LOG_MAX = 100;
const AFK_DURATIONS = [[30, '30 minutes'], [60, '1 hour'], [120, '2 hours'], [240, '4 hours'], [480, '8 hours']];
let afkTimer = 0;

/** "3:45 PM", "3:45 PM tomorrow", or the date too when it is further off. */
function afkClock(ms) {
  const d = new Date(ms);
  const time = d.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
  const now = new Date();
  if (d.toDateString() === now.toDateString()) return time;
  const tomorrow = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 1);
  if (d.toDateString() === tomorrow.toDateString()) return `${time} tomorrow`;
  return `${d.toLocaleDateString([], { weekday: 'short', day: 'numeric', month: 'short' })} ${time}`;
}

/** How the agent is told how long the user is away. */
function afkWhen(until) {
  return until === Infinity ? 'until they are back (no set time)' : `until about ${afkClock(until)}`;
}

function afkNoteText(until) {
  return `${AFK_PREFIX} mode.) The user has gone AFK ${afkWhen(until)} and can't answer questions or approve anything before then. `
    + 'Keep going with whatever you can do without them. Don\'t ask questions: make the call you would recommend, note the assumption, and carry on. '
    + 'If an approval is declined while they are away, it is deferred, not refused: don\'t look for another way to do the same thing. '
    + 'Note it as pending and carry on with work that doesn\'t depend on it. When nothing else can proceed, stop and list what is waiting for them.';
}

function afkNudgeText(until) {
  return `${AFK_PREFIX} mode.) Your turn ended while the user is still away ${afkWhen(until)}. If there is more you can do without them, carry on. `
    + 'If you stopped to ask them something, make the call you would recommend, note it, and continue. '
    + 'If everything left needs them, reply with a short list of what is waiting for them and stop.';
}

const AFK_BACK_TEXT = `${AFK_PREFIX} over.) The user is back. Ask for approvals and decisions as usual again.`;

function afkDeferText(until) {
  return `The user is AFK ${afkWhen(until)} and can't approve this right now. This is not a no: it is deferred until they are back. `
    + 'Don\'t look for another way to do the same thing. Note it as pending, carry on with anything that doesn\'t depend on it, '
    + 'and if nothing else can proceed, stop and list what is waiting for them.';
}

function afkPlanText(until) {
  return `The user is AFK ${afkWhen(until)} and can't review this plan. Don't start implementing it and don't propose it again. `
    + 'Do any read-only investigation that would sharpen it, then stop; they will review it when they are back.';
}

function afkAnswerText(until) {
  const back = until === Infinity ? 'for a while' : `until about ${afkClock(until)}`;
  return `I'm AFK ${back} and can't answer. Go with the option you would recommend, note the assumption so I can review it, and carry on.`;
}

// ---------- state ----------

/** A saved AFK setting: { until (ms, or null for no set time), since }, or null. */
function sanitizeAfk(a) {
  if (!a || typeof a !== 'object' || !(a.until == null || Number.isFinite(a.until))) return null;
  return { until: a.until ?? null, since: Number.isFinite(a.since) ? a.since : Date.now() };
}

/** A cell's own setting: null follows HQ, false keeps it out of AFK, otherwise its own AFK. */
function sanitizeCellAfk(a) {
  return a === false ? false : sanitizeAfk(a);
}

function sanitizeAfkLog(log) {
  if (!Array.isArray(log)) return [];
  return log.filter(e => e && Number.isFinite(e.at) && typeof e.kind === 'string' && typeof e.text === 'string')
    .slice(-AFK_LOG_MAX).map(({ at, kind, text }) => ({ at, kind, text }));
}

/** When the cell's AFK ends: Infinity with no set time, 0 when it is not AFK. */
function afkUntil(cell) {
  if (cell.afk === false || cell.missing) return 0;
  const a = cell.afk || app.afk;
  if (!a) return 0;
  return a.until == null ? Infinity : a.until > Date.now() ? a.until : 0;
}

/** `until`: ms, Infinity for no set time, 0 to end it. `cell` null: all of HQ. */
function setAfk(cell, until) {
  const value = until ? { until: until === Infinity ? null : until, since: Date.now() } : null;
  if (cell) cell.afk = value;
  else app.afk = value;
  syncAfk();
  saveState();
}

/** Keeps a session out of HQ's AFK (true), or lets it follow HQ again (false). */
function setAfkOptOut(cell, out) {
  cell.afk = out ? false : null;
  syncAfk();
  saveState();
}

/** Starts or ends AFK in each cell as the settings and the clock say; runs on a timer while any is set. */
function syncAfk() {
  const now = Date.now();
  let changed = false;
  if (app.afk && app.afk.until != null && app.afk.until <= now) { app.afk = null; changed = true; }
  for (const cell of app.cells) {
    if (cell.afk && cell.afk.until != null && cell.afk.until <= now) { cell.afk = null; changed = true; }
    const on = afkUntil(cell) > 0;
    if (on && !cell.afkOn) afkStart(cell);
    else if (!on && cell.afkOn) afkEnd(cell);
    else if (!on && cell.afkLog.length && !cell.afkDigested) afkDigest(cell); // ended while HQ was closed
    cell.renderAfk();
  }
  const any = !!app.afk || app.cells.some(c => c.afk);
  if (any && !afkTimer) afkTimer = setInterval(syncAfk, AFK_TICK_MS);
  else if (!any && afkTimer) { clearInterval(afkTimer); afkTimer = 0; }
  renderAfkPill();
  if (changed) saveState();
}

function afkStart(cell) {
  cell.afkOn = true;
  cell.afkDigested = false;
  cell.afkNoted = false;
  cell.afkNudges = 0;
  cell.afkNudged = false;
  cell.afkQueued = false;
  cell.afkPlanHeld = false;
  cell.afkNote = null;
  if (!cell.afkLog.length) cell.afkSince = Date.now();
  if (cell.status === 'running' && cell.proc != null) afkSendNote(cell);
  // Prompts already waiting are answered at once: the user chose to leave them.
  for (const [requestId, card] of [...cell.pending]) afkHandle(cell, requestId, card);
  if (cell.backend === 'claude' && cell.limitHit && !cell.limitResume && cell.limitAutoContinue) cell.setLimitResume(true); // not a chat the user unchecked
}

function afkEnd(cell) {
  cell.afkOn = false;
  for (const card of cell.pending.values()) afkUnwatch(card);
  clearTimeout(cell.afkNudgeTimer);
  // Only to a turn that can read it now: a Hermes turn would only read it when it ends, and a new
  // turn just to say so costs a request (the user's next message says it anyway), unless the AFK
  // note itself is still queued there, which would otherwise arrive after the user is back.
  if (cell.afkNoted && cell.status === 'running' && cell.proc != null && (cell.backend !== 'hermes' || cell.afkQueued)) {
    cell.notice(`AFK over: told ${cell.agent} you are back.`);
    cell.submit(AFK_BACK_TEXT);
  }
  afkDigest(cell);
}

/** Resolves once the note is written to the agent (submit waits for the transcript and the process). */
function afkSendNote(cell) {
  const until = afkUntil(cell);
  cell.afkNoted = true;
  // Hermes reads it only once the turn ends, and it then starts the next turn: that is the nudge.
  if (cell.backend === 'hermes' && cell.status === 'running') cell.afkQueued = true;
  cell.notice(`AFK: told ${cell.agent} you are away ${until === Infinity ? 'until you are back' : `until about ${afkClock(until)}`}.`);
  cell.afkNote = cell.submit(afkNoteText(until));
  return cell.afkNote;
}

function afkRecord(cell, kind, text) {
  cell.afkLog.push({ at: Date.now(), kind, text });
  if (cell.afkLog.length > AFK_LOG_MAX) cell.afkLog.splice(0, cell.afkLog.length - AFK_LOG_MAX);
  saveState();
}

// ---------- prompts ----------

const AFK_ACTIONS = { question: 'Answering for you', plan: 'Holding the plan', permission: 'Deferring' };

/** A prompt arrived (onControlRequest): answered after the grace period if the user has not. The card counts it down. */
function afkWatch(cell, requestId, card) {
  if (!cell.afkOn) return;
  const due = Date.now() + AFK_GRACE_MS;
  const text = h('span', { class: 'afk-count-text' });
  const bar = h('div', { class: 'afk-count-bar' });
  bar.style.animationDuration = `${AFK_GRACE_MS}ms`;
  card.afkCount = h('div', { class: 'afk-count', title: 'You are AFK. Answer it yourself before then to keep it.' },
    h('div', { class: 'afk-count-row' }, icon('watch'), text), h('div', { class: 'afk-count-track' }, bar));
  const tick = () => {
    const s = Math.max(0, Math.ceil((due - Date.now()) / 1000));
    text.textContent = `AFK: ${AFK_ACTIONS[card.dataset.kind] || 'Deferring'} in ${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  };
  tick();
  card.afkTick = setInterval(tick, 1000);
  card.prepend(card.afkCount);
  card.afkTimer = setTimeout(() => afkHandle(cell, requestId, card), AFK_GRACE_MS);
}

/** Stops a prompt's AFK countdown: it was answered, AFK ended, or HQ answers it now. */
function afkUnwatch(card) {
  clearTimeout(card.afkTimer);
  clearInterval(card.afkTick);
  if (card.afkCount) card.afkCount.remove();
  card.afkCount = null;
}

/** Answers a waiting prompt for the away user: a question with "your call", a plan held, anything else deferred. */
async function afkHandle(cell, requestId, card) {
  afkUnwatch(card);
  const live = () => afkUntil(cell) && cell.pending.get(requestId) === card && !cell.closing;
  if (!live()) return;
  // Claude reads the reason given below; Codex and Hermes only the note, which must reach them first.
  if (cell.backend !== 'claude') {
    if (!cell.afkNoted) afkSendNote(cell);
    await cell.afkNote;
    if (!live()) return; // the user answered meanwhile
  }
  const until = afkUntil(cell);
  const req = card.afkReq || {};
  const input = req.input || {};
  if (req.tool_name === 'AskUserQuestion') {
    const questions = Array.isArray(input.questions) ? input.questions : [];
    const answers = {};
    for (const q of questions) answers[q.question] = afkAnswerText(until);
    cell.answer(requestId, { behavior: 'allow', updatedInput: { ...input, answers } });
    const asked = questions.map(q => q.question).filter(Boolean).join(' / ') || 'a question';
    cell.notice(`AFK: answered "${firstLine(asked, 120)}" with "use your judgment".`);
    afkRecord(cell, 'question', asked);
  } else if (req.tool_name === 'ExitPlanMode') {
    cell.answer(requestId, { behavior: 'deny', message: afkPlanText(until) });
    cell.afkPlanHeld = true;
    cell.notice('AFK: held the plan for you to review.');
    afkRecord(cell, 'plan', firstLine(String(input.plan || ''), 120) || 'Plan');
  } else {
    cell.answer(requestId, { behavior: 'deny', message: afkDeferText(until) });
    const what = afkToolText(cell, req);
    cell.notice(`AFK: deferred ${what}.`);
    afkRecord(cell, 'permission', what);
  }
}

/** "Bash: git push origin main", "Edit: src/app.rs": what a permission asked for, in one line. */
function afkToolText(cell, req) {
  const input = req.input || {};
  const name = req.display_name || req.tool_name || 'a tool';
  const detail = input.command || (input.file_path && relPath(input.file_path, cell.cwd)) || req.description || '';
  return detail ? `${name}: ${firstLine(String(detail), 140)}` : name;
}

// ---------- turns ----------

/** A turn ended (onResult): while the user is away, the agent is asked to carry on. */
function afkTurnEnded(cell, m, local) {
  if (!cell.afkOn || local || cell.closing) return;
  if (cell.afkQueued) {
    // Hermes now reads the note it was sent during the turn, which starts the next turn.
    cell.afkQueued = false;
    cell.afkNudged = true;
    cell.afkNudges++;
    return;
  }
  if (cell.limitHit || cell.limitResume) return; // carries on once usage is back (checkLimitResumes)
  if (m.is_error || m.subtype !== 'success') return; // stopped or failed: left for the user
  if (cell.afkPlanHeld) return; // a held plan waits for the user; carrying on would only propose it again
  if (cell.bgTasks.length) return; // the agent goes on by itself when they finish
  if (cell.afkNudged && !cell.turnTools) return; // the last nudge found nothing left to do
  if (cell.afkNudges >= AFK_MAX_NUDGES) {
    if (cell.afkNudges++ === AFK_MAX_NUDGES) cell.notice(`AFK: ${cell.agent} was asked to carry on ${AFK_MAX_NUDGES} times; it now waits for you.`);
    return;
  }
  clearTimeout(cell.afkNudgeTimer);
  cell.afkNudgeTimer = setTimeout(() => {
    // A message sent during the turn may have started the next one, or the user took over.
    if (!cell.afkOn || cell.closing || !app.cells.includes(cell)) return;
    if (cell.status === 'running' || cell.pending.size || cell.spawning || cell.proc == null || cell.limitResume) return;
    cell.afkNudges++;
    cell.afkNudged = true;
    cell.notice(`AFK: asked ${cell.agent} to carry on.`);
    afkRecord(cell, 'nudge', firstLine(cell.lastReply || '', 140));
    cell.submit(afkNudgeText(afkUntil(cell)));
  }, AFK_NUDGE_DELAY_MS);
}

/** The user sent a message themselves: the next turn is theirs, not a nudge's. */
function afkUserSent(cell) {
  cell.afkNudged = false;
}

// ---------- digest ----------

const AFK_KINDS = {
  permission: ['Deferred', 'circle-slash'],
  question: ['Answered for you', 'question'],
  plan: ['Plan held', 'checklist'],
  nudge: ['Asked to carry on', 'debug-continue'],
};

/** The card that lists what HQ did while the user was away, once the session is no longer AFK. */
function afkDigest(cell) {
  cell.afkDigested = true;
  const log = cell.afkLog;
  cell.afkLog = [];
  saveState();
  if (!log.length) return;
  const counts = Object.keys(AFK_KINDS).map(k => [k, log.filter(e => e.kind === k).length]).filter(([, n]) => n);
  const deferred = log.filter(e => e.kind === 'permission').length;
  const head = `While you were away (${afkClock(cell.afkSince || log[0].at)} – ${afkClock(Date.now())})`;
  const rows = log.filter(e => e.kind !== 'nudge').map(e => h('div', { class: `afk-row afk-${e.kind}` },
    icon(AFK_KINDS[e.kind] ? AFK_KINDS[e.kind][1] : 'info'),
    h('span', { class: 'afk-kind', text: AFK_KINDS[e.kind] ? AFK_KINDS[e.kind][0] : e.kind }),
    h('span', { class: 'afk-text', text: e.text }),
    h('span', { class: 'afk-time', text: new Date(e.at).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }) })));
  const summary = counts.map(([k, n]) => `${n} ${AFK_KINDS[k][0].toLowerCase()}`).join(' · ');
  cell.append(h('div', { class: 'item afk-digest' },
    h('div', { class: 'afk-head' }, icon('coffee'), h('span', { text: head })),
    h('div', { class: 'afk-summary', text: summary }),
    rows,
    deferred ? h('div', { class: 'afk-hint', text: `To go ahead with what was deferred, tell ${cell.agent} so (for example "go ahead with the deferred steps").` }) : null));
  cell.markUnread();
  cell.refreshState();
}

// ---------- UI ----------

const afkPill = document.getElementById('afk'); // loaded before app.js, which defines $

function renderAfkPill() {
  const a = app.afk;
  const own = app.cells.filter(c => c.afk).length;
  afkPill.classList.toggle('on', !!a);
  afkPill.classList.toggle('some', !a && own > 0);
  afkPill.querySelector('.afk-text').textContent = a ? (a.until == null ? 'AFK until back' : `AFK until ${afkClock(a.until)}`) : 'AFK';
  afkPill.title = a
    ? 'You are AFK: sessions carry on without asking you. Click to change or end it.'
    : own ? `${own} session${own === 1 ? ' is' : 's are'} AFK on their own. Click to go AFK everywhere.`
      : 'Going AFK? Sessions carry on without asking you until you are back.';
}

/** The AFK menu, for all of HQ (cell null) or for one session. */
function buildAfkMenu(menuEl, cell) {
  const item = (name, desc, run, ico, checked) => h('button', { class: `menu-item${checked ? ' checked' : ''}`, onclick: () => { closeMenu(); run(); } },
    icon(ico), h('span', { class: 'menu-main' }, h('span', { class: 'name', text: name }), desc ? h('span', { class: 'desc', text: desc }) : null));
  const own = cell ? cell.afk : app.afk;
  const until = cell ? afkUntil(cell) : own ? (own.until == null ? Infinity : own.until) : 0;
  menuEl.append(h('div', { class: 'menu-title', text: cell ? 'AFK for this session' : 'AFK' }));
  if (cell && cell.afk === false) {
    menuEl.append(h('div', { class: 'menu-note', text: 'This session never goes AFK: it always waits for you.' }));
  } else if (until) {
    const by = cell && !cell.afk ? ' (all of HQ)' : '';
    menuEl.append(h('div', { class: 'menu-note', text: until === Infinity ? `AFK until you are back${by}.` : `AFK until ${afkClock(until)}${by}.` }));
  } else {
    menuEl.append(h('div', { class: 'menu-note', text: 'Sessions carry on without you: questions get "your call", plans are held and other approvals are deferred. Each waits a minute for you first.' }));
  }
  if (own) menuEl.append(item("I'm back", cell ? 'End AFK for this session' : 'End AFK for every session', () => setAfk(cell, 0), 'debug-stop'));
  menuEl.append(h('div', { class: 'menu-title', text: own ? 'Change to' : 'Away for' }));
  for (const [minutes, label] of AFK_DURATIONS) {
    const at = Date.now() + minutes * 60 * 1000;
    menuEl.append(item(label, `Until ${afkClock(at)}`, () => setAfk(cell, Date.now() + minutes * 60 * 1000), 'watch'));
  }
  const time = h('input', { type: 'time', class: 'afk-time-input', title: 'Back at' });
  const setAt = () => {
    const m = /^(\d{1,2}):(\d{2})$/.exec(time.value);
    if (!m) return;
    const d = new Date();
    d.setHours(Number(m[1]), Number(m[2]), 0, 0);
    if (d.getTime() <= Date.now()) d.setDate(d.getDate() + 1); // an earlier time means tomorrow
    closeMenu();
    setAfk(cell, d.getTime());
  };
  time.addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); setAt(); } });
  menuEl.append(h('div', { class: 'menu-item afk-at' }, icon('calendar'), h('span', { class: 'menu-main' }, h('span', { class: 'name', text: 'Back at' })), time,
    h('button', { class: 'btn', text: 'Set', onclick: setAt })));
  menuEl.append(item('Until I am back', 'No set time', () => setAfk(cell, Infinity), 'coffee'));
  if (cell) {
    menuEl.append(h('div', { class: 'menu-title', text: 'This session' }));
    menuEl.append(item('Follow HQ', app.afk ? 'AFK while HQ is' : 'AFK only when HQ is', () => setAfkOptOut(cell, false), cell.afk == null ? 'check' : 'blank', cell.afk == null));
    menuEl.append(item('Never AFK', 'Always wait for you here', () => setAfkOptOut(cell, true), cell.afk === false ? 'check' : 'blank', cell.afk === false));
  }
}

afkPill.addEventListener('click', () => openMenu(afkPill, null, menuEl => buildAfkMenu(menuEl, null)));
