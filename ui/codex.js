'use strict';

// ======================================================================================
// Codex cells
// ======================================================================================
//
// A Codex cell is an ordinary Cell with `backend === 'codex'`: src/codex.rs translates the
// Codex app-server protocol to the stream-json lines Claude speaks, so the chat itself needs
// nothing new. What differs is kept here, apart from Claude's: the model list (which Codex
// reports, so new models show up by themselves), the slash commands, the defaults for new
// sessions, the permission modes and keeping Codex up to date. app.js calls these functions at
// the few places the agents differ; removing Codex means removing this file and those calls.

const codex = {
  models: [],     // from the last Codex initialize reply (or codex_models); Claude's list is app.models
  modelsAt: 0,    // when `models` was last read (ms)
  commands: [],   // Codex's slash commands; Claude's are app.commands
  defaults: {},   // permission mode, model and effort picked last in a Codex cell
  updateAt: 0,    // when `codex update` last ran (ms), so a restart does not repeat it
  used: false,    // a Codex cell has run on this board: only then is Codex updated or asked anything
};

// Codex's three presets under the names HQ saves (Claude's); src/codex.rs maps them.
const CODEX_MODES = [
  ['default', 'Read only (asks before edits)'],
  ['acceptEdits', 'Edit automatically (this folder)'],
  ['bypassPermissions', 'Full access (never asks)'],
];
/** Codex's own default: edits inside the folder, asking for anything beyond it. */
const CODEX_DEFAULT_MODE = 'acceptEdits';

/** Codex's wording of TASK_HINT; codex.rs (COMPOSER_TASK_HINT) cuts it out of replayed history. */
const CODEX_TASK_HINT = '(HQ: track this work with a short plan so the user can follow progress.)';

// Codex updates itself only from its own screens, and HQ runs the app-server, so HQ runs
// `codex update` while no Codex cell is running: soon after start and then every 6 hours.
// New models reach the model menu without it too: Codex reports its model list each start.
const CODEX_UPDATE_EVERY_MS = 6 * 60 * 60 * 1000;
const CODEX_UPDATE_WAIT_MS = 180 * 1000;
const CODEX_MODELS_EVERY_MS = 6 * 60 * 60 * 1000;
const codexUpdate = { promise: null };

function codexAvailable() {
  return Array.isArray(build.backends) && build.backends.includes('codex');
}

/** Fits a new Codex cell's header and composer to Codex. */
function codexDecorate(cell) {
  const el = cell.el;
  el.classList.add('agent-codex');
  el.querySelector('.cell-state').before(h('span', { class: 'agent-tag', text: 'Codex', title: 'Codex session' }));
  cell.modeSel.replaceChildren(...CODEX_MODES.map(([value, label]) => h('option', { value, text: label })));
  if (!CODEX_MODES.some(([value]) => value === cell.permissionMode)) cell.permissionMode = CODEX_DEFAULT_MODE;
  cell.mcpBtn.classList.add('hidden'); // Codex's MCP servers are set up in its own config (codex mcp)
  el.querySelector('.act-handoff').title = 'Handoff: Codex writes a Markdown summary another session can continue from';
  cell.input.placeholder = 'Ask Codex…  (Enter to send, Shift+Enter for new line)';
  codex.used = true;
  refreshCodexModels();
}

function codexModels() {
  return codex.models.length ? codex.models : [{ value: 'default', displayName: 'Default', description: "Codex's configured model" }];
}

/** Effort levels the cell's model accepts, as Codex lists them per model; empty until the list is in. */
function codexEffortLevels(cell) {
  const m = codex.models.find(x => x.value === (cell.modelChoice || 'default'));
  return m && m.supportsEffort ? m.supportedEffortLevels : [];
}

/** Keeps what a Codex initialize reply reports, without touching Claude's lists. */
function codexInitialized(r) {
  if (Array.isArray(r.models) && r.models.length) {
    codex.models = sanitizeModels(r.models);
    codex.modelsAt = Date.now();
    saveState();
    for (const c of app.cells) if (c.backend === 'codex') c.updateModelLabel();
  }
  if (Array.isArray(r.commands)) codex.commands = sanitizeCommands(r.commands);
}

