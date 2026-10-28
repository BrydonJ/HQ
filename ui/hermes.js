'use strict';

// ======================================================================================
// Hermes Agent cells
// ======================================================================================
//
// A Hermes cell is an ordinary Cell with `backend === 'hermes'`: src/hermes.rs translates
// Hermes' ACP to the stream-json lines Claude speaks, so the chat itself needs nothing new.
// What differs is kept here, apart from Claude's: the model list, the slash commands, the
// defaults for new sessions and the permission modes. app.js calls these functions at the few
// places the two agents differ; removing Hermes means removing this file and those calls.

const hermes = {
  models: [],   // from the last Hermes initialize reply; Claude's list is app.models
  commands: [], // Hermes' slash commands; Claude's are app.commands
  defaults: {}, // permission mode and model picked last in a Hermes cell
};

// Hermes' modes under the names HQ saves (Claude's); src/hermes.rs maps them. Hermes has no plan mode.
const HERMES_MODES = [
  ['default', 'Ask before edits'],
  ['acceptEdits', 'Edit automatically'],
  ['bypassPermissions', "Don't ask (except sensitive paths)"],
];

function hermesAvailable() {
  return Array.isArray(build.backends) && build.backends.includes('hermes');
}

/** Fits a new Hermes cell's header and composer to Hermes. */
function hermesDecorate(cell) {
  const el = cell.el;
  el.classList.add('agent-hermes');
  el.querySelector('.cell-state').before(h('span', { class: 'agent-tag', text: 'Hermes', title: 'Hermes Agent session' }));
  cell.modeSel.replaceChildren(...HERMES_MODES.map(([value, label]) => h('option', { value, text: label })));
  if (!HERMES_MODES.some(([value]) => value === cell.permissionMode)) cell.permissionMode = 'default';
  cell.mcpBtn.classList.add('hidden'); // Hermes' MCP servers are set up in its own config
  el.querySelector('.act-handoff').title = 'Handoff: Hermes writes a Markdown summary another session can continue from';
  cell.input.placeholder = 'Ask Hermes…  (Enter to send, Shift+Enter for new line)';
  hermesWatchLearnings();
}

function hermesModels() {
  if (!hermes.models.length) return [{ value: 'default', displayName: 'Default', description: "Hermes' configured model" }];
  // Hermes names each model "<provider> · <model>" and puts the provider in the description too.
  return hermes.models.map(m => {
    const short = m.displayName.split(' · ').at(-1);
    return /^Provider:/.test(m.description || '') && short ? { ...m, displayName: short } : m;
  });
}

/** Keeps what a Hermes initialize reply reports, without touching Claude's lists. */
function hermesInitialized(r) {
  if (Array.isArray(r.models) && r.models.length) {
    hermes.models = sanitizeModels(r.models);
    saveState();
  }
  if (Array.isArray(r.commands)) hermes.commands = sanitizeCommands(r.commands);
}

function hermesRemember(choice) {
  const { permissionMode, model } = choice;
  hermes.defaults = { ...hermes.defaults, ...(permissionMode ? { permissionMode } : {}), ...(model !== undefined ? { model } : {}) };
  saveState();
}

function hermesState() {
  return { models: hermes.models, commands: hermes.commands, defaults: hermes.defaults };
}

function hermesRestore(saved) {
  const s = saved && typeof saved === 'object' ? saved : {};
  hermes.models = sanitizeModels(s.models);
  hermes.commands = sanitizeCommands(s.commands);
  hermes.defaults = sanitizeDefaults(s.defaults);
}

/**
 * Hermes' own sessions for the resume list, shaped like Claude's, or null when not asked for.
 * Asking starts a short-lived Hermes, so only once a Hermes cell has run on this board.
 */
function hermesSessions(cwd) {
  return hermesAvailable() && hermes.models.length ? readHermesSessions(cwd) : null;
}

async function readHermesSessions(cwd) {
  try {
    const list = await invoke('list_hermes_sessions', { limit: 80, cwd });
    return list.map(s => ({
      sessionId: s.sessionId, cwd: s.cwd, cwdExists: s.cwdExists, title: s.title || null,
      entrypoint: 'Hermes', backend: 'hermes', modifiedMs: Date.parse(s.updatedAt || '') || 0,
    }));
  } catch (err) {
    console.warn('Hermes sessions unavailable', err);
    return [];
  }
}

