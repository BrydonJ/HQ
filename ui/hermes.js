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
  next: false,  // the new-session picker starts the next session in Hermes
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
  cell.taskBtn.classList.add('hidden'); // the task-list nudge names Claude Code's tools
  el.querySelector('.act-handoff').title = 'Handoff: Hermes writes a Markdown summary another session can continue from';
  cell.input.placeholder = 'Ask Hermes…  (Enter to send, Shift+Enter for new line)';
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

/** The agent the new-session picker is set to. */
function pickerBackend() {
  return hermes.next && hermesAvailable() ? 'hermes' : 'claude';
}

/** Adds the Claude / Hermes switch to the new-session picker, when this build has Hermes. */
function hermesPickerSwitch(picker) {
  let row = picker.querySelector('.picker-agent');
  if (!hermesAvailable()) {
    if (row) row.remove();
    return;
  }
  if (!row) {
    row = h('div', { class: 'segmented picker-agent', title: 'Which agent the new session runs' });
    picker.querySelector('.picker-title').after(row);
  }
  const option = (label, value) => h('button', {
    class: hermes.next === value ? 'on' : '',
    onclick: e => {
      e.stopPropagation();
      hermes.next = value;
      saveState();
      hermesPickerSwitch(picker);
    },
  }, label);
  row.replaceChildren(option('Claude', false), option('Hermes', true));
}

function hermesState() {
  return { models: hermes.models, commands: hermes.commands, defaults: hermes.defaults, next: hermes.next || undefined };
}

function hermesRestore(saved) {
  const s = saved && typeof saved === 'object' ? saved : {};
  hermes.models = sanitizeModels(s.models);
  hermes.commands = sanitizeCommands(s.commands);
  hermes.defaults = sanitizeDefaults(s.defaults);
  hermes.next = s.next === true;
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

/** Shown at the top of the resume list while Hermes' sessions are still being read. */
function hermesSessionsLoading() {
  return h('div', { class: 'session-empty hermes-loading', text: 'Loading Hermes sessions…' });
}

/** Adds a Hermes filter after HQ's in the resume dialog, when this build has Hermes. */
function hermesResumeFilter(filters) {
  if (!hermesAvailable() || filters.querySelector('[data-source="Hermes"]')) return;
  filters.querySelector('[data-source="sdk-cli"]').after(h('button', { 'data-source': 'Hermes', text: 'Hermes' }));
}