/** Reads Codex's model list when it is missing or old, so a cell that has not started can offer it. */
async function refreshCodexModels() {
  if (!codexAvailable() || codex.loadingModels || Date.now() - codex.modelsAt < CODEX_MODELS_EVERY_MS) return;
  codex.loadingModels = true;
  try {
    await withTimeout(codexUpdate.promise || Promise.resolve(), CODEX_UPDATE_WAIT_MS);
    codexInitialized(await invoke('codex_models'));
  } catch (err) {
    console.warn('Codex models unavailable', err);
  } finally {
    codex.loadingModels = false;
  }
}

function codexRemember(choice) {
  const { permissionMode, model, effort } = choice;
  codex.defaults = {
    ...codex.defaults, ...(permissionMode ? { permissionMode } : {}), ...(model !== undefined ? { model } : {}), ...(effort !== undefined ? { effort } : {}),
  };
  saveState();
}

/** The defaults a new Codex cell starts with. */
function codexDefaults() {
  return { permissionMode: CODEX_DEFAULT_MODE, ...codex.defaults };
}

/**
 * Runs `codex update` unless one ran in the last 6 hours, a Codex cell is running (the update
 * replaces the binary under it), or Codex has never been used here. Resolves when it ends.
 */
function updateCodex() {
  if (codexUpdate.promise) return codexUpdate.promise;
  if (!codexAvailable() || !codex.used || Date.now() - codex.updateAt < CODEX_UPDATE_EVERY_MS) return Promise.resolve();
  if (app.cells.some(c => c.backend === 'codex' && (c.proc != null || c.spawning))) return Promise.resolve();
  codex.updateAt = Date.now();
  saveState();
  const run = invoke('update_codex')
    .then(out => {
      console.info('codex update:', out);
      codex.modelsAt = 0; // a new Codex can list new models
      refreshCodexModels();
    }, err => console.warn('codex update failed:', err))
    .finally(() => { codexUpdate.promise = null; });
  codexUpdate.promise = run;
  return run;
}

/** A Codex cell about to start waits for an update in flight, so it never starts a half-replaced binary. */
async function codexBeforeSpawn(cell) {
  if (!codexUpdate.promise) return;
  cell.metaEl.textContent = 'Updating Codex…';
  await withTimeout(codexUpdate.promise, CODEX_UPDATE_WAIT_MS);
  if (cell.metaEl.textContent === 'Updating Codex…') cell.metaEl.textContent = '';
}

function codexState() {
  return {
    models: codex.models, modelsAt: codex.modelsAt || undefined, commands: codex.commands, defaults: codex.defaults,
    updateAt: codex.updateAt || undefined, used: codex.used || undefined,
  };
}

function codexRestore(saved) {
  const s = saved && typeof saved === 'object' ? saved : {};
  codex.models = sanitizeModels(s.models);
  codex.modelsAt = Number.isFinite(s.modelsAt) ? s.modelsAt : 0;
  codex.commands = sanitizeCommands(s.commands);
  codex.defaults = sanitizeDefaults(s.defaults);
  codex.updateAt = Number.isFinite(s.updateAt) ? s.updateAt : 0;
  codex.used = s.used === true;
  // Checked once the board is back, then every hour (it runs at most every 6).
  setTimeout(updateCodex, 20 * 1000);
  setInterval(updateCodex, 60 * 60 * 1000);
}

/**
 * Codex's own sessions for the resume list, shaped like Claude's, or null when not asked for.
 * Asking starts a short-lived Codex, so only once a Codex cell has run on this board.
 */
function codexSessions(cwd) {
  return codexAvailable() && codex.used ? readCodexSessions(cwd) : null;
}

async function readCodexSessions(cwd) {
  try {
    return await invoke('list_codex_sessions', { limit: 80, cwd });
  } catch (err) {
    console.warn('Codex sessions unavailable', err);
    return [];
  }
}

/** Adds a Codex filter after HQ's in the resume dialog, when this build has Codex. */
function codexResumeFilter(filters) {
  if (!codexAvailable() || filters.querySelector('[data-source="Codex"]')) return;
  filters.querySelector('[data-source="sdk-cli"]').after(h('button', { 'data-source': 'Codex', text: 'Codex' }));
}