/** Adds a Hermes filter after HQ's in the resume dialog, when this build has Hermes. */
function hermesResumeFilter(filters) {
  if (!hermesAvailable() || filters.querySelector('[data-source="Hermes"]')) return;
  filters.querySelector('[data-source="sdk-cli"]').after(h('button', { 'data-source': 'Hermes', text: 'Hermes' }));
}

// ---------- self-improvement ----------
//
// Hermes saves notes to its memory and writes skills as it works: during a turn, in a background
// review after one, and when its curator tidies up. src/hermes.rs reads those changes from Hermes'
// own files (the review and the curator say nothing over ACP); each one is shown in the chat of
// the cell it most likely came from, at every detail level.

const HERMES_LEARN_EVERY_MS = 10 * 1000;
let hermesLearnTimer = 0;

/** Starts reading Hermes' learnings while a Hermes cell is open (a new Hermes cell calls it). */
function hermesWatchLearnings() {
  if (hermesLearnTimer || !hermesAvailable()) return;
  hermesLearnTimer = setInterval(readHermesLearnings, HERMES_LEARN_EVERY_MS);
  setTimeout(readHermesLearnings); // after the cell joins the board; the first read only notes what is there already
}

async function readHermesLearnings() {
  const cells = app.cells.filter(c => c.backend === 'hermes' && !c.closing);
  if (!cells.length) { clearInterval(hermesLearnTimer); hermesLearnTimer = 0; return; }
  let list;
  try {
    list = await invoke('hermes_learnings');
  } catch (err) {
    console.warn('Hermes learnings unavailable', err);
    return;
  }
  const byCell = new Map();
  for (const entry of list) {
    const cell = hermesLearningCell(entry, cells);
    if (!cell) continue;
    if (!byCell.has(cell)) byCell.set(cell, []);
    byCell.get(cell).push(entry);
  }
  for (const [cell, entries] of byCell) cell.append(hermesLearningItem(entries));
}

/**
 * The cell a learning most likely came from: the one whose session a skill change names, else the
 * only Hermes cell working, else the one whose turn ended last (a review runs right after a turn).
 */
function hermesLearningCell(entry, cells) {
  const named = entry.sessionId && cells.find(c => c.sessionId === entry.sessionId);
  if (named) return named;
  const working = cells.filter(c => c.status === 'running');
  if (working.length === 1) return working[0];
  return cells.reduce((a, b) => ((b.lastTurnEnd || 0) > (a.lastTurnEnd || 0) ? b : a));
}

const SKILL_VERBS = { create: 'created', edit: 'rewritten', patch: 'patched', write_file: 'updated', remove_file: 'updated',
  delete: 'deleted', archive: 'archived', restore: 'restored', rollback: 'rolled back', consolidate: 'merged' };

/** One line per change, worded as Hermes words them ("Skill 'x' created", "Memory updated"). */
function hermesLearningItem(entries) {
  const rows = [];
  let background = false;
  for (const e of entries) {
    if (e.kind === 'skill') {
      if (e.actor === 'curator') background = true;
      rows.push(h('div', { class: 'learn-row' }, icon('book'),
        h('span', { text: `Skill '${e.skill || '?'}' ${SKILL_VERBS[e.action] || e.action}` })));
    } else if (e.kind === 'memory') {
      const label = e.target === 'user' ? 'User profile' : 'Memory';
      for (const text of e.added || []) rows.push(h('div', { class: 'learn-row' }, icon('add'), h('span', { class: 'learn-label', text: label }), h('span', { class: 'learn-text', text })));
      for (const text of e.removed || []) rows.push(h('div', { class: 'learn-row removed' }, icon('remove'), h('span', { class: 'learn-label', text: label }), h('span', { class: 'learn-text', text })));
    }
  }
  const title = background ? 'Self-improvement review' : 'Hermes learned';
  const item = h('details', { class: 'item learn' },
    h('summary', {}, icon('lightbulb'), h('span', { class: 'learn-title', text: title }),
      h('span', { class: 'learn-count', text: `${rows.length} change${rows.length === 1 ? '' : 's'}` })),
    h('div', { class: 'learn-body' }, rows));
  item.open = rows.length <= 3; // a long review stays folded to its count
  return item;
}
