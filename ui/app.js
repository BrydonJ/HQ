'use strict';
// Board, cells and the stream-json protocol. Every message is parsed tolerantly: unknown
// message types and fields are ignored, so Claude Code updates degrade to "not shown"
// instead of breaking the app.

const { invoke, Channel } = window.__TAURI__.core;

const $ = sel => document.querySelector(sel);
const grid = $('#grid');
const dock = $('#dock');
const board = $('#board');
const MAX_RECENT = 20;

const app = {
  cells: [],
  focused: null,
  maximized: null,
  recent: [],
  models: [],          // model list from the last initialize reply, so idle cells can offer it too
  defaults: {},        // permission mode, model and effort last picked, for new sessions
  workspaces: {},      // projectKey(folder) -> .code-workspace file the VS Code button opens
  // (project colour picks live in render.js's projectColorPicks; saved as `projectColors`)
  commands: [],        // slash commands from the last initialize reply, for the composer's autocomplete
  claudeUpdateAt: 0,   // when `claude update` last ran (ms), so a restart does not repeat it
  remoteDefault: null, // new sessions start with Remote Control on; null until seeded from the account
  remoteAvailable: null, // from the last initialize reply: null until a session has reported it
  settings: null,      // the Settings screen; see sanitizeSettings for the keys
  sidebarOpen: false,
  query: '',
};

// Which copy of HQ this is (see build_info in main.rs); filled in by restore().
let build = { profile: null, version: '', liveUi: false, updatable: false };

// Shown until a session has started once and reported the real list.
const FALLBACK_MODELS = [
  { value: 'default', displayName: 'Default', description: 'Your configured default model' },
  { value: 'fable', displayName: 'Fable' },
  { value: 'opus', displayName: 'Opus' },
  { value: 'sonnet', displayName: 'Sonnet' },
  { value: 'haiku', displayName: 'Haiku' },
];
const ALL_EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max'];

// Shown above the prompt box while Claude works, in the spirit of the CLI's spinner. A new one is drawn
// every BUSY_ROTATE_MS during a turn, from a shuffled deck shared by all cells, so none repeats until
// the whole list has been shown.
const BUSY_VERBS = ['Cooking', 'Locking in', 'Vibe coding', 'Aura farming', 'Manifesting', 'Speedrunning', 'Main charactering',
  'Big braining', 'Rizzing up the compiler', 'Sending it', 'Yapping internally', 'Touching grass (briefly)', 'Lowkey grinding',
  'Highkey grinding', 'Gatekeeping bugs', 'Glazing the codebase', 'Rawdogging the stack', 'Delulu debugging',
  'Doomscrolling the docs', 'Entering goblin mode', 'Clauding', 'Letting it cook', 'Mewing', 'No-capping', 'Slaying',
  'Understanding the assignment', 'Living rent free in RAM', 'Being so real', 'Eating (no crumbs)',
  'Giving it main quest energy', 'Pulling an all-nighter', 'Spilling the tea', 'Running it back', 'Hitting the griddy',
  'Pondering', 'Ruminating', 'Noodling', 'Percolating', 'Marinating', 'Simmering', 'Brewing', 'Fermenting', 'Steeping',
  'Kneading the dough', 'Proofing', 'Whisking', 'Flambéing', 'Seasoning to taste', 'Reducing the sauce', 'Plating up',
  'Consulting the rubber duck', 'Asking the rubber duck nicely', 'Bribing the linter', 'Negotiating with the borrow checker',
  'Appeasing the type system', 'Reticulating splines', 'Herding cats', 'Wrangling semicolons', 'Untangling spaghetti',
  'Polishing pixels', 'Counting brackets', 'Balancing parentheses', 'Chasing a null', 'Hunting heisenbugs',
  'Interrogating the stack trace', 'Reading the fine print', 'Grepping the universe', 'Spelunking the call graph',
  'Defragmenting thoughts', 'Compiling vibes', 'Refactoring reality', 'Bikeshedding (efficiently)', 'Yak shaving',
  'Shaving the yak closer', 'Summoning the daemon', 'Feeding the hamsters', 'Spinning up the hamster wheel',
  'Warming up the GPUs', 'Cranking the flux capacitor', 'Calibrating the vibes', 'Aligning the chakras of the repo',
  'Channeling the senior dev', 'Doing the thing', 'Thinking really hard', 'Thinking even harder', 'Galaxy braining',
  'Connecting the dots', 'Following the white rabbit', 'Taking the red pill', 'Entering the matrix', 'Hacking the mainframe',
  'Enhancing… enhance', 'Going full send', 'Shipping it (almost)', 'Crunching numbers', 'Munching tokens',
  'Snacking on context', 'Digesting the diff', 'Sipping coffee', 'Refilling the coffee', 'Speed-reading the codebase',
  'Taking notes', 'Sketching a plan', 'Drafting the blueprint', 'Laying the bricks', 'Pouring the foundation',
  'Tightening the bolts', 'Oiling the gears', 'Tuning the engine', 'Checking under the hood', 'Kicking the tires',
  'Doing a vibe check', 'Passing the vibe check', 'Rolling for initiative', 'Casting fireball at the bug', 'Leveling up',
  'Grinding XP', 'Farming loot', 'Speedrunning any%', 'Finding the secret level', 'Saving at the bonfire',
  'Respawning', 'Unlocking achievements', 'Collecting the infinity stones', 'Assembling', 'Transforming',
  'Charging the kamehameha', 'Going super saiyan', 'Using the force', 'Consulting the oracle', 'Reading the tea leaves',
  'Gazing into the crystal ball', 'Divining', 'Conjuring', 'Enchanting', 'Transmuting', 'Alchemizing', 'Spellcasting',
  'Brewing a potion', 'Moonwalking', 'Breakdancing', 'Doing the worm', 'Vogueing', 'Jazz-handing', 'Tap dancing',
  'Beatboxing', 'Freestyling', 'Dropping bars', 'Mixing the beat', 'Tuning the guitar', 'Composing', 'Orchestrating',
  'Conducting', 'Improvising', 'Riffing', 'Jamming', 'Vibing', 'Grooving', 'Boogieing', 'Skedaddling', 'Moseying',
  'Sashaying', 'Galumphing', 'Frolicking', 'Gallivanting', 'Meandering', 'Wandering', 'Exploring', 'Charting the map',
  'Navigating', 'Setting sail', 'Raising the anchor', 'Catching the wind', 'Riding the wave', 'Surfing the stack',
  'Diving deep', 'Coming up for air', 'Climbing the tree', 'Walking the AST', 'Traversing the graph', 'Sorting it out',
  'Binary searching', 'Hashing it out', 'Caching in', 'Flushing buffers', 'Garbage collecting', 'Allocating brain cells',
  'Defrosting the cache', 'Reheating leftovers', 'Microwaving the build', 'Toasting', 'Buttering the toast',
  'Spreading the jam', 'Slicing the bread', 'Rolling the sushi', 'Folding dumplings', 'Tossing the pizza', 'Grilling',
  'Barbecuing', 'Smoking the brisket', 'Braai-ing', 'Lighting the braai', 'Making a plan, boet', 'Sharp-sharping',
  'Just now-ing', 'Now now-ing', 'Eish-ing', 'Lekker coding', 'Jislaaik', 'Hustling', 'Bustling', 'Tinkering',
  'Fiddling', 'Twiddling', 'Tweaking', 'Nudging', 'Finessing', 'Massaging', 'Sculpting', 'Whittling', 'Chiselling',
  'Sanding the edges', 'Varnishing', 'Knitting', 'Crocheting', 'Weaving', 'Stitching it together', 'Patching',
  'Darning socks', 'Ironing out wrinkles', 'Origami-ing', 'Building the Lego set', 'Finding the missing piece',
  'Solving the puzzle', 'Cracking the code', 'Picking the lock', 'Decrypting', 'Deciphering', 'Translating',
  'Interpreting', 'Parsing', 'Lexing', 'Tokenizing', 'Embedding', 'Attending to attention', 'Predicting the next token',
  'Sampling', 'Beam searching', 'Hallucinating responsibly', 'Fact-checking itself', 'Double-checking',
  'Triple-checking', 'Measuring twice', 'Cutting once', "Crossing the t's", "Dotting the i's", 'Minding the gap',
  'Mind melding', 'Telepathing', 'Overclocking', 'Underpromising', 'Overdelivering', 'Cooking with gas',
  'Cooking up a storm', 'Stirring the pot', 'Adding a pinch of salt', "Chef's kissing", 'Being built different',
  'Hitting different', 'Living its best life', 'Rent-free thinking', 'Sweating the details', 'Going the extra mile',
  'Rubbing two neurons together', 'Firing synapses', 'Booting up the brain', 'Loading… please hold', 'Buffering wisdom',
  'Downloading more RAM', 'Turning it off and on again', 'Blowing on the cartridge', 'Wiggling the cable',
  'Reading the manual (finally)', 'Asking Stack Overflow (in spirit)', 'Rereading the error', 'Staring at the screen',
  'Squinting at the logs', 'Tracing the breadcrumbs', 'Following the money', 'Sniffing out bugs', 'Squashing bugs',
  'Swatting flies', 'Debugging with print', 'Adding console.log', 'Removing console.log', 'Committing to the bit',
  'Rebasing feelings', 'Cherry-picking ideas', 'Merging thoughts', 'Resolving conflicts', 'Stashing worries',
  'Pushing through', 'Pulling it together', 'Forking the timeline', 'Branching out', 'Tagging the release (mentally)'];
const BUSY_ROTATE_MS = 9000;
const busyDeck = [];

/** The next verb from the shuffled deck; a fresh shuffle once it runs out, never starting on `avoid`. */
function nextBusyVerb(avoid) {
  if (!busyDeck.length) {
    busyDeck.push(...BUSY_VERBS);
    for (let i = busyDeck.length - 1; i > 0; i--) {
      const j = Math.floor(Math.random() * (i + 1));
      [busyDeck[i], busyDeck[j]] = [busyDeck[j], busyDeck[i]];
    }
    if (`${busyDeck[busyDeck.length - 1]}…` === avoid) busyDeck.unshift(busyDeck.pop());
  }
  return `${busyDeck.pop()}…`;
}
const loaderDeck = [];

/** The next waiting-view loader ([name, description, svg] from loaders.js), dealt like the busy verbs. */
function nextLoader(avoid) {
  if (!loaderDeck.length) {
    loaderDeck.push(...LOADERS);
    for (let i = loaderDeck.length - 1; i > 0; i--) {
      const j = Math.floor(Math.random() * (i + 1));
      [loaderDeck[i], loaderDeck[j]] = [loaderDeck[j], loaderDeck[i]];
    }
    if (loaderDeck[loaderDeck.length - 1] === avoid) loaderDeck.unshift(loaderDeck.pop());
  }
  return loaderDeck.pop();
}

setInterval(() => {
  for (const cell of app.cells) if (cell.status === 'running' && !cell.compacting && !cell.wrapUp && !cell.collapsed) cell.nextBusyWord();
}, BUSY_ROTATE_MS);

// The eye button's levels: [icon, what shows, what the next click shows].
const DETAIL_LEVELS = {
  1: ['eye', 'everything', 'replies, thinking and narration'],
  2: ['eye-closed', 'replies, thinking and narration', 'replies only'],
  3: ['comment', 'replies only', 'everything'],
};

// Claude Code only updates itself from interactive sessions, and HQ runs it headless, so HQ
// runs `claude update` before the first session starts and again once the last run is old.
// New models reach the model menu this way, since the menu lists what the CLI reports.
const FORCE_STOP_AFTER_MS = 10 * 1000; // an interrupt Claude has not answered by then offers "Force stop"
const CLAUDE_UPDATE_EVERY_MS = 6 * 60 * 60 * 1000;
const CLAUDE_UPDATE_WAIT_MS = 150 * 1000;
// `promise` is set only while an update runs. The time of the last run is saved in board.json,
// so a restart does not repeat it, and a spawn does not wait for it (see ensureProcess).
const claudeUpdate = { promise: null };

/** Runs `claude update` unless one ran in the last 6 hours. Resolves when the update in flight ends. */
function updateClaude() {
  if (claudeUpdate.promise) return claudeUpdate.promise;
  if (Date.now() - app.claudeUpdateAt < CLAUDE_UPDATE_EVERY_MS) return Promise.resolve();
  app.claudeUpdateAt = Date.now();
  saveState();
  const run = invoke('update_claude')
    .then(out => console.info('claude update:', out), err => console.warn('claude update failed:', err))
    .finally(() => { claudeUpdate.promise = null; });
  claudeUpdate.promise = run;
  return run;
}

let reqSeq = 0;
const nextRequestId = () => `hq-${Date.now().toString(36)}-${++reqSeq}`;

// ======================================================================================
// Cell: one chat session bound to one folder
// ======================================================================================

class Cell {
  constructor({ cwd, sessionId = null, permissionMode = 'default', model = null, effort = null, title = '', aiTitle = false,
    customTitle = null, renamePending = false, detail, clean, progress = false, fork = false, collapsed = false,
    missing = false, remote = null, bridgeSessionId = null, draft = '', backend = 'claude', resumeAfterUpdate = false, resumeQueued = null }) {
    this.cwd = cwd;
    this.backend = backend === 'hermes' ? 'hermes' : 'claude'; // the agent: Claude Code, or Hermes (hermes.js)
    this.sessionId = sessionId;
    this.fork = !!(fork && sessionId); // resume into a new session ID, leaving the original alone
    this.permissionMode = permissionMode || 'default';
    this.modelChoice = model && model !== 'default' ? model : null; // null = Claude's default
    this.effort = effort || null;  // null = Claude's default
    this.resolvedModel = null;     // what Claude reports it is running
    this.appliedEffort = null;
    this.title = title || '';      // Claude's title for the session, or else its first prompt
    this.aiTitle = !!aiTitle;      // true once `title` is Claude's own title
    this.customTitle = customTitle || null; // the user's rename, shown instead of `title`
    this.renamePending = !!(renamePending && customTitle); // rename not yet saved in Claude's transcript
    this.titleTries = 0;           // times Claude was asked for a title by this process
    this.titleAsked = '';          // the prompt text Claude was last asked to title
    this.promptText = '';          // the prompts sent so far, which Claude titles the session from
    this.renaming = false;
    // How much of the chat shows (the eye button): 1 everything, 2 no tool calls, 3 replies only (no thinking or narration).
    // Cells saved before the levels had a `clean` flag: true was the prompts-and-replies view,
    // false showed everything. Cells with no flag start at the new default, 2.
    this.detail = [1, 2, 3].includes(detail) ? detail : clean === true ? 3 : clean === false ? 1 : 2;
    this.progressView = !!progress; // the progress panel over the chat (the checklist button)
    this.wmArmed = false;          // waiting mode: while Claude works, a calm progress view stands in for the chat
    this.wmWorked = false;         // work has been seen since arming (an idle cell arms and waits for it)
    this.wmView = false;           // the waiting view is on screen now (the chat is display:none)
    this.wmTimer = 0;              // one timeout: finishes waiting mode once the cell has stayed idle
    this.wmStick = true;           // stickToBottom, kept while the chat is hidden
    this.taskLine = '';            // "3/7 · Creating b.txt": the task list at a glance, in the header
    this.sideTaskLine = '';        // the same without the live tool name, for the sidebar (which a change rebuilds)
    this.askTasks = false;         // the composer's task-list toggle: asks for a list on the next prompt only
    this.collapsed = !!collapsed;  // only the header shows, in the dock under the grid
    this.savedScroll = null;       // [scrollTop, stickToBottom] from when the chat was collapsed
    this.expandedAt = 0;
    this.missing = !!missing;      // the folder was gone at startup: the cell stays, with its composer off
    this.remote = remote ?? app.remoteDefault; // Remote Control wanted for this session; null until the account's default is known
    this.bridgeSessionId = bridgeSessionId || null; // the claude.ai session Remote Control last used
    this.remoteState = 'off';      // off | connecting | on | failed | policy
    this.remoteUrl = null;
    this.remoteDetail = '';
    this.remoteSeq = 0;            // drops the reply of an enable request that a newer one overtook
    this.sentPrompts = [];         // prompts HQ wrote, so a prompt from elsewhere (the phone) can be told apart
    this.initialDraft = typeof draft === 'string' ? draft : '';
    this.resumeAfterUpdate = !!resumeAfterUpdate; // an update restarted HQ while this cell was working: it carries on by itself
    this.resumeQueued = Array.isArray(resumeQueued) ? resumeQueued.filter(t => typeof t === 'string') : [];
    this.queuedTexts = [];         // prompts sent while a turn ran, which the agent may not have read yet (until the turn ends)
    this.resumeMark = null;        // set as an update starts installing: { queued } for a working cell, which the restart saves
    this.busyWord = '';            // the verb shown above the prompt box this turn
    this.lastReply = '';
    this.color = projectColor(cwd);
    this.attachments = [];         // images waiting to be sent: { mediaType, data, url }
    this.turnTexts = [];           // top-level text items since the last tool call in this turn
    this.mcpServers = null;        // last mcp_status reply (names and states only)
    this.mcpError = null;
    this.proc = null;          // backend process id while claude is running
    this.spawning = null;      // promise while a spawn is in flight
    this.status = 'idle';      // idle | running | exited
    this.bgTasks = [];         // background agents and shells still running: { id, type, description }
    this.unread = false;
    this.pending = new Map();  // control request_id -> prompt element
    this.promptsMin = false;   // the prompts area is folded to its bar (a class only, so typed answers survive)
    this.controls = new Map(); // our control request_id -> resolver awaiting Claude's reply
    this.tools = new Map();    // tool_use_id -> { item, body }
    this.tasks = new Map();    // Claude's task list: task id -> { subject, activeForm, status }
    this.taskCreates = new Map(); // TaskCreate tool_use_id -> task, until its result gives the id
    this.tasksStale = false;   // the list is a finished one from an earlier prompt
    this.taskProgress = new Map(); // task_id -> latest task_progress of an agent Claude started
    this.stoppingTasks = new Set(); // task ids a stop_task was sent for and not yet answered
    this.ctx = { tokens: null, window: null, info: null, infoAt: 0, loading: null, model: null }; // context window use
    this.compacting = false;   // a compaction is running (the CLI's status 'compacting')
    this.wrapUp = null;        // the plan's limit was hit mid-turn and Claude is finishing on the wrap-up allowance: { credits }
    this.cost = { usd: 0, tokens: 0, procUsd: 0, procTokens: 0 }; // this HQ run's spend; proc* = last total of the current process
    this.drafts = new Set();   // prompts submitted while claude was still starting, which Esc can take back
    this.openTools = new Set(); // tool_use ids of tool calls that have not reported a result yet
    this.lastDuration = '';    // how long the last turn took, for the line under the composer
    this.historyStart = 0;     // byte offset in the transcript of the oldest line shown (0: all of it is)
    this.historyLoading = null; // promise while the transcript is being drawn
    this.historyPending = false; // collapsed on restore: the transcript is drawn when the cell is first opened
    this.replayingEarlier = false;
    this.turnStart = null;     // when the current turn started (ms)
    this.turnTools = 0;        // top-level tool calls in the current turn
    this.currentTool = null;   // { id, name, summary } of the latest top-level tool still running
    this.statusAnswer = null;  // { text, at, percent, asking } from the Sitrep button
    this.progressTimer = null;
    this.replaying = false;    // loadHistory is replaying the transcript
    this.live = null;        // { msgId, blocks: Map<index, {item, md, text}> } for streamed text
    this.renderTimer = null;
    this.stickToBottom = true;
    this.closing = false;
    this.buildDom();
  }

  // ---------- DOM ----------

  buildDom() {
    const el = $('#cell-template').content.firstElementChild.cloneNode(true);
    this.el = el;
    this.msgs = el.querySelector('.messages');
    this.prompts = el.querySelector('.prompts');
    this.promptsBar = el.querySelector('.prompts-bar');
    this.input = el.querySelector('textarea');
    this.modeSel = el.querySelector('.mode');
    this.modelBtn = el.querySelector('.act-model');
    this.mcpBtn = el.querySelector('.act-mcp');
    this.attachEl = el.querySelector('.attachments');
    this.metaEl = el.querySelector('.meta');
    this.stopBtn = el.querySelector('.act-stop');
    this.titleEl = el.querySelector('.cell-title');
    this.detailBtn = el.querySelector('.act-detail');
    this.progressBtn = el.querySelector('.act-progress');
    this.progressEl = el.querySelector('.progress-panel');
    this.headBar = el.querySelector('.head-progress');
    this.taskEl = el.querySelector('.cell-tasks');
    this.waitBtn = el.querySelector('.act-waiting');
    this.waitEl = el.querySelector('.waiting-view');
    this.waitBody = el.querySelector('.wv-body');
    this.loader = null;            // the waiting view's current loader, a fresh one each time the view opens
    this.taskBtn = el.querySelector('.act-tasklist');
    this.bgEl = el.querySelector('.bg-tasks');
    this.stateEl = el.querySelector('.cell-state');
    this.codeBtn = el.querySelector('.act-code');
    this.collapseBtn = el.querySelector('.act-collapse');
    this.ctxBtn = el.querySelector('.act-context');
    this.remoteBtn = el.querySelector('.act-remote');
    this.missingBar = el.querySelector('.missing-bar');

    el.style.setProperty('--proj', this.color);
    for (const name of el.querySelectorAll('.proj-name, .cell-project .sweep-name')) name.textContent = baseName(this.cwd) || this.cwd;
    el.querySelector('.cell-path').textContent = this.cwd;
    if (this.backend === 'hermes') hermesDecorate(this);
    this.modeSel.value = this.permissionMode;
    this.setTitle(this.title);
    this.applyDetail();
    this.applyProgressView();
    this.applyWaitingMode();
    this.setAskTasks(false);
    this.applyCollapsed();
    this.applyMissing();
    this.stateEl.textContent = stateOf(this)[1];
    this.renderCodeButton();
    this.updateModelLabel();
    this.renderContext();
    this.renderRemote();
    if (this.initialDraft) {
      this.input.value = this.initialDraft;
      autoGrow(this.input);
    }

    el.addEventListener('mousedown', () => focusCell(this, false));
    const head = el.querySelector('.cell-head');
    // A collapsed cell opens on a click anywhere in its header except its buttons.
    head.addEventListener('click', e => {
      if (this.collapsed && !e.target.closest('button, .cell-rename')) collapseCell(this, false);
    });
    head.addEventListener('dblclick', e => {
      if (Date.now() - this.expandedAt < 600) return; // the first click of this double-click expanded the cell
      if (!e.target.closest('button, .cell-title, .cell-rename, .cell-project')) toggleMaximize(this);
    });
    this.titleEl.addEventListener('click', () => { if (!this.collapsed) this.startRename(); });
    const proj = el.querySelector('.cell-project');
    proj.title = 'Change the project colour';
    proj.addEventListener('click', e => {
      if (this.collapsed) return; // the click opens the cell instead
      e.stopPropagation();
      openMenu(proj, this, menuEl => buildColorMenu(menuEl, this.cwd));
    });
    this.detailBtn.addEventListener('click', () => this.setDetail(this.detail % 3 + 1));
    this.progressBtn.addEventListener('click', () => this.setProgressView(!this.progressView));
    this.waitBtn.addEventListener('click', () => this.setWaitingMode(!this.wmArmed));
    this.taskBtn.addEventListener('click', () => this.setAskTasks(!this.askTasks));
    el.querySelector('.act-max').addEventListener('click', () => toggleMaximize(this));
    el.querySelector('.act-close').addEventListener('click', e => this.requestClose(e.currentTarget));
    el.querySelector('.act-terminal').addEventListener('click', () => invoke('open_terminal', { path: this.cwd }).catch(err => this.notice(String(err), true)));
    el.querySelector('.act-reveal').addEventListener('click', () => invoke('reveal_folder', { path: this.cwd }).catch(err => this.notice(String(err), true)));
    this.codeBtn.addEventListener('click', () => this.openInCode());
    this.codeBtn.addEventListener('contextmenu', e => { e.preventDefault(); this.openCodeMenu(); });
    this.collapseBtn.addEventListener('click', () => collapseCell(this, !this.collapsed));
    this.ctxBtn.addEventListener('click', e => this.compact(e.shiftKey));
    this.ctxBtn.addEventListener('mouseenter', () => showContextPop(this));
    this.ctxBtn.addEventListener('mouseleave', hideContextPop);
    this.ctxBtn.addEventListener('focus', () => showContextPop(this));
    this.ctxBtn.addEventListener('blur', hideContextPop);
    this.remoteBtn.addEventListener('click', () => openMenu(this.remoteBtn, this, menuEl => this.buildRemoteMenu(menuEl)));
    this.handoffBtn = el.querySelector('.act-handoff');
    this.handoffBtn.addEventListener('click', () => this.writeHandoff());
    el.querySelector('.act-locate').addEventListener('click', () => this.locateFolder());
    el.querySelector('.act-drop').addEventListener('click', () => closeCell(this));
    el.querySelector('.act-send').addEventListener('click', () => this.submit());
    this.stopBtn.addEventListener('click', () => this.interrupt());
    this.modeSel.addEventListener('change', () => this.setMode(this.modeSel.value).then(() => this.remember({ permissionMode: this.permissionMode })));
    this.modelBtn.addEventListener('click', () => openMenu(this.modelBtn, this, menuEl => this.buildModelMenu(menuEl)));
    this.mcpBtn.addEventListener('click', () => {
      openMenu(this.mcpBtn, this, menuEl => this.buildMcpMenu(menuEl));
      if (menuState.anchor === this.mcpBtn) this.loadMcp();
    });
    this.input.addEventListener('paste', e => {
      const data = e.clipboardData;
      const files = [...((data && data.files) || [])].filter(f => f.type.startsWith('image/'));
      if (!files.length) return;
      if (!data.types.includes('text/plain')) e.preventDefault();
      this.addImages(files);
    });
    el.addEventListener('dragover', e => {
      if (!e.dataTransfer || !e.dataTransfer.types.includes('Files')) return;
      e.preventDefault();
      e.dataTransfer.dropEffect = 'copy';
      el.classList.add('drag-over');
    });
    el.addEventListener('dragleave', e => {
      if (!el.contains(e.relatedTarget)) el.classList.remove('drag-over');
    });
    el.addEventListener('drop', e => {
      el.classList.remove('drag-over');
      if (!e.dataTransfer || !e.dataTransfer.files.length) return;
      e.preventDefault();
      if (this.collapsed) this.setCollapsed(false);
      focusCell(this, true);
      this.addImages([...e.dataTransfer.files]);
    });

    this.input.addEventListener('keydown', e => {
      if (slashKeydown(this, e)) return;
      if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
        e.preventDefault();
        this.submit();
      }
    });
    this.input.addEventListener('input', () => {
      autoGrow(this.input);
      slashUpdate(this);
      saveState(); // the unsent text is kept in board.json
    });
    this.input.addEventListener('blur', () => slashClose(this));
    this.input.addEventListener('click', () => slashUpdate(this)); // the caret moved into or out of a "/name"
    this.input.addEventListener('keyup', e => { if (e.key === 'ArrowLeft' || e.key === 'ArrowRight' || e.key === 'Home' || e.key === 'End') slashUpdate(this); });
    this.msgs.addEventListener('scroll', () => {
      if (this.chatHidden) return; // a hidden scroller reads as scrolled to the bottom
      const m = this.msgs;
      this.stickToBottom = m.scrollHeight - m.scrollTop - m.clientHeight < 40;
    });
    this.msgs.addEventListener('click', onLinkClick);
    this.msgs.addEventListener('click', onCodeCopyClick);
    this.msgs.addEventListener('click', e => onCodeActionClick(e, this));
    this.msgs.addEventListener('click', onImageClick);
    this.attachEl.addEventListener('click', onImageClick);
    this.promptsBar.addEventListener('click', () => this.setPromptsMin(!this.promptsMin));
    this.prompts.addEventListener('click', onLinkClick);
    this.prompts.addEventListener('click', onCodeCopyClick);
    this.prompts.addEventListener('click', e => onCodeActionClick(e, this));
    for (const area of [this.msgs, this.prompts]) area.addEventListener('mouseup', e => autoCopySelection(e, area));
  }

  append(node, parent) {
    (parent || this.msgs).append(node);
    this.scheduleScroll();
    return node;
  }

  /** Reading scrollHeight forces a layout, so a burst of new items scrolls once, in the next frame. */
  scheduleScroll() {
    if (this.scrollFrame || !this.stickToBottom || this.chatHidden) return;
    this.scrollFrame = requestAnimationFrame(() => {
      this.scrollFrame = 0;
      this.scroll();
    });
  }

  scroll() {
    if (this.stickToBottom) this.msgs.scrollTop = this.msgs.scrollHeight;
  }

  /**
   * Items off screen are sized by an estimate (content-visibility), so the real bottom moves
   * while the ones near it are laid out: scroll again, a few frames, until it holds.
   */
  pinToBottom(frames = 90, steady = 0) {
    const m = this.msgs;
    m.scrollTop = 1e9;
    if (frames <= 0) return;
    requestAnimationFrame(() => {
      if (!this.stickToBottom || this.chatHidden) return;
      const gap = m.scrollHeight - m.scrollTop - m.clientHeight;
      // Done once the bottom has held for a few frames in a row.
      if (gap <= 2 && steady >= 3) return;
      this.pinToBottom(frames - 1, gap <= 2 ? steady + 1 : 0);
    });
  }

  notice(text, isError) {
    return this.append(h('div', { class: `item notice${isError ? ' error' : ''}`, text }));
  }

  // ---------- title and view ----------

  /** The name shown for the session: the user's rename, else Claude's title or the first prompt. */
  displayTitle() {
    return this.customTitle || this.title;
  }

  setTitle(title) {
    this.title = title || '';
    this.renderTitle();
  }

  renderTitle() {
    const name = this.displayTitle();
    this.titleEl.textContent = name || 'New session';
    this.titleEl.classList.toggle('placeholder', !name);
    this.el.querySelector('.cell-head').title = name ? `${this.cwd}\n${name}` : this.cwd;
    renderSidebar();
  }

  startRename() {
    if (this.renaming || !this.titleEl.isConnected) return;
    this.renaming = true;
    const field = h('input', { class: 'cell-rename', type: 'text', value: this.displayTitle(), placeholder: 'Session name', spellcheck: 'false', maxlength: '120' });
    this.titleEl.replaceWith(field);
    field.focus();
    field.select();
    let done = false;
    const finish = save => {
      if (done) return;
      done = true;
      this.renaming = false;
      field.replaceWith(this.titleEl);
      if (save) this.rename(field.value);
    };
    field.addEventListener('keydown', e => {
      e.stopPropagation(); // keeps Esc from stopping Claude and Ctrl+W from closing the cell
      if (e.key === 'Enter') { e.preventDefault(); finish(true); }
      else if (e.key === 'Escape') { e.preventDefault(); finish(false); this.input.focus(); }
    });
    field.addEventListener('blur', () => finish(true));
  }

  /** An empty name keeps the old one: Claude has no way to clear a session's name. */
  rename(text) {
    const name = text.replace(/\s+/g, ' ').trim();
    if (!name || name === this.displayTitle()) return;
    this.customTitle = name;
    this.renamePending = true;
    this.renderTitle();
    saveState();
    this.syncTitle();
  }

  /**
   * Saves a rename in Claude's transcript, so VS Code and `claude --resume` show it too, or asks
   * Claude to title a session that only has its first prompt as a name. Both need a running
   * process and a session Claude has written to disk. A title is asked for when each turn starts
   * (Claude answers beside a running turn, so a long first turn gets its title within seconds);
   * both are tried again after each turn.
   */
  async syncTitle() {
    if (this.proc == null || !this.sessionId || this.fork) return;
    if (this.renamePending) {
      const name = this.customTitle;
      const reply = await this.control({ subtype: 'rename_session', title: name, source: 'host', session_id: this.sessionId });
      if (reply && reply.subtype === 'success' && this.customTitle === name) {
        this.renamePending = false;
        saveState();
      }
      return;
    }
    if (this.customTitle || this.aiTitle || this.titleTries >= 4) return;
    const description = this.promptText || this.title;
    if (!description || description === this.titleAsked) return;
    this.titleAsked = description;
    this.titleTries++; // Claude gives no title to a prompt like "hi", so each later prompt asks again with more text
    const reply = await this.control({ subtype: 'generate_session_title', description, persist: true });
    if (!reply || reply.subtype !== 'success') this.titleAsked = ''; // failed, so the same text may be asked again
    const title = reply && reply.subtype === 'success' && reply.response && reply.response.title;
    if (typeof title === 'string' && title.trim()) {
      this.aiTitle = true;
      this.setTitle(firstLine(title.trim(), 120));
      saveState();
    }
  }

  /** Title lines replayed from Claude's transcript: a rename made anywhere, or Claude's own title. */
  onTitleLine(m) {
    if (m.type === 'custom-title' && typeof m.customTitle === 'string' && m.customTitle.trim()) {
      if (!this.renamePending) this.customTitle = m.customTitle.trim();
    } else if (m.type === 'ai-title' && typeof m.aiTitle === 'string' && m.aiTitle.trim()) {
      this.title = m.aiTitle.trim();
      this.aiTitle = true;
    }
  }

  /** Sets how much of the chat shows: 1 everything, 2 no tool calls, 3 only prompts and replies. */
  setDetail(level) {
    this.detail = [1, 2, 3].includes(level) ? level : 2;
    this.applyDetail();
    this.scroll();
    saveState();
  }

  applyDetail() {
    for (const n of [1, 2, 3]) this.el.classList.toggle(`detail-${n}`, this.detail === n);
    const [icon, now, next] = DETAIL_LEVELS[this.detail];
    this.detailBtn.classList.toggle('on', this.detail !== 2); // lit when off the default
    this.detailBtn.querySelector('.codicon').className = `codicon codicon-${icon}`;
    this.detailBtn.title = `Showing ${now}. Click to show ${next}.`;
  }

  /** Fades the chat and shows Claude's task list, what it is doing now, and its background work. */
  setProgressView(on) {
    this.progressView = on;
    this.applyProgressView();
    if (!on) this.scroll();
    saveState();
  }

  applyProgressView() {
    this.el.classList.toggle('progress-view', this.progressView);
    this.progressBtn.classList.toggle('on', this.progressView);
    this.progressBtn.title = this.progressView ? 'Back to the chat' : 'Show progress: task list and current work';
    this.renderProgress();
  }

  /**
   * Hides the chat and leaves the header in the dock under the grid. The header keeps every
   * status cue (dot, sweep, glow, background badge, task bar), and names the state in words.
   */
  setCollapsed(on) {
    if (this.collapsed === on) return;
    if (on) {
      this.savedScroll = this.wmView ? [this.wmTop, this.wmStick] : [this.msgs.scrollTop, this.stickToBottom];
      if (app.maximized === this) app.maximized = null;
      if (menuState.cell === this) closeMenu();
    }
    this.collapsed = on;
    this.applyCollapsed();
    this.syncProgressTimer();
    layout();
    if (!on) {
      this.expandedAt = Date.now();
      autoGrow(this.input); // a draft restored while the cell was collapsed
      if (this.historyPending) {
        this.historyPending = false;
        this.loadHistory();
      }
      if (this.live) this.scheduleLiveRender(); // text that arrived while collapsed was not drawn
      // Text that arrived while hidden did not scroll, so land where the user left, or at the bottom.
      const [top, stick] = this.savedScroll || [0, true];
      this.stickToBottom = stick;
      if (stick) this.pinToBottom();
      else this.msgs.scrollTop = top;
    }
    saveState();
  }

  applyCollapsed() {
    this.el.classList.toggle('collapsed', this.collapsed);
    this.collapseBtn.querySelector('.codicon').className = `codicon codicon-${this.collapsed ? 'chrome-restore' : 'chrome-minimize'}`;
    this.collapseBtn.title = keyLabel(this.collapsed ? 'Expand (Ctrl+Shift+M)' : 'Collapse to the dock (Ctrl+Shift+M)');
  }

  // ---------- VS Code ----------

  /** Opens the workspace picked for this folder (right-click the button), else the folder itself. */
  async openInCode() {
    const workspace = workspaceFor(this.cwd);
    if (workspace) {
      try {
        await invoke('open_in_vscode', { path: workspace });
        return;
      } catch (err) {
        this.notice(`Could not open ${baseName(workspace)}: ${err}. Opened the folder instead. Right-click the VS Code button to pick another workspace.`, true);
      }
    }
    invoke('open_in_vscode', { path: this.cwd }).catch(err => this.notice(String(err), true));
  }

  renderCodeButton() {
    const workspace = workspaceFor(this.cwd);
    this.codeBtn.classList.toggle('linked', !!workspace);
    this.codeBtn.title = workspace
      ? `Open ${baseName(workspace)} in VS Code\nRight-click to change the workspace`
      : 'Open folder in VS Code\nRight-click to pick a workspace';
  }

  openCodeMenu() {
    openMenu(this.codeBtn, this, menuEl => this.buildCodeMenu(menuEl));
    if (menuState.anchor !== this.codeBtn) return;
    this.workspaceFiles = null;
    // Workspaces often sit together in one folder, so wherever one was picked is searched too.
    invoke('find_workspaces', { path: this.cwd, dirs: workspaceDirs() }).catch(() => []).then(files => {
      this.workspaceFiles = Array.isArray(files) ? files : [];
      if (menuState.anchor === this.codeBtn) refreshMenu();
    });
  }

  buildCodeMenu(menuEl) {
    const current = workspaceFor(this.cwd);
    const files = [...(this.workspaceFiles || [])];
    if (current && !files.some(f => projectKey(f.path) === projectKey(current))) files.unshift({ path: current, includesFolder: false });
    const option = (path, name, desc) => {
      const checked = path ? projectKey(path) === projectKey(current || '') : !current;
      return h('button', { class: `menu-item${checked ? ' checked' : ''}`, title: path || this.cwd, onclick: () => { closeMenu(); setWorkspace(this.cwd, path); } },
        icon(checked ? 'check' : 'blank'),
        h('span', { class: 'menu-main' }, h('span', { class: 'name', text: name }), h('span', { class: 'desc', text: desc })));
    };
    menuEl.append(h('div', { class: 'menu-title', text: 'The VS Code button opens' }));
    menuEl.append(option(null, 'This folder', this.cwd));
    for (const f of files) menuEl.append(option(f.path, baseName(f.path), f.includesFolder ? `Includes this folder · ${parentDir(f.path)}` : parentDir(f.path)));
    if (!this.workspaceFiles) menuEl.append(h('div', { class: 'menu-note', text: 'Looking for workspace files…' }));
    menuEl.append(h('button', { class: 'menu-item', onclick: () => { closeMenu(); this.chooseWorkspace(); } },
      icon('folder-opened'), h('span', { class: 'menu-main' }, h('span', { class: 'name', text: 'Choose a workspace file…' }))));
    menuEl.append(h('div', { class: 'menu-note', text: 'Applies to every session in this folder.' }));
  }

  async chooseWorkspace() {
    const current = workspaceFor(this.cwd);
    const dir = current ? parentDir(current) : workspaceDirs()[0] || this.cwd;
    const path = await invoke('pick_workspace', { dir }).catch(() => null);
    if (path) setWorkspace(this.cwd, path);
  }

  // ---------- progress ----------

  /** Picks the line over the prompt box: compaction or the wrap-up allowance when either is on, else the next verb. */
  nextBusyWord() {
    this.busyWord = this.compacting ? 'Compacting the conversation…'
      : this.wrapUp ? (this.wrapUp.credits ? 'Usage limit reached · continuing on usage credits…' : 'Usage limit reached · wrapping up…')
      : nextBusyVerb(this.busyWord);
    for (const el of this.el.querySelectorAll('.busy-word')) el.textContent = this.busyWord;
  }

  /**
   * The wrap-up allowance: when the plan's 5-hour limit is hit mid-reply, Claude Code (2.1.277+) gets a small
   * allowance to finish the reply instead of stopping dead. It is automatic; the CLI asks for it and tells
   * Claude to wrap up. HQ only shows it, and must not cut the turn short, since a stop ends the allowance.
   * `rateLimitGraceActive` is internal to the CLI and outlives a refused request, so it counts only with a
   * status other than 'rejected' and before `resetsAt` (seconds).
   */
  onRateLimit(info) {
    if (!info || typeof info !== 'object') return;
    // An event about another window (weekly, overage) says nothing about the 5-hour wrap-up; it must not end it.
    if (info.rateLimitGraceActive !== true && info.rateLimitType && info.rateLimitType !== 'five_hour') return;
    const resetsAt = Number(info.resetsAt) * (Number(info.resetsAt) < 1e12 ? 1000 : 1);
    const grace = info.rateLimitGraceActive === true && info.status !== 'rejected'
      && !(Number.isFinite(resetsAt) && resetsAt > 0 && resetsAt < Date.now());
    this.setWrapUp(grace ? { credits: info.overageStatus === 'allowed' || info.overageStatus === 'allowed_warning' } : null);
  }

  setWrapUp(state) {
    const was = this.wrapUp;
    this.wrapUp = state;
    if (!was && state && this.status === 'running') {
      this.notice(state.credits ? 'Usage limit reached: Claude is finishing this reply, then continuing on usage credits.'
        : 'Usage limit reached: Claude is using its wrap-up allowance to finish this reply. Stopping it now ends the allowance.');
    }
    if (!!was !== !!state || (was && state && was.credits !== state.credits)) {
      this.el.classList.toggle('wrapping-up', !!state);
      if (this.status === 'running') this.nextBusyWord();
    }
  }

  /** A new turn restarts the clock and sets aside a task list that was already finished. */
  startTurn() {
    this.nextBusyWord();
    this.turnStart = Date.now();
    this.limitHit = false;
    this.turnTools = 0;
    this.currentTool = null;
    this.sealTasks();
  }

  /** A task list that is all done belongs to the last prompt: the next task starts a new one. */
  sealTasks() {
    const list = this.taskItems();
    if (list.length && list.every(t => t.status === 'completed')) this.tasksStale = true;
  }

  taskItems() {
    return [...this.tasks.values(), ...this.taskCreates.values()];
  }

  /**
   * Follows Claude's task list the way Claude Code rebuilds it from a transcript: only top-level
   * calls count, a TaskCreate gets its id from its result, and TodoWrite (the older tool, used
   * when CLAUDE_CODE_ENABLE_TASKS=false) sends the whole list each time. HQ starts Claude with
   * CLAUDE_CODE_ENABLE_TODO_TOOLS=1, so newer models get TaskCreate/TaskUpdate too. Shapes checked
   * against Claude Code 2.1.301: TaskCreate {subject, description, activeForm} is answered with
   * "Task #N created successfully: ..."; TaskUpdate is {taskId, status, ...}. Resume replays the
   * same calls, which rebuilds the list. Without the tools the panel relies on Sitrep.
   */
  trackTaskCall(block) {
    const input = block.input || {};
    if (block.name === 'TaskCreate' && typeof input.subject === 'string') {
      if (this.tasksStale) { // the first task after a finished list starts a new list
        this.tasks.clear();
        this.taskCreates.clear();
        this.tasksStale = false;
      }
      this.taskCreates.set(block.id, { subject: input.subject, activeForm: input.activeForm || '', status: 'pending' });
    } else if (block.name === 'TaskUpdate') {
      const id = String(input.taskId ?? input.id ?? '');
      if (!id) return;
      this.tasksStale = false;
      if (input.status === 'deleted') {
        this.tasks.delete(id);
      } else {
        const old = this.tasks.get(id);
        this.tasks.set(id, {
          subject: input.subject || (old && old.subject) || `Task ${id}`,
          activeForm: input.activeForm || (old && old.activeForm) || '',
          status: input.status || (old && old.status) || 'pending',
        });
      }
    } else if (block.name === 'TodoWrite' && Array.isArray(input.todos)) {
      this.tasks = new Map(input.todos.map((t, i) => [String(i), { subject: t.content || '', activeForm: t.activeForm || '', status: t.status || 'pending' }]));
      this.taskCreates.clear();
      this.tasksStale = false;
    }
  }

  /** TaskCreate's result names the new task: "Task #3 created successfully: …". */
  settleTaskCreate(block) {
    const task = this.taskCreates.get(block.tool_use_id);
    if (!task) return;
    const id = toolResultText(block.content).match(/^Task #(\S+) created successfully/);
    if (!block.is_error && !id) return; // unrecognised reply: keep showing it, as Claude Code does
    this.taskCreates.delete(block.tool_use_id);
    if (id && !this.tasks.has(id[1])) this.tasks.set(id[1], task);
    this.renderProgress();
  }

  /** task_progress: a running agent's latest tool, tool count and summary. */
  onTaskProgress(m) {
    if (!m.task_id) return;
    const u = m.usage || {};
    this.taskProgress.set(String(m.task_id), {
      toolUseId: m.tool_use_id || null,
      description: firstLine(m.description || '', 120),
      toolUses: Number.isFinite(u.tool_uses) ? u.tool_uses : null,
      durationMs: Number.isFinite(u.duration_ms) ? u.duration_ms : null,
      lastTool: typeof m.last_tool_name === 'string' ? m.last_tool_name : '',
      summary: typeof m.summary === 'string' ? firstLine(m.summary, 200) : '',
    });
    this.renderProgress();
  }

  /** Forgets agents that are neither in the background list nor a tool call still running. */
  pruneTaskProgress() {
    for (const [id, p] of this.taskProgress) {
      const t = p.toolUseId && this.tools.get(p.toolUseId);
      if (!this.bgTasks.some(x => x.id === id) && !(t && t.item.classList.contains('pending'))) this.taskProgress.delete(id);
    }
  }

  /** Asks the agent, beside the running turn, for a quick sitrep; it answers without stopping its work. */
  async askStatus() {
    if (this.proc == null || (this.statusAnswer && this.statusAnswer.asking)) return;
    this.statusAnswer = { ...this.statusAnswer, asking: true };
    this.renderProgress();
    const reply = await this.control({ subtype: 'side_question', question: STATUS_QUESTION });
    const r = reply && reply.subtype === 'success' && reply.response;
    let text = r && typeof r.response === 'string' ? r.response.trim() : '';
    let percent = null;
    // Tolerates markdown around the label: "**Progress:** 40%", "Progress - 40%", "# Progress: 40 %".
    const m = text.match(/^[\s>#*_`-]*Progress\b[\s:*_`–—-]*(\d{1,3})\s*%[^\n]*\n?/i);
    if (m) {
      percent = Math.min(100, Number(m[1]));
      text = text.slice(m[0].length).trim();
      // "Nothing in progress" while the turn runs contradicts the status, so show the words without a percentage.
      // Only the label line and the first sentence count: a later "no background work is running" is a detail.
      if (this.status === 'running' && /\b(?:nothing|no (?:task|work|request)s?)\s+(?:is |are |was |currently |actively )*(?:in progress|running|underway|being worked)/i.test(m[0] + text.split(/[.!?](?:\s|$)|\n/, 1)[0])) percent = null;
    }
    const name = this.backend === 'hermes' ? 'Hermes' : 'Claude';
    const error = !text && percent == null;
    if (error) text = reply && reply.subtype === 'error' ? `Could not ask: ${reply.error || `rejected by ${name}`}` : `${name} gave no answer.`;
    this.statusAnswer = { text, percent, error, at: Date.now(), asking: false };
    this.renderProgress();
  }

  /** Stops one background agent or shell, or an agent Claude is waiting on; the rest of the turn goes on. */
  async stopTask(id) {
    const key = String(id);
    if (this.proc == null || this.stoppingTasks.has(key)) return;
    this.stoppingTasks.add(key);
    this.renderProgress();
    const epoch = this.epoch;
    const reply = await this.control({ subtype: 'stop_task', task_id: key });
    if (epoch !== this.epoch) return; // /clear started over meanwhile
    this.stoppingTasks.delete(key);
    if (reply && reply.subtype === 'error') this.notice(`Could not stop the task: ${reply.error || 'rejected by Claude'}`, true);
    this.renderProgress();
  }

  /** The elapsed time ticks once a second while the panel is open and Claude is working, and the cell can be seen. */
  syncProgressTimer() {
    const need = (this.progressView || this.wmView) && (this.status === 'running' || this.bgTasks.length > 0) && !this.collapsed && !document.hidden;
    const tick = () => {
      const el = (this.wmView ? this.waitBody : this.progressEl).querySelector('.pv-elapsed');
      if (el && this.turnStart) el.textContent = formatDuration(Date.now() - this.turnStart);
    };
    if (need && !this.progressTimer) {
      tick(); // catch up at once on coming back into view
      this.progressTimer = setInterval(tick, 1000);
    } else if (!need && this.progressTimer) {
      clearInterval(this.progressTimer);
      this.progressTimer = null;
    }
  }

  /** Redraws at most once per frame: every stream event and task update calls this. */
  renderProgress() {
    if (this.replaying || this.progressFrame) return; // loadHistory draws it once the transcript is in
    this.progressFrame = requestAnimationFrame(() => {
      this.progressFrame = 0;
      this.drawProgress();
    });
  }

  drawProgress() {
    if (this.replaying) return;
    const list = this.taskItems();
    const live = list.length > 0 && !this.tasksStale;
    const done = list.filter(t => t.status === 'completed').length;
    const active = list.filter(t => t.status === 'in_progress').length;
    const taskPct = list.length ? Math.round(((done + active / 2) / list.length) * 100) : 0;
    const running = this.status === 'running';
    // A thin bar along the header, so progress shows in the grid without opening the panel.
    const headBar = live && (running || taskPct < 100);
    this.headBar.classList.toggle('hidden', !headBar);
    if (headBar) this.headBar.style.setProperty('--pct', `${taskPct}%`);
    this.drawTaskLine(list, headBar, done);
    this.syncProgressTimer();
    const asWaiting = this.wmView; // the waiting view takes the panel's place (the chat is hidden under both)
    if (!this.progressView && !asWaiting) return;

    const waiting = this.pending.size > 0;
    const [state, stateCls] = waiting ? ['Waiting for you', 'waiting']
      : running ? ['Working', 'running']
      : this.bgTasks.length ? ['Background work running', 'bg']
      : this.status === 'exited' ? ['Stopped', 'exited']
      : ['Idle', ''];
    const answer = this.statusAnswer;
    const agent = this.backend === 'hermes' ? 'Hermes' : 'Claude';
    const estimate = answer && answer.percent != null && this.turnStart && answer.at >= this.turnStart ? answer.percent : null;
    const pct = live ? taskPct : estimate;
    const barLabel = live ? `${taskPct}%  ·  ${done} of ${list.length} tasks done`
      : estimate != null ? `About ${estimate}%  ·  ${agent}'s estimate`
      : running ? 'No task list. Ask for a sitrep to get a rough percentage.' : '';

    const ask = h('button', { class: 'btn pv-ask', disabled: this.proc == null || (answer && answer.asking),
      title: this.proc == null ? `${agent} starts when you send a message` : `Asks ${agent} for a quick progress report on what it is doing and what is left, without interrupting it` },
    icon(answer && answer.asking ? 'loading codicon-modifier-spin' : 'comment-discussion'), answer && answer.asking ? 'Getting sitrep…' : 'Sitrep');
    ask.addEventListener('click', () => this.askStatus());

    const turnActive = (running || waiting) && this.turnStart;
    const head = h('div', { class: 'pv-head' },
      h('span', { class: `pv-state ${stateCls}` }, h('span', { class: 'pv-dot' }), state),
      turnActive ? h('span', { class: 'pv-elapsed', text: formatDuration(Date.now() - this.turnStart) }) : null,
      turnActive && this.turnTools ? h('span', { text: `${this.turnTools} tool call${this.turnTools === 1 ? '' : 's'}` }) : null,
      h('span', { class: 'spacer' }),
      ask,
      asWaiting
        ? h('button', { class: 'icon-btn pv-close', title: 'Show the chat (turns waiting mode off)', onclick: () => this.setWaitingMode(false) }, icon('comment'))
        : h('button', { class: 'icon-btn pv-close', title: 'Close', onclick: () => this.setProgressView(false) }, icon('close')));

    let bar = null;
    if (pct != null || (running && !asWaiting)) { // the waiting view's loader stands in for the indeterminate bar
      const fill = h('span');
      if (pct != null) fill.style.setProperty('--pct', `${pct}%`);
      bar = h('div', { class: 'pv-progress' }, h('div', { class: `pv-bar${pct == null ? ' indeterminate' : ''}` }, fill),
        barLabel ? h('div', { class: 'pv-label', text: barLabel }) : null);
    }

    let now = null;
    if (waiting) now = h('div', { class: 'pv-now' }, icon('bell'), `${agent} needs your answer or permission below.`);
    else if (running) {
      const t = this.currentTool;
      now = h('div', { class: 'pv-now' }, icon('debug-stackframe'),
        t ? [h('span', { class: 'tname', text: t.name }), h('span', { class: 'targ', text: t.summary })] : h('span', { text: 'Thinking or writing' }));
    }

    let tasks = null;
    if (list.length) {
      const iconFor = { completed: 'pass-filled', in_progress: 'loading codicon-modifier-spin', pending: 'circle-large-outline' };
      tasks = h('div', { class: 'pv-section' },
        h('div', { class: 'pv-title', text: this.tasksStale ? 'Previous task list' : 'Tasks' }),
        h('ul', { class: `pv-tasks${this.tasksStale ? ' stale' : ''}` }, list.map(t => h('li', { class: t.status },
          icon(iconFor[t.status] || 'circle-large-outline'),
          h('span', { text: t.status === 'in_progress' && t.activeForm ? t.activeForm : t.subject }),
        ))));
    }

    const agents = this.bgTasks.map(t => ({ id: t.id, label: `${BG_LABELS[t.type] || 'Task'}: ${t.description}`, p: this.taskProgress.get(t.id) }));
    for (const [id, p] of this.taskProgress) {
      if (!this.bgTasks.some(t => t.id === id)) agents.push({ id, label: `Agent: ${p.description}`, p });
    }
    const agentsEl = agents.length ? h('div', { class: 'pv-section' },
      h('div', { class: 'pv-title', text: 'Agents and shells' }),
      h('ul', { class: 'pv-agents' }, agents.map(({ id, label, p }) => {
        const detail = p ? [p.lastTool && `last: ${p.lastTool}`, p.toolUses != null && `${p.toolUses} tool uses`,
          p.durationMs != null && formatDuration(p.durationMs)].filter(Boolean).join('  ·  ') : '';
        const stopping = this.stoppingTasks.has(String(id));
        const stop = h('button', { class: 'icon-btn pv-stop', disabled: stopping || this.proc == null,
          title: stopping ? 'Stopping…' : 'Stop this agent or shell', onclick: () => this.stopTask(id) }, icon(stopping ? 'loading codicon-modifier-spin' : 'debug-stop'));
        return h('li', null, h('span', { class: 'codicon codicon-loading codicon-modifier-spin' }),
          h('div', { class: 'pv-agent-main' }, h('div', { text: label }), detail ? h('div', { class: 'desc', text: detail }) : null,
            p && p.summary ? h('div', { class: 'desc', text: p.summary }) : null), stop);
      }))) : null;

    const answerEl = answer && answer.at ? h('div', { class: 'pv-section' },
      h('div', { class: 'pv-title' }, `${agent} says`, stamp(answer.at)),
      h('div', { class: `pv-answer${answer.error ? ' error' : ''}`, text: answer.text })) : null;

    const empty = !bar && !now && !tasks && !agentsEl
      ? h('div', { class: 'pv-empty', text: 'Nothing is running. Progress shows here while Claude works.' }) : null;

    const parts = asWaiting ? [bar, head, now, tasks, agentsEl, answerEl, empty] : [head, bar, now, tasks, agentsEl, answerEl, empty];
    (asWaiting ? this.waitBody : this.progressEl).replaceChildren(...parts.filter(Boolean));
  }

  /** "3/7 · Creating b.txt" for the header and the sessions list; touches the DOM only when it changes. */
  drawTaskLine(list, show, done) {
    let line = '';
    let side = '';
    if (show) {
      const cur = list.find(t => t.status === 'in_progress');
      const t = this.currentTool;
      const task = cur ? cur.activeForm || cur.subject : '';
      const what = task || (this.status === 'running' && t ? `${t.name} ${t.summary}` : '');
      line = `${done}/${list.length}${what ? ` · ${firstLine(what, 140)}` : ''}`;
      side = `${done}/${list.length}${task ? ` · ${firstLine(task, 140)}` : ''}`;
    }
    if (line !== this.taskLine) {
      this.taskLine = line;
      this.taskEl.textContent = line;
      this.taskEl.title = line ? `Tasks: ${line}` : '';
      this.taskEl.classList.toggle('hidden', !line);
    }
    if (side !== this.sideTaskLine) {
      this.sideTaskLine = side;
      renderSidebar();
    }
  }

  // ---------- waiting mode ----------

  setWaitingMode(on) {
    if (this.wmArmed === on) return;
    this.wmArmed = on;
    this.wmWorked = on && (this.status === 'running' || this.bgTasks.length > 0);
    this.applyWaitingMode();
    this.syncWaiting();
  }

  applyWaitingMode() {
    this.waitBtn.classList.toggle('on', this.wmArmed);
    this.waitBtn.title = this.wmArmed ? 'Waiting mode is on. Click to turn it off and show the chat.' : 'Waiting mode';
  }

  /**
   * Decides what the cell shows; runs on every state change (refreshState), never on a timer
   * except the single timeout below. The waiting view stands in for the chat while Claude works
   * (a turn, a background agent or shell). A question for the user shows the chat, which has its
   * context. Idle is not finished: a background task's notification often starts a new turn, so
   * waiting mode ends only after the cell stays idle, with nothing in the background or pending,
   * for WAIT_SETTLE_MS; any state change in between starts that count over.
   */
  syncWaiting() {
    if (this.closing) return;
    let view = false;
    let settling = false;
    if (this.wmArmed && this.pending.size === 0) {
      if (this.status === 'running' || this.bgTasks.length > 0) {
        this.wmWorked = true;
        view = true;
      } else if (this.wmWorked) {
        view = true;
        settling = true;
      }
    }
    if (settling && this.wmTimer) return; // still idle: a refresh with no new work (a focus, say) does not extend the wait
    clearTimeout(this.wmTimer);
    this.wmTimer = 0;
    if (settling) this.wmTimer = setTimeout(() => { this.wmTimer = 0; this.setWaitingMode(false); }, WAIT_SETTLE_MS);
    this.setWmView(view);
  }

  setWmView(view) {
    if (view === this.wmView) return;
    if (view) {
      // Set once per opening, outside wv-body, so redrawing the progress never restarts the animation.
      this.loader = nextLoader(this.loader);
      const [name, desc, svg] = this.loader;
      this.waitEl.querySelector('.wv-loader').innerHTML = `<svg viewBox="0 0 40 40">${svg}</svg>`;
      this.waitEl.querySelector('.wv-hero').title = `${name}: ${desc}`;
      // a display:none scroller forgets where it was; a collapsed cell kept it in savedScroll
      [this.wmTop, this.wmStick] = this.collapsed && this.savedScroll ? this.savedScroll : [this.msgs.scrollTop, this.stickToBottom];
    }
    this.wmView = view;
    this.el.classList.toggle('wm-on', view);
    if (!view && !this.collapsed) {
      if (this.live) this.scheduleLiveRender(); // text that streamed in while hidden was not drawn
      this.stickToBottom = this.wmStick;
      if (this.wmStick) this.pinToBottom();
      else this.msgs.scrollTop = this.wmTop;
    }
    this.renderProgress(); // the view and the panel share the data, not the element
  }

  /** The chat is not on screen: collapsed to the dock, or replaced by the waiting view. */
  get chatHidden() {
    return this.collapsed || this.wmView;
  }

  /** The composer's task-list toggle: the next prompt carries a nudge to keep a list. One-shot. */
  setAskTasks(on) {
    this.askTasks = on;
    this.taskBtn.classList.toggle('on', on);
  }

  // ---------- images ----------

  async addImages(files) {
    for (const file of files) {
      if (!file.type.startsWith('image/')) {
        this.notice(`Only images can be attached (${file.name || file.type || 'file'} skipped).`, true);
        continue;
      }
      try {
        this.attachments.push(await prepareImage(file));
      } catch (err) {
        this.notice(`Could not attach ${file.name || 'image'}: ${err.message || err}`, true);
      }
    }
    this.renderAttachments();
    this.input.focus();
  }

  renderAttachments() {
    this.attachEl.replaceChildren(...this.attachments.map((a, i) => {
      const remove = h('button', { class: 'icon-btn remove', title: 'Remove image' }, icon('close'));
      remove.addEventListener('click', () => {
        this.attachments.splice(i, 1);
        this.renderAttachments();
      });
      return h('div', { class: 'attachment' }, h('img', { src: a.url, alt: 'attached image' }), remove);
    }));
  }

  // ---------- model and effort ----------

  /** Keeps a pick made in this cell as the default for the next new session of the same agent. */
  remember(choice) {
    if (this.backend === 'hermes') hermesRemember(choice);
    else rememberChoice(choice);
  }

  models() {
    return this.backend === 'hermes' ? hermesModels() : modelList();
  }

  modelLabel() {
    const list = this.models();
    if (this.modelChoice) {
      const m = list.find(x => x.value === this.modelChoice);
      return (m && m.displayName) || this.modelChoice;
    }
    // Default: name the model it resolves to, e.g. "Opus 5.5", even before this cell has started.
    const fallback = list.find(x => x.value === 'default');
    const target = this.resolvedModel || (fallback && fallback.resolvedModel);
    const resolved = target && list.find(x => x.value !== 'default' && x.resolvedModel === target);
    return resolved ? resolved.displayName : target || 'Default model';
  }

  /** Effort levels the chosen model accepts; empty when it has no effort setting. */
  effortLevels() {
    if (this.backend === 'hermes') return []; // Hermes has no effort setting
    // Searches the unfiltered list: a saved choice outside ALLOWED_MODELS still has its real effort levels.
    const m = (app.models.length ? app.models : FALLBACK_MODELS).find(x => x.value === (this.modelChoice || 'default'));
    if (!m || !app.models.length) return ALL_EFFORTS; // unknown yet: let Claude decide
    if (!m.supportsEffort) return [];
    return Array.isArray(m.supportedEffortLevels) && m.supportedEffortLevels.length ? m.supportedEffortLevels : ALL_EFFORTS;
  }

  updateModelLabel() {
    const effort = this.effortLevels().length ? this.effort || this.appliedEffort : null;
    this.modelBtn.querySelector('.label').textContent = effort ? `${this.modelLabel()} · ${effort}` : this.modelLabel();
    this.modelBtn.title = `Model: ${this.resolvedModel || this.modelChoice || 'default'}${effort ? `\nEffort: ${effort}` : ''}`;
    if (menuState.cell === this) refreshMenu();
  }

  async setModel(value) {
    const previous = this.modelChoice;
    this.modelChoice = value && value !== 'default' ? value : null;
    this.updateModelLabel();
    saveState();
    if (this.proc == null) return; // applied with --model on the next spawn
    const reply = await this.control({ subtype: 'set_model', model: this.modelChoice || 'default' });
    if (reply && reply.subtype === 'error') {
      this.notice(`Could not switch model: ${reply.error || 'rejected by Claude'}`, true);
      this.modelChoice = previous;
      this.updateModelLabel();
      saveState();
      return;
    }
    await this.refreshApplied();
  }

  async setEffort(level) {
    const previous = this.effort;
    this.effort = level;
    this.updateModelLabel();
    saveState();
    if (this.proc == null) return; // applied with --effort on the next spawn
    const reply = await this.control({ subtype: 'apply_flag_settings', settings: { effortLevel: level } });
    if (reply && reply.subtype === 'error') {
      this.notice(`Could not change effort: ${reply.error || 'rejected by Claude'}`, true);
      this.effort = previous;
      this.updateModelLabel();
      saveState();
      return;
    }
    await this.refreshApplied();
  }

  /** Reads the model and effort Claude is actually using. Only the `applied` summary is kept. */
  async refreshApplied() {
    const reply = await this.control({ subtype: 'get_settings' });
    const applied = reply && reply.subtype === 'success' && reply.response && reply.response.applied;
    if (!applied) return;
    if (typeof applied.model === 'string') this.resolvedModel = applied.model;
    if (typeof applied.effort === 'string') this.appliedEffort = applied.effort;
    this.updateModelLabel();
  }

  buildModelMenu(menuEl) {
    const current = this.modelChoice || 'default';
    const list = [...this.models()];
    if (!list.some(m => m.value === current)) list.push({ value: current, displayName: current });
    menuEl.append(h('div', { class: 'menu-title', text: 'Model' }));
    for (const m of list) {
      const checked = m.value === current;
      menuEl.append(h('button', { class: `menu-item${checked ? ' checked' : ''}`, onclick: () => this.setModel(m.value).then(() => this.remember({ model: this.modelChoice })) },
        icon(checked ? 'check' : 'blank'),
        h('span', { class: 'menu-main' }, h('span', { class: 'name', text: m.displayName || m.value }), m.description ? h('span', { class: 'desc', text: m.description }) : null)));
    }
    if (this.backend === 'hermes') return; // Hermes has no effort setting
    menuEl.append(h('div', { class: 'menu-title', text: 'Effort' }));
    const levels = this.effortLevels();
    if (!levels.length) {
      menuEl.append(h('div', { class: 'menu-note', text: 'This model has no effort setting.' }));
      return;
    }
    const active = this.effort || this.appliedEffort;
    menuEl.append(h('div', { class: 'segmented' }, levels.map(level =>
      h('button', { class: level === active ? 'on' : '', onclick: () => this.setEffort(level).then(() => this.remember({ effort: this.effort })) }, level))));
    if (!this.effort) menuEl.append(h('div', { class: 'menu-note', text: active ? `Using your default (${active}).` : 'Using your default effort.' }));
  }

  // ---------- MCP servers ----------

  async loadMcp() {
    this.mcpServers = null;
    this.mcpError = null;
    if (menuState.cell === this) refreshMenu();
    if (!(await this.ensureProcess())) {
      this.mcpError = 'Claude could not start.';
    } else {
      const reply = await this.control({ subtype: 'mcp_status' });
      if (!reply) this.mcpError = 'Claude stopped.';
      else if (reply.subtype === 'error') this.mcpError = reply.error || 'Claude could not list MCP servers.';
      else this.mcpServers = sanitizeMcp(reply.response && reply.response.mcpServers);
    }
    if (this.mcpServers) this.setMcpBadge(this.mcpServers);
    if (menuState.cell === this) refreshMenu();
  }

  async mcpAction(request, verb) {
    const reply = await this.control(request);
    if (reply && reply.subtype === 'error') {
      this.mcpError = `Could not ${verb} ${request.serverName}: ${reply.error || 'rejected by Claude'}`;
      if (menuState.cell === this) refreshMenu();
      return;
    }
    await this.loadMcp();
  }

  setMcpBadge(servers) {
    const bad = servers.filter(s => s.status === 'failed' || s.status === 'needs-auth');
    this.mcpBtn.classList.toggle('warn', bad.length > 0);
    this.mcpBtn.title = bad.length ? `MCP servers: ${bad.map(s => `${s.name} (${s.status})`).join(', ')}` : 'MCP servers';
  }

  buildMcpMenu(menuEl) {
    const refresh = h('button', { class: 'icon-btn', title: 'Refresh', onclick: () => this.loadMcp() }, icon('refresh'));
    menuEl.append(h('div', { class: 'menu-title row' }, h('span', { text: 'MCP servers' }), h('span', { class: 'spacer' }), refresh));
    if (this.mcpError) menuEl.append(h('div', { class: 'menu-note error', text: this.mcpError }));
    if (!this.mcpServers) {
      if (!this.mcpError) menuEl.append(h('div', { class: 'menu-note', text: this.proc == null ? 'Starting Claude…' : 'Loading…' }));
      return;
    }
    if (!this.mcpServers.length) {
      menuEl.append(h('div', { class: 'menu-note', text: 'No MCP servers are configured for this folder.' }));
      return;
    }
    for (const s of this.mcpServers) {
      const enabled = s.status !== 'disabled';
      const toggle = h('button', { class: `switch${enabled ? ' on' : ''}`, title: enabled ? 'Disable' : 'Enable', role: 'switch', 'aria-checked': String(enabled) });
      toggle.addEventListener('click', () => this.mcpAction({ subtype: 'mcp_toggle', serverName: s.name, enabled: !enabled }, enabled ? 'disable' : 'enable'));
      const reconnect = enabled ? h('button', { class: 'icon-btn', title: 'Reconnect' }, icon('debug-restart')) : null;
      if (reconnect) reconnect.addEventListener('click', () => this.mcpAction({ subtype: 'mcp_reconnect', serverName: s.name }, 'reconnect'));
      const detail = [s.status, s.source || s.scope, s.tools ? `${s.tools} tools` : ''].filter(Boolean).join(' · ');
      menuEl.append(h('div', { class: `mcp-row mcp-${s.status}` },
        h('span', { class: 'mcp-dot' }),
        h('span', { class: 'menu-main' }, h('span', { class: 'name', text: s.name, title: s.name }), h('span', { class: 'desc', text: detail }),
          s.error ? h('span', { class: 'desc error', text: s.error }) : null),
        reconnect, toggle));
    }
  }

  // ---------- state ----------

  setStatus(status) {
    if (status === 'running' && this.status !== 'running') this.startTurn();
    this.status = status;
    this.refreshState();
  }

  refreshState() {
    const waiting = this.pending.size > 0;
    const el = this.el;
    el.classList.toggle('state-running', this.status === 'running' && !waiting);
    el.classList.toggle('state-waiting', waiting);
    el.classList.toggle('state-exited', this.status === 'exited');
    el.classList.toggle('state-bg', this.bgTasks.length > 0 && this.status !== 'running' && !waiting);
    el.classList.toggle('unread', this.unread && !waiting);
    this.stopBtn.classList.toggle('hidden', this.status !== 'running' && !waiting);
    this.bgEl.classList.toggle('hidden', !this.bgTasks.length);
    this.stateEl.textContent = stateOf(this)[1]; // shown while collapsed, where the chat cannot say it
    if (this.bgTasks.length) {
      this.bgEl.querySelector('.bg-count').textContent = String(this.bgTasks.length);
      const lines = this.bgTasks.map(t => `${BG_LABELS[t.type] || 'Task'}: ${t.description}`);
      this.bgEl.title = ['Running in the background:', ...lines].join('\n');
    }
    this.syncWaiting();
    this.renderProgress();
    this.renderContext(); // the button waits while a turn runs
    updateSummary();
  }

  /** Anything that would be lost if this cell's claude stopped now. */
  busy() {
    return this.status === 'running' || this.pending.size > 0 || this.bgTasks.length > 0;
  }

  setBackground(tasks) {
    this.bgTasks = tasks;
    this.pruneTaskProgress();
    this.refreshState();
  }

  /** Keeps a backgrounded tool call marked as running until its task reports back. */
  onTaskEvent(m) {
    const t = m.tool_use_id && this.tools.get(m.tool_use_id);
    if (m.subtype === 'task_started') {
      if (t && m.is_backgrounded) t.item.classList.add('bg');
      return;
    }
    // task_notification: the task is done, even if a background_tasks_changed was missed.
    if (t) {
      t.item.classList.remove('bg');
      if (m.status === 'failed') t.item.classList.replace('ok', 'err');
    }
    this.taskProgress.delete(String(m.task_id));
    if (this.bgTasks.some(x => x.id === m.task_id)) this.setBackground(this.bgTasks.filter(x => x.id !== m.task_id));
    else this.renderProgress();
  }

  markUnread() {
    const seen = document.hasFocus() && app.focused === this && !this.collapsed && !document.hidden;
    if (!seen) {
      this.unread = true;
      invoke('request_attention').catch(() => {});
    }
  }

  // ---------- process ----------

  async ensureProcess() {
    if (this.proc != null) return true;
    if (this.spawning) return this.spawning;
    this.spawning = (async () => {
      if (this.backend === 'claude') updateClaude(); // runs in the background; a spawn does not wait for it
      this.spawnedFresh = !this.sessionId; // Remote Control waits for a new session's first prompt
      try {
        for (let attempt = 0; ; attempt++) {
          try {
            this.proc = await this.spawnProcess();
            break;
          } catch (err) {
            // The spawn can fail while `claude update` replaces the binary: wait for it, then try once more.
            if (attempt === 0 && this.backend === 'claude' && claudeUpdate.promise) {
              this.metaEl.textContent = 'Updating Claude Code…';
              await withTimeout(claudeUpdate.promise, CLAUDE_UPDATE_WAIT_MS);
              if (this.metaEl.textContent === 'Updating Claude Code…') this.metaEl.textContent = '';
              continue;
            }
            this.notice(String(err), true);
            this.setStatus('exited');
            return false;
          }
        }
      } finally {
        this.spawning = null;
      }
      if (this.closing) { // cell was closed while claude was starting
        invoke('kill_session', { id: this.proc });
        this.proc = null;
        return false;
      }
      this.cost.procUsd = 0;
      this.cost.procTokens = 0;
      this.remoteAsked = false;
      this.initialize();
      return true;
    })();
    return this.spawning;
  }

  spawnProcess() {
    const channel = new Channel();
    channel.onmessage = batch => this.onLine(batch);
    return invoke('spawn_session', {
      cwd: this.cwd,
      resume: this.sessionId,
      permissionMode: this.permissionMode,
      model: this.modelChoice,
      effort: this.effortLevels().length ? this.effort : null,
      fork: this.fork,
      backend: this.backend,
      autoTasks: app.settings.autoTasks !== false,
      onEvent: channel,
    });
  }

  async initialize() {
    const reply = await this.control({ subtype: 'initialize' });
    const r = reply && reply.subtype === 'success' && reply.response;
    if (this.backend === 'hermes') { // keeps its own lists, and has no Remote Control
      if (r) hermesInitialized(r);
      await this.refreshApplied();
      return;
    }
    if (r) {
      if (Array.isArray(r.models) && r.models.length) {
        app.models = sanitizeModels(r.models);
        saveState();
      }
      if (Array.isArray(r.commands)) app.commands = sanitizeCommands(r.commands);
      if (typeof r.remote_control_available === 'boolean') app.remoteAvailable = r.remote_control_available;
      if (app.remoteDefault == null && typeof r.remote_control_auto_enable === 'boolean') {
        app.remoteDefault = r.remote_control_auto_enable; // seeded once from the account, then the user's choice
        saveState();
      }
    }
    if (this.remote == null) {
      this.remote = !!app.remoteDefault;
      saveState();
    }
    renderRemoteButtons();
    // A session that already has a transcript has its title and messages, so Remote Control can start now.
    if (this.remote && (!this.spawnedFresh || this.promptWritten)) this.autoEnableRemote();
    await this.refreshApplied();
  }

  write(obj) {
    if (this.proc == null) return Promise.resolve();
    return invoke('send_line', { id: this.proc, line: JSON.stringify(obj) }).then(() => true, err => { this.notice(String(err), true); return false; }); // resolves to whether it was sent
  }

  /** Sends a control request; resolves with Claude's control_response (or null if the process ends). */
  control(request) {
    const requestId = nextRequestId();
    const reply = new Promise(resolve => this.controls.set(requestId, resolve));
    this.write({ type: 'control_request', request_id: requestId, request });
    return reply;
  }

  respond(requestId, response) {
    return this.write({ type: 'control_response', response: { subtype: 'success', request_id: requestId, response } });
  }

  /**
   * Sends the composer's text and images, or `prompt` instead (HQ's own), which leaves the composer
   * alone. `restore`: what of HQ's prompt goes back in the composer if it is never sent (the user's own words).
   */
  async submit(prompt = null, restore = '') {
    if (this.missing || this.clearing) return;
    if (this.historyLoading) await this.historyLoading; // the transcript is drawn first, so live messages follow it
    const text = prompt ?? this.input.value.trim();
    const images = prompt == null ? this.attachments : [];
    if (!text && !images.length) return;
    if (this.wmView && prompt == null) this.setWaitingMode(false); // sending is the user coming back (an armed idle cell keeps waiting for the work)
    // /clear (and its aliases) is answered here: Claude's own /clear would change the session
    // behind HQ's back. The cell starts a new session; the old one stays on disk for Resume.
    const clear = prompt == null && !images.length && /^\/(?:clear|reset|new)(?:\s+([\s\S]*))?$/.exec(text);
    if (clear) {
      if (this.status === 'running' || this.pending.size || this.spawning) {
        this.notice('Claude is still working. Stop it (Esc) before you clear the conversation.', true);
        return;
      }
      this.input.value = '';
      autoGrow(this.input);
      slashClose(this);
      await this.clearConversation(clear[1] || '');
      return;
    }
    for (const old of this.msgs.querySelectorAll('.item.user.unsent')) old.remove(); // a prompt that failed to send is replaced by this one
    const draft = { text, images, cancelled: false, bubble: null, addedPrompt: false, titled: false, hq: prompt != null, restore };
    if (prompt == null) {
      this.input.value = '';
      autoGrow(this.input);
      slashClose(this);
      saveState(); // the sent text is no longer a draft
      this.attachments = [];
      this.renderAttachments();
    }
    this.stickToBottom = true;
    // HQ's own prompt shows as a notice instead, and names nothing.
    if (prompt == null) draft.bubble = this.append(userBubble(text, images.map(i => i.url), Date.now()));
    if (prompt == null && !this.aiTitle && !this.customTitle && text && this.promptText.length < 2000) {
      this.promptText = `${this.promptText}\n${text}`.trim().slice(0, 2000);
      draft.addedPrompt = true;
    }
    if (!this.title && prompt == null) {
      this.setTitle(firstLine(text, 90) || 'Image');
      draft.titled = true;
      saveState();
    }
    if (prompt == null) this.unread = false;
    if (this.status !== 'running') this.queuedTexts = []; // a new turn: the agent has read everything sent before it
    else if (prompt == null) this.queuedTexts.push({ text, images: images.length, results: 0 });
    this.setStatus('running');
    this.drafts.add(draft);
    const started = await this.ensureProcess();
    this.drafts.delete(draft);
    if (draft.cancelled || this.closing) return;
    if (!started) {
      this.unsend(draft, true);
      return;
    }
    // Not on a slash command: Claude Code only runs one sent as the whole prompt.
    const hint = this.askTasks && prompt == null && !/^\/[\w:-]+(\s|$)/.test(text.trimStart()) ? [{ type: 'text', text: this.backend === 'hermes' ? HERMES_TASK_HINT : TASK_HINT }] : [];
    const content = images.length || hint.length
      ? [...images.map(i => ({ type: 'image', source: { type: 'base64', media_type: i.mediaType, data: i.data } })), ...(text ? [{ type: 'text', text }] : []), ...hint]
      : text;
    if (text) this.sentPrompts.push(text);
    this.promptWritten = true;
    const written = this.write({ type: 'user', message: { role: 'user', content }, parent_tool_use_id: null });
    // After the first prompt is written, so that Claude has a message to title the remote session from.
    if (this.remote && this.spawnedFresh) this.autoEnableRemote();
    this.lastSent = draft; // until Claude answers, so that a process that dies at startup gives the prompt back
    if (hint.length && await written) this.setAskTasks(false); // one prompt only; a failed send keeps the toggle for the retry
  }

  /** Picks up the work an update cut off: the transcript is drawn (even in a collapsed cell), then the agent is told to carry on. */
  async resumeAfterRestart() {
    if (this.closing || this.missing || !app.cells.includes(this)) return;
    if (this.historyPending) {
      this.historyPending = false;
      this.loadHistory();
    }
    if (this.historyLoading) await this.historyLoading; // drawn without the usual "resumes when you send" notices
    this.resumeAfterUpdate = false;
    const texts = this.resumeQueued;
    this.resumeQueued = [];
    // The user may have taken the cell over in the meantime (stagger and history load), or closed it.
    if (this.closing || this.clearing || this.proc != null || this.status === 'running' || !app.cells.includes(this)) {
      if (texts.length && !this.closing && this.proc == null) { // unread messages from before the update go back in the composer
        this.input.value = [...texts, this.input.value].filter(Boolean).join('\n\n');
        autoGrow(this.input);
        saveState();
      }
      return;
    }
    this.notice(RESUME_NOTICE);
    const queued = texts.map(t => `\n\n---\n${t}`).join('');
    this.submit(queued ? `${RESUME_PROMPT}\n\nMessages I sent during that turn, which you may not have read yet:${queued}` : RESUME_PROMPT, texts.join('\n\n'));
  }

  /**
   * Puts a prompt that was never sent back in the composer, and undoes the title and title text it
   * set, so a prompt Claude never saw cannot become the session's name. `keepBubble` marks the
   * bubble as not sent (a failed start); otherwise it is removed (the user pressed Esc).
   */
  unsend(draft, keepBubble) {
    if (draft.hq) {
      if (draft.restore) {
        this.input.value = this.input.value ? `${draft.restore}\n${this.input.value}` : draft.restore;
        autoGrow(this.input);
        saveState();
      }
      this.notice('This session could not carry on by itself. Send a message to continue it.', true);
      return;
    }
    const current = this.input.value;
    this.input.value = draft.text && current ? `${draft.text}\n${current}` : draft.text || current;
    autoGrow(this.input);
    this.attachments = [...draft.images, ...this.attachments];
    this.renderAttachments();
    if (draft.addedPrompt && this.promptText.endsWith(draft.text)) this.promptText = this.promptText.slice(0, this.promptText.length - draft.text.length).trim();
    if (draft.titled && !this.aiTitle && !this.customTitle) {
      this.setTitle('');
      saveState();
    }
    const inner = draft.bubble && draft.bubble.querySelector('.bubble');
    if (keepBubble && inner) {
      draft.bubble.classList.add('unsent');
      inner.append(h('div', { class: 'unsent-tag', text: 'Not sent' }));
    } else if (draft.bubble) {
      draft.bubble.remove();
    }
    saveState();
  }

  interrupt() {
    if (this.drafts.size && this.proc == null) {
      // Claude is still starting, so nothing was sent yet: take the prompts back.
      for (const draft of [...this.drafts].reverse()) { // last first, so each is put in front of the text already restored
        draft.cancelled = true;
        this.unsend(draft, false);
      }
      this.drafts.clear();
      this.setStatus('idle');
      return;
    }
    if (this.proc == null) return;
    this.control({ subtype: 'interrupt' });
    if (!this.interruptTimer) this.interruptTimer = setTimeout(() => this.offerForceStop(), FORCE_STOP_AFTER_MS);
  }

  /** The CLI normally answers an interrupt at once; when it does not, the user may end the process. */
  offerForceStop() {
    this.interruptTimer = null;
    if (this.proc == null || (this.status !== 'running' && !this.pending.size)) return;
    const note = this.notice('Claude has not stopped yet.');
    note.append(' ', h('button', { class: 'btn', text: 'Force stop', onclick: () => { this.clearForceNote(); this.forceStop(); } }));
    this.forceNote = note;
  }

  clearForceNote() {
    clearTimeout(this.interruptTimer);
    this.interruptTimer = null;
    if (this.forceNote) this.forceNote.remove();
    this.forceNote = null;
  }

  /** Ends the process. The session resumes (--resume) in a new one when the next message is sent. */
  forceStop() {
    if (this.proc == null) return;
    this.forced = true;
    invoke('kill_session', { id: this.proc });
  }

  /** Ends the process and starts the cell over as a new session in the same folder. `name` titles it. */
  async clearConversation(name) {
    this.clearing = true;
    try {
      await this.resetSession(name);
    } finally {
      this.clearing = false;
    }
  }

  async resetSession(name) {
    this.epoch = (this.epoch || 0) + 1; // a handoff still being written belongs to the old session
    if (this.proc != null) {
      this.quietExit = true;
      const exited = new Promise(resolve => { this.exitWaiter = resolve; });
      invoke('kill_session', { id: this.proc });
      await withTimeout(exited, 5000);
      this.exitWaiter = null;
      if (this.proc != null) { // the process did not end; quietExit stays set so its late exit says nothing
        this.notice('Could not clear: Claude did not stop. Try again.', true);
        return;
      }
    }
    for (const id of [...this.pending.keys()]) this.dropPrompt(id);
    Object.assign(this, {
      sessionId: null, fork: false, bridgeSessionId: null, aiTitle: false, renamePending: false, titleTries: 0,
      titleAsked: '', promptText: '', sentPrompts: [], lastReply: '', turnTexts: [], tasksStale: false, lastDuration: '',
      historyStart: 0, historyPending: false, statusAnswer: null, live: null, promptWritten: false, lastSent: null,
      remoteAsked: false, currentTool: null, turnStart: null, turnTools: 0, resumeWarning: null,
    });
    const title = name.replace(/\s+/g, ' ').trim();
    this.customTitle = title || null;
    this.renamePending = !!title; // saved in the new transcript once Claude has written it
    this.ctx = { tokens: null, window: null, info: null, infoAt: 0, loading: null, model: null };
    for (const m of [this.tools, this.tasks, this.taskCreates, this.taskProgress, this.openTools, this.stoppingTasks]) m.clear();
    this.msgs.replaceChildren();
    this.stickToBottom = true;
    this.setTitle('');
    this.setStatus('idle');
    this.renderContext();
    this.renderProgress();
    saveState();
    this.notice('Conversation cleared. The previous session stays on disk: Resume can open it again.');
  }

  async setMode(mode) {
    const previous = this.permissionMode;
    this.permissionMode = mode;
    this.modeSel.value = mode;
    saveState();
    if (this.proc == null) return; // applied with --permission-mode on the next spawn
    const reply = await this.control({ subtype: 'set_permission_mode', mode });
    if (reply && reply.subtype === 'error') {
      this.notice(`Could not switch mode: ${reply.error || 'rejected by Claude'}`, true);
      this.permissionMode = previous;
      this.modeSel.value = previous;
      saveState();
    }
  }

  requestClose(btn) {
    if (this.busy() && !btn.classList.contains('confirm')) {
      btn.classList.add('confirm');
      btn.title = this.status === 'running' || this.pending.size ? 'Claude is working. Click again to close.' : 'Background tasks are still running. Click again to stop them and close.';
      btn.style.color = 'var(--error)';
      setTimeout(() => { btn.classList.remove('confirm'); btn.style.color = ''; btn.title = 'Close session'; }, 3000);
      return;
    }
    closeCell(this);
  }

  dispose() {
    this.closing = true;
    if (menuState.cell === this) closeMenu();
    if (this.proc != null) invoke('kill_session', { id: this.proc });
    this.proc = null;
    cancelAnimationFrame(this.renderTimer);
    cancelAnimationFrame(this.scrollFrame);
    cancelAnimationFrame(this.progressFrame);
    clearTimeout(this.interruptTimer);
    clearInterval(this.progressTimer);
    clearTimeout(this.wmTimer);
  }

  // ---------- protocol ----------

  /** The backend sends batches: a JSON array of messages, with `hq_exit` last. A lone object or string is accepted too. */
  onLine(batch) {
    if (this.closing) return;
    let list = batch;
    if (typeof batch === 'string') {
      try { list = [JSON.parse(batch)]; } catch { return; }
    } else if (!Array.isArray(batch)) {
      list = [batch];
    }
    for (const m of list) {
      if (this.closing) return;
      if (!m || typeof m !== 'object') continue;
      try {
        this.handle(m, false);
      } catch (err) {
        console.error('render failed', err, m);
      }
    }
  }

  handle(m, replay) {
    if (this.lastSent && !replay && (m.type === 'stream_event' || m.type === 'assistant' || m.type === 'result' || (m.type === 'system' && m.subtype === 'init'))) this.lastSent = null;
    switch (m.type) {
      case 'system': return this.onSystem(m);
      case 'stream_event': return this.onStreamEvent(m);
      case 'assistant': return this.onAssistant(m, replay);
      case 'user': return this.onUser(m, replay);
      case 'result': return this.onResult(m);
      case 'control_request': return this.onControlRequest(m);
      case 'control_cancel_request': return this.dropPrompt(m.request_id);
      case 'control_response': return this.onControlResponse(m);
      case 'hq_exit': return this.onExit(m);
      case 'rate_limit_event':
        if (this.backend !== 'claude') return;
        this.onRateLimit(m.rate_limit_info);
        return scheduleUsageRefresh(this);
      case 'custom-title':
      case 'ai-title': return this.onTitleLine(m);
      default: return; // keep_alive, future types
    }
  }

  onSystem(m) {
    if (m.subtype === 'init') {
      const changed = this.sessionId !== m.session_id;
      this.sessionId = m.session_id || this.sessionId;
      if (changed) this.fork = false; // the fork now has its own ID
      if (m.model) { this.resolvedModel = m.model; this.updateModelLabel(); }
      if (Array.isArray(m.mcp_servers)) this.setMcpBadge(sanitizeMcp(m.mcp_servers));
      if (m.permissionMode && m.permissionMode !== this.permissionMode) {
        this.permissionMode = m.permissionMode;
        this.modeSel.value = m.permissionMode;
      }
      if (changed) saveState();
      if (this.status !== 'running') this.setStatus('running');
      // A pending rename waits for the turn's end, when Claude's transcript surely exists.
      if (!this.renamePending) this.syncTitle();
    } else if (m.subtype === 'status') {
      this.onStatus(m);
    } else if (m.subtype === 'compact_boundary') {
      this.onCompactBoundary(m);
    } else if (m.subtype === 'bridge_state') {
      this.onBridgeState(m);
    } else if (m.subtype === 'worker_shutting_down') {
      this.setRemoteState('off');
    } else if (m.subtype === 'background_tasks_changed') {
      this.setBackground(sanitizeTasks(m.tasks)); // Claude's full list, sent whenever it changes
    } else if (m.subtype === 'task_started' || m.subtype === 'task_notification') {
      this.onTaskEvent(m);
    } else if (m.subtype === 'task_progress') {
      this.onTaskProgress(m);
    }
  }

  onStreamEvent(m) {
    if (m.parent_tool_use_id) return; // subagent chatter is shown when its messages complete
    const ev = m.event || {};
    if (this.status !== 'running') this.setStatus('running');
    if (ev.type === 'message_start') {
      this.live = { msgId: ev.message && ev.message.id, blocks: new Map() };
    } else if (ev.type === 'content_block_start' && this.live) {
      const type = ev.content_block && ev.content_block.type;
      if (type === 'text') {
        const md = h('div', { class: 'md streaming' });
        const item = this.append(h('div', { class: 'item text' }, md));
        this.turnTexts.push(item);
        this.live.blocks.set(ev.index, { item, md, text: ev.content_block.text || '', state: {} });
      } else if (type === 'tool_use' || type === 'server_tool_use') {
        this.demoteTurnTexts();
      }
    } else if (ev.type === 'content_block_delta' && this.live) {
      const block = this.live.blocks.get(ev.index);
      if (block && ev.delta && ev.delta.type === 'text_delta') {
        block.text += ev.delta.text || '';
        this.scheduleLiveRender();
      }
    }
  }

  /** At most one render per frame, and none while the cell is collapsed: it draws when shown again. */
  scheduleLiveRender() {
    if (this.renderTimer || this.chatHidden) return;
    this.renderTimer = requestAnimationFrame(() => {
      this.renderTimer = 0;
      if (!this.live || this.chatHidden) return;
      for (const b of this.live.blocks.values()) {
        if (b.md.classList.contains('streaming')) renderMarkdownLive(b.md, b.text, b.state);
      }
      this.scroll();
    });
  }

  /** Swaps the oldest still-streaming block of this message for its final text. */
  takeLiveText(msgId) {
    if (!this.live || this.live.msgId !== msgId) return null;
    for (const [idx, b] of this.live.blocks) {
      if (b.md.classList.contains('streaming')) {
        this.live.blocks.delete(idx);
        b.md.classList.remove('streaming');
        return b;
      }
    }
    return null;
  }

  finishLive() {
    if (!this.live) return;
    for (const b of this.live.blocks.values()) {
      b.md.classList.remove('streaming');
      renderMarkdown(b.md, b.text);
    }
    this.live = null;
  }

  containerFor(parentToolUseId) {
    if (!parentToolUseId) return null;
    const t = this.tools.get(parentToolUseId);
    if (!t) return null;
    if (!t.sub) {
      t.sub = h('div', { class: 'subagent' });
      t.item.append(t.sub);
    }
    return t.sub;
  }

  onAssistant(m, replay) {
    const msg = m.message || {};
    this.noteUsage(m);
    // A local command (/context, /usage...) is answered by the CLI itself, as text in a synthetic message. Its
    // /compact answer repeats the status notice that was already shown.
    if (m.local_command_run && m.local_command_run.command === 'compact') return;
    const limit = usageLimitText(m);
    if (limit) return this.showUsageLimit(limit, replay);
    const parent = this.containerFor(m.parent_tool_use_id);
    for (const block of msg.content || []) {
      if (block.type === 'text') {
        if (!block.text) continue;
        // When the text was written, for the reply's time stamp.
        const ts = String(replay ? Date.parse(m.timestamp) : Date.now());
        const live = !replay && !parent ? this.takeLiveText(msg.id) : null;
        if (live) {
          renderMarkdown(live.md, block.text);
          live.item.dataset.ts = ts;
        } else {
          const md = h('div', { class: 'md' });
          renderMarkdown(md, block.text);
          const item = this.append(h('div', { class: parent ? 'item text narration' : 'item text' }, md), parent);
          if (!parent) {
            item.dataset.ts = ts;
            this.turnTexts.push(item);
          }
        }
      } else if (block.type === 'thinking') {
        if (!block.thinking) continue;
        const md = h('div', { class: 'md' });
        renderMarkdown(md, block.thinking);
        this.append(h('details', { class: 'item thinking', open: this.backend === 'hermes' }, h('summary', { text: 'Thinking' }), md), parent);
      } else if (block.type === 'tool_use' || block.type === 'server_tool_use') {
        this.addTool(block, parent);
      }
    }
  }

  /**
   * Out of plan usage: one card instead of Claude's synthetic reply plus the turn's error. A card with
   * nothing but prompts and notices after it is moved down and counted again rather than repeated.
   */
  showUsageLimit(text, replay) {
    this.limitHit = true;
    const [title, ...rest] = text.split(/\s+[·•]\s+/);
    const reset = rest.join(' · ').replace(/^resets\b/i, 'Resets');
    const cards = this.msgs.querySelectorAll(':scope > .limit-card');
    let card = cards[cards.length - 1];
    for (let n = card && card.nextElementSibling; n; n = n.nextElementSibling) {
      if (!n.matches('.notice, .user')) { card = null; break; }
    }
    if (card) {
      card._count = (card._count || 1) + 1;
      card.querySelector('.limit-count').textContent = `×${card._count}`;
      this.append(card);
    } else {
      card = this.append(h('div', { class: 'item limit-card' },
        h('span', { class: 'limit-icon' }, icon('watch')),
        h('div', { class: 'limit-main' }, h('div', { class: 'limit-title' }), h('div', { class: 'limit-reset' })),
        h('span', { class: 'limit-count' }),
        h('button', { class: 'btn limit-usage', text: 'Plan usage', onclick: () => { usagePill.focus(); refreshUsage(this); } })));
    }
    card.querySelector('.limit-title').textContent = title || 'Usage limit reached';
    card.querySelector('.limit-reset').textContent = reset;
    if (!replay && this.backend === 'claude') scheduleUsageRefresh(this);
  }

  /** Text followed by a tool call is Claude narrating its work, not its answer. */
  demoteTurnTexts() {
    for (const item of this.turnTexts) item.classList.add('narration');
    this.turnTexts = [];
  }

  /** Ends a turn: the text after the last tool call is Claude's reply. */
  closeTurn(isReply) {
    const items = this.turnTexts;
    this.turnTexts = [];
    if (!isReply || !items.length) {
      // `tail`: the last words of a stopped or failed turn stay visible when only replies are shown.
      for (const item of items) item.classList.add('narration', 'tail');
      return;
    }
    for (const item of items) item.classList.add('reply');
    const time = stamp(Number(items[items.length - 1].dataset.ts));
    if (time) items[0].prepend(time);
    const last = items[items.length - 1].querySelector('.md');
    const text = last ? last.textContent.trim() : '';
    if (text) {
      this.lastReply = firstLine(text, 140);
      renderSidebar();
    }
    // A copy button for the reply's Markdown source, shown when the reply is hovered.
    const copy = h('button', { class: 'msg-action', title: 'Copy this reply as Markdown' }, icon('copy'), 'Copy');
    copy.addEventListener('click', () => {
      const source = items.map(i => (i.querySelector('.md') || {})._src || '').filter(Boolean).join('\n\n');
      copyText(source).then(() => { copy.lastChild.textContent = 'Copied'; setTimeout(() => { copy.lastChild.textContent = 'Copy'; }, 1500); });
    });
    items[items.length - 1].append(h('div', { class: 'reply-actions' }, copy));
  }

  addTool(block, parent) {
    const name = block.name || 'tool';
    if (!parent && !this.replayingEarlier) {
      this.demoteTurnTexts();
      this.turnTools++;
      this.currentTool = { id: block.id, name, summary: toolSummary(name, block.input, this.cwd) };
      this.trackTaskCall(block);
      this.renderProgress();
    }
    if (name === 'AskUserQuestion' && !parent) return this.addQuestion(block);
    const summary = h('summary', null,
      h('span', { class: 'tname', text: name }),
      h('span', { class: 'targ', text: toolSummary(name, block.input, this.cwd) }));
    const body = h('div', { class: 'tool-body' });
    const item = h('details', { class: 'item tool pending' }, summary, body);
    // The body (input, diff, output) is built when the tool is first opened: most never are.
    const t = { item, body, name, input: block.input, outputs: [], patch: null, built: false };
    item.addEventListener('toggle', () => { if (item.open) this.buildTool(t); });
    if (name === 'TodoWrite') {
      item.open = true;
      this.buildTool(t);
    }
    this.tools.set(block.id, t);
    this.openTools.add(block.id);
    this.append(item, parent);
  }

  buildTool(t) {
    if (t.built) return;
    t.built = true;
    t.body.append(toolRow('in', (t.patch && patchView(t.patch)) || toolInputView(t.name, t.input)));
    for (const make of t.outputs) t.body.append(make());
    t.outputs = [];
  }

  /** Claude's questions and the answers given: a small collapsible record, shown even in the replies-only view. */
  addQuestion(block) {
    const questions = block.input && Array.isArray(block.input.questions) ? block.input.questions : [];
    const first = questions[0] || {};
    const more = questions.length > 1 ? `  (+${questions.length - 1} more)` : '';
    const answerEl = h('span', { class: 'qa-a', text: 'Waiting for your answer…' });
    const summary = h('summary', null, icon('comment-discussion'), h('span', { class: 'qa-q', text: (first.question || 'Question') + more }), answerEl);
    const body = h('div', { class: 'qa-body' });
    const t = { item: h('details', { class: 'item qa pending' }, summary, body), body, name: 'AskUserQuestion', qa: { questions, answerEl } };
    this.renderQuestion(t, null);
    this.tools.set(block.id, t);
    this.openTools.add(block.id);
    this.append(t.item);
  }

  /** `result` is the tool's structured result ({ answers: { question: "label, label" } }) when Claude sent one. */
  answerQuestion(t, block, result) {
    t.item.classList.remove('pending');
    let answers = result && result.answers && typeof result.answers === 'object' ? result.answers : null;
    if (!answers && !block.is_error) answers = parseAnswers(toolResultText(block.content));
    const given = answers ? t.qa.questions.map(q => answers[q.question]).filter(a => typeof a === 'string' && a) : [];
    if (block.is_error || !given.length) {
      t.item.classList.add('skipped');
      t.qa.answerEl.textContent = 'Skipped';
      return;
    }
    t.qa.answerEl.textContent = given.join(' · ');
    this.renderQuestion(t, answers);
  }

  renderQuestion(t, answers) {
    t.body.replaceChildren(...t.qa.questions.map(q => {
      const options = Array.isArray(q.options) ? q.options : [];
      const answer = answers && typeof answers[q.question] === 'string' ? answers[q.question] : null;
      const { chosen, other } = answer == null ? { chosen: new Set(), other: '' } : splitAnswer(answer, options);
      return h('div', { class: 'qa-block' },
        q.header ? h('div', { class: 'qa-head', text: q.header }) : null,
        h('div', { class: 'qa-question', text: q.question || '' }),
        h('ul', { class: 'qa-options' }, options.map(opt => h('li', { class: chosen.has(opt.label) ? 'chosen' : '' },
          icon(chosen.has(opt.label) ? 'pass-filled' : 'circle-large-outline'), opt.label,
          opt.description ? h('span', { class: 'desc', text: ` — ${opt.description}` }) : null))),
        other ? h('div', { class: 'qa-other' }, h('span', { class: 'desc', text: 'Other: ' }), other) : null);
    }));
  }

  /** A finished turn has no tool calls left running. */
  settleTools() {
    for (const id of this.openTools) {
      const t = this.tools.get(id);
      if (!t) continue;
      if (t.qa && t.item.classList.contains('pending')) t.qa.answerEl.textContent = 'No answer';
      t.item.classList.remove('pending');
    }
    this.openTools.clear();
    this.currentTool = null;
  }

  onUser(m, replay) {
    const msg = m.message || {};
    if (m.isMeta || m.isCompactSummary) {
      if (m.isCompactSummary) this.notice('Conversation compacted');
      return;
    }
    const parent = this.containerFor(m.parent_tool_use_id);
    const content = typeof msg.content === 'string' ? [{ type: 'text', text: msg.content }] : msg.content || [];
    const texts = [];
    const images = [];
    let omitted = 0; // images of a restored transcript, whose data the backend leaves out
    // The structured result is per message, so it can only be matched to a lone tool_result.
    const lone = content.filter(b => b.type === 'tool_result').length === 1;
    const structured = lone ? m.tool_use_result || m.toolUseResult : null;
    for (const block of content) {
      if (block.type === 'tool_result') {
        this.settleTaskCreate(block);
        if (this.currentTool && this.currentTool.id === block.tool_use_id) {
          this.currentTool = null;
          this.renderProgress();
        }
        const t = this.tools.get(block.tool_use_id);
        if (!t) continue;
        this.openTools.delete(block.tool_use_id);
        if (t.qa) { this.answerQuestion(t, block, structured); continue; }
        const text = toolResultText(block.content);
        t.item.classList.remove('pending');
        t.item.classList.add(block.is_error ? 'err' : 'ok');
        // An Edit's result carries the real patch, with line numbers; it replaces the computed diff.
        if (structured && t.name === 'Edit' && Array.isArray(structured.structuredPatch) && structured.structuredPatch.length) {
          t.patch = structured.structuredPatch;
          const view = t.built && patchView(t.patch);
          if (view) t.body.querySelector('.tool-row').lastElementChild.replaceWith(view);
        }
        if (text) {
          const make = () => toolRow('out', longPre(text), block.is_error ? 'err' : '');
          if (t.built) t.body.append(make());
          else t.outputs.push(make);
        }
      } else if (block.type === 'text' && block.text) {
        const text = block.text;
        if (text.startsWith('[Request interrupted')) this.notice('Interrupted');
        else if (/^\s*<(command-|local-command-|system-reminder|task-notification|ide_)/.test(text) || text.startsWith(TASK_HINT_PREFIX)) continue;
        else if (text.startsWith(RESUME_PROMPT)) { if (this.replaying) this.notice(RESUME_NOTICE); } // live, resumeAfterRestart said so
        else texts.push(text);
      } else if (block.type === 'image') {
        const url = imageBlockUrl(block.source);
        if (url) images.push(url);
        else if (block.source && block.source.data === '') omitted++;
      }
    }
    // Live prompts are drawn when sent; only a restored transcript needs them drawn here.
    if (replay && !parent && (texts.length || images.length || omitted)) {
      this.closeTurn(true);
      this.sealTasks(); // a replay has no turn start, so each prompt closes the list before it
      const text = texts.join('\n');
      this.append(userBubble(text, images, Date.parse(m.timestamp), omitted));
      if (!this.title) this.title = firstLine(text, 90) || 'Image';
    } else if (!replay && !parent && !m.isSynthetic && this.remoteState === 'on') {
      // A prompt typed on the phone or on claude.ai/code arrives like this; HQ's own prompts were drawn when sent.
      for (const text of texts) {
        const i = this.sentPrompts.indexOf(text);
        if (i >= 0) {
          this.sentPrompts.splice(0, i + 1);
          continue;
        }
        const bubble = userBubble(text, [], Date.now());
        bubble.querySelector('.bubble').prepend(h('span', { class: 'remote-tag', text: 'Remote' }));
        this.append(bubble);
      }
    }
  }

  onResult(m) {
    const local = !!m.local_command; // /compact and other commands the CLI answers itself
    // A prompt sent during a turn can be read in it or start the next one, so it is kept through one more result.
    this.queuedTexts = this.queuedTexts.filter(q => q.results++ < 1);
    this.clearForceNote();
    this.finishLive();
    this.closeTurn(m.subtype === 'success' && !m.is_error);
    this.settleTools();
    this.pruneTaskProgress();
    for (const id of [...this.pending.keys()]) this.dropPrompt(id); // a finished turn has nothing left to approve
    if (m.is_error && m.subtype !== 'error_during_execution') {
      const detail = m.result || (m.errors && m.errors.join('\n')) || m.subtype;
      if (!this.limitHit && !LIMIT_TEXT.test(String(detail))) this.notice(`Error: ${detail}`, true);
    }
    this.noteResult(m);
    if (this.compacting) this.setCompacting(false);
    this.setWrapUp(null);
    if (!local && typeof m.duration_ms === 'number') this.lastDuration = `${(m.duration_ms / 1000).toFixed(1)}s`;
    this.renderMeta();
    this.setStatus('idle');
    if (!local) {
      this.markUnread();
      notifyTurnDone(this, m);
    }
    this.refreshState();
    if (this.backend === 'claude') scheduleUsageRefresh(this); // Claude's plan usage; Hermes turns do not touch it
    this.syncTitle();
  }

  onControlResponse(m) {
    const response = m.response || {};
    const resolve = this.controls.get(response.request_id);
    if (!resolve) return;
    this.controls.delete(response.request_id);
    resolve(response);
  }

  onExit(m) {
    const wasRunning = this.status === 'running';
    this.queuedTexts = []; // a new process starts from the transcript
    const forced = this.forced;
    this.forced = false;
    this.clearForceNote();
    // Claude died before it answered the last prompt (a failed --resume, say): the prompt goes back to the composer.
    const lost = this.lastSent;
    this.lastSent = null;
    this.proc = null;
    for (const resolve of this.controls.values()) resolve(null);
    this.controls.clear();
    this.finishLive();
    this.closeTurn(false);
    for (const id of [...this.pending.keys()]) this.dropPrompt(id);
    this.settleTools();
    for (const t of this.tools.values()) t.item.classList.remove('bg');
    this.bgTasks = []; // they ran inside this claude, so they ended with it
    this.compacting = false;
    this.setWrapUp(null);
    this.remoteState = this.remoteState === 'policy' ? 'policy' : 'off'; // the claude.ai session ended with the process
    this.remoteUrl = null;
    this.renderRemote();
    this.renderContext();
    if (this.quietExit) { // /clear ended it: the cell starts over, so nothing is said
      this.quietExit = false;
      this.setStatus('idle');
    } else if (forced) {
      this.notice('Stopped. Claude resumes this session when you send a message.');
      this.setStatus('idle');
    } else if (m.code !== 0 && m.code != null) {
      const tail = (m.stderr || []).slice(-8).join('\n');
      this.notice(`Claude exited with code ${m.code}${tail ? '\n' + tail : ''}`, true);
      if (lost) this.unsend(lost, true);
      this.setStatus('exited');
    } else {
      this.setStatus(wasRunning ? 'exited' : 'idle');
    }
    if (this.exitWaiter) this.exitWaiter();
  }

  // ---------- prompts: permission, questions, plan approval ----------

  onControlRequest(m) {
    const req = m.request || {};
    if (req.subtype !== 'can_use_tool') {
      // Hooks and SDK MCP servers are never registered by HQ, so refuse anything else, and say so.
      this.write({ type: 'control_response', response: { subtype: 'error', request_id: m.request_id, error: `HQ does not handle ${req.subtype}` } });
      this.notice(`Claude sent a request that HQ does not handle (${req.subtype || 'unknown'}), and HQ refused it.`, true);
      return;
    }
    let card;
    if (req.tool_name === 'AskUserQuestion') card = this.questionCard(m.request_id, req);
    else if (req.tool_name === 'ExitPlanMode') card = this.planCard(m.request_id, req);
    else card = this.permissionCard(m.request_id, req);
    card.dataset.kind = req.tool_name === 'AskUserQuestion' ? 'question' : req.tool_name === 'ExitPlanMode' ? 'plan' : 'permission';
    this.pending.set(m.request_id, card);
    this.prompts.append(card);
    this.syncPromptsBar(); // a card arriving while minimized only updates the count; it does not reopen the area
    this.unread = false;
    this.refreshState();
    invoke('request_attention').catch(() => {});
    notifyWaiting(this, req);
  }

  /** Folds the prompts area to its bar, or opens it again. Only a class changes, so nothing is re-rendered. */
  setPromptsMin(min) {
    if (min === this.promptsMin || (min && !this.pending.size)) return;
    this.promptsMin = min;
    this.syncPromptsBar();
    if (min && this.prompts.contains(document.activeElement)) this.input.focus();
  }

  /** The bar's count and wording, and the area's class. The area resets to open once the last card is answered. */
  syncPromptsBar() {
    const n = this.pending.size;
    if (!n) this.promptsMin = false;
    const min = this.promptsMin;
    this.el.classList.toggle('prompts-min', min);
    this.promptsBar.classList.toggle('hidden', !n);
    this.promptsBar.setAttribute('aria-expanded', String(!min));
    if (n) {
      const kinds = new Set([...this.pending.values()].map(c => c.dataset.kind));
      const noun = kinds.size > 1 ? 'prompt' : kinds.has('question') ? 'question' : kinds.has('plan') ? 'plan' : 'permission request';
      const label = n === 1 && noun === 'plan' ? 'Plan waiting for approval' : `${n} ${noun}${n === 1 ? '' : 's'} waiting`;
      this.promptsBar.querySelector('.pb-text').textContent = `${label} — ${min ? 'show' : 'hide'}`;
      this.promptsBar.title = min ? 'Show the prompts again' : 'Minimize the prompts to read the chat; typed answers are kept';
      const chev = this.promptsBar.firstElementChild;
      chev.classList.toggle('codicon-chevron-up', min);
      chev.classList.toggle('codicon-chevron-down', !min);
    }
    this.scheduleScroll(); // the chat grew or shrank; stay at the bottom if it was there
  }

  dropPrompt(requestId) {
    const card = this.pending.get(requestId);
    if (card) card.remove();
    this.pending.delete(requestId);
    this.syncPromptsBar();
    this.refreshState();
  }

  answer(requestId, response) {
    this.respond(requestId, response);
    this.dropPrompt(requestId);
  }

  permissionCard(requestId, req) {
    const input = req.input || {};
    const name = req.display_name || req.tool_name;
    const title = h('div', { class: 'prompt-title' }, `Allow ${name}?`,
      req.description ? h('span', { class: 'targ', text: '  ' + req.description }) : null);
    let preview;
    if (req.tool_name === 'Edit' || req.tool_name === 'MultiEdit') {
      preview = h('div', { class: 'preview' }, h('div', { text: relPath(input.file_path, this.cwd) }), toolInputView(req.tool_name, input));
    } else if (req.tool_name === 'Bash' || req.tool_name === 'PowerShell') {
      preview = h('pre', { text: input.command || '' });
    } else if (req.tool_name === 'Write') {
      const lines = String(input.content || '').split('\n');
      const shown = lines.slice(0, 30).join('\n') + (lines.length > 30 ? `\n… ${lines.length - 30} more lines` : '');
      preview = h('div', { class: 'preview' }, h('div', { text: relPath(input.file_path, this.cwd) }), h('div', { class: 'diff' }, h('div', { class: 'add', text: shown })));
    } else {
      preview = h('pre', { text: JSON.stringify(input, null, 2) });
    }
    const reason = req.decision_reason ? h('div', { class: 'desc', text: String(req.decision_reason) }) : null;

    const feedback = h('input', { type: 'text', placeholder: 'No — tell Claude what to do instead (optional), then Enter' });
    const allow = () => this.answer(requestId, { behavior: 'allow', updatedInput: input });
    const allowAlways = () => this.answer(requestId, { behavior: 'allow', updatedInput: input, updatedPermissions: req.permission_suggestions });
    const deny = () => this.answer(requestId, { behavior: 'deny', message: feedback.value.trim() || 'The user denied this tool use.' });
    feedback.addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); deny(); } });

    const choices = [choice('1', 'Yes', null, allow)];
    const suggestions = Array.isArray(req.permission_suggestions) ? req.permission_suggestions : [];
    if (suggestions.length) choices.push(choice('2', suggestionLabel(suggestions), null, allowAlways));
    choices.push(choice(String(choices.length + 1), 'No', null, deny));

    const card = h('div', { class: 'prompt' }, title, reason, preview, h('div', { class: 'choices' }, choices), feedback);
    card.hotkeys = choices.map(c => c.run);
    return card;
  }

  questionCard(requestId, req) {
    const input = req.input || {};
    const questions = Array.isArray(input.questions) ? input.questions : [];
    const picks = questions.map(() => new Set());
    const others = [];
    let focusedQ = 0; // the question the number keys answer
    const focusQuestion = i => {
      focusedQ = i;
      blocks.forEach((b, k) => b.classList.toggle('qfocus', questions.length > 1 && k === i));
    };
    // After a single-choice answer the number keys move on to the next question that has none yet.
    const nextOpen = qi => {
      for (let k = 1; k < questions.length; k++) {
        const i = (qi + k) % questions.length;
        if (!picks[i].size && !others[i].value) return i;
      }
      return qi;
    };
    const blocks = questions.map((q, qi) => {
      const buttons = (q.options || []).map((opt, oi) => {
        const b = choice(String(oi + 1), opt.label, opt.description, () => {
          if (q.multiSelect) {
            picks[qi].has(opt.label) ? picks[qi].delete(opt.label) : picks[qi].add(opt.label);
          } else {
            picks[qi].clear();
            picks[qi].add(opt.label);
            others[qi].value = '';
          }
          buttons.forEach(btn => btn.classList.toggle('selected', picks[qi].has(btn.dataset.label)));
          if (!q.multiSelect && questions.length === 1) submit();
          else if (!q.multiSelect) focusQuestion(nextOpen(qi));
        });
        b.dataset.label = opt.label;
        return b;
      });
      others[qi] = h('input', { type: 'text', placeholder: 'Other…' });
      others[qi].addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); submit(); } });
      const block = h('div', { class: 'qblock' },
        q.header ? h('div', { class: 'qhead', text: q.header }) : null,
        h('div', { class: 'prompt-title', text: q.question }),
        h('div', { class: 'choices' }, buttons),
        others[qi]);
      block.addEventListener('pointerdown', () => focusQuestion(qi));
      block.addEventListener('focusin', () => focusQuestion(qi));
      return block;
    });
    focusQuestion(0);
    const submit = () => {
      const answers = {};
      questions.forEach((q, qi) => {
        const other = others[qi].value.trim();
        const chosen = [...picks[qi]];
        if (other) chosen.push(other);
        answers[q.question] = chosen.join(', ');
      });
      this.answer(requestId, { behavior: 'allow', updatedInput: { ...input, answers } });
    };
    const skip = () => this.answer(requestId, { behavior: 'deny', message: 'The user declined to answer.' });
    const card = h('div', { class: 'prompt' }, blocks,
      h('div', { class: 'actions' },
        h('button', { class: 'btn primary', onclick: submit }, 'Submit'),
        h('button', { class: 'btn', onclick: skip }, 'Skip')));
    card.hotkeys = Array.from({ length: 9 }, (_, i) => () => {
      const button = blocks[focusedQ].querySelectorAll('.choice')[i];
      if (button) button.click();
    });
    return card;
  }

  planCard(requestId, req) {
    const input = req.input || {};
    const plan = h('div', { class: 'preview md' });
    renderMarkdown(plan, input.plan || '');
    const feedback = h('textarea', { rows: '2', placeholder: 'Keep planning — tell Claude what to change (optional)' });
    const approve = mode => {
      this.answer(requestId, { behavior: 'allow', updatedInput: input });
      this.setMode(mode);
    };
    const keepPlanning = () => this.answer(requestId, { behavior: 'deny', message: feedback.value.trim() || 'The user wants to keep planning.' });
    const choices = [
      choice('1', 'Yes, and auto-accept edits', null, () => approve('acceptEdits')),
      choice('2', 'Yes, and manually approve edits', null, () => approve('default')),
      choice('3', 'No, keep planning', null, keepPlanning),
    ];
    const card = h('div', { class: 'prompt' }, h('div', { class: 'prompt-title', text: 'Ready to code? Here is Claude\'s plan:' }), plan, h('div', { class: 'choices' }, choices), feedback);
    card.hotkeys = choices.map(c => c.run);
    return card;
  }

  // ---------- history ----------

  /**
   * Draws the transcript, in slices that give the event loop back so a long one does not freeze
   * the window. A collapsed cell waits until it is opened. Only the newest part is loaded; an
   * earlier part is fetched on request ("Load earlier").
   */
  loadHistory() {
    if (!this.sessionId || this.historyLoading) return this.historyLoading;
    this.historyLoading = this.drawHistory().finally(() => { this.historyLoading = null; });
    return this.historyLoading;
  }

  async drawHistory() {
    let chunk = { lines: [], start: 0 };
    try {
      chunk = await invoke(this.backend === 'hermes' ? 'load_hermes_transcript' : 'load_transcript', { sessionId: this.sessionId, cwd: this.cwd, before: null });
    } catch (err) {
      this.notice(`Could not load history: ${err}`, true);
    }
    const lines = Array.isArray(chunk.lines) ? chunk.lines : [];
    this.historyStart = Number(chunk.start) || 0;
    const prevStick = this.stickToBottom;
    this.stickToBottom = false;
    this.replaying = true;
    const last = await this.replayLines(lines);
    this.replaying = false;
    this.closeTurn(true);
    this.settleTools();
    this.drawProgress();
    this.renderContext();
    this.stickToBottom = prevStick;
    this.setTitle(this.title);
    saveState(); // keeps a title recovered from the transcript
    if (this.historyStart > 0) this.msgs.prepend(this.earlierNote());
    if (this.fork) this.notice('Forked: your first message starts a copy of this session. The original is left as it was.');
    else if (this.missing) this.notice('The folder of this session is missing, so the session cannot continue. Use "Locate folder…" below, or close it.');
    else if (!this.resumeAfterUpdate) this.notice(lines.length ? `Session restored. ${this.backend === 'hermes' ? 'Hermes' : 'Claude'} resumes when you send a message.` : 'Session restored (no history found).');
    if (last && endsMidTurn(last) && !this.resumeAfterUpdate) this.notice('The last turn was interrupted when HQ closed.');
    if (this.resumeWarning) this.notice(this.resumeWarning, true);
    this.pinToBottom();
  }

  /**
   * Replays transcript lines through the live handlers; returns the last user or assistant message.
   * `slices`: give the event loop back now and then (not for the earlier part, which swaps state
   * that live messages must not see).
   */
  async replayLines(lines, slices = true) {
    let last = null;
    let sliceStart = performance.now();
    for (const line of lines) {
      try {
        const m = JSON.parse(line);
        this.handle(m, true);
        if (m.type === 'user' || m.type === 'assistant') last = m;
      } catch { /* skip malformed line */ }
      if (slices && performance.now() - sliceStart > 12) {
        await new Promise(resolve => setTimeout(resolve, 0));
        sliceStart = performance.now();
        if (this.closing) return last;
      }
    }
    return last;
  }

  earlierNote() {
    const button = h('button', { class: 'btn', text: 'Load earlier' });
    const note = h('div', { class: 'item notice' }, 'Earlier messages of this session are not shown. ', button);
    button.addEventListener('click', async () => {
      button.disabled = true;
      await this.loadEarlier();
      note.remove();
      if (this.historyStart > 0) this.msgs.prepend(this.earlierNote());
    });
    return note;
  }

  /** Draws the part of the transcript before the one shown, above it, and keeps the view where it was. */
  async loadEarlier() {
    let chunk;
    try {
      chunk = await invoke(this.backend === 'hermes' ? 'load_hermes_transcript' : 'load_transcript', { sessionId: this.sessionId, before: this.historyStart });
    } catch (err) {
      this.notice(`Could not load earlier history: ${err}`, true);
      return;
    }
    const lines = Array.isArray(chunk.lines) ? chunk.lines : [];
    // Draw into a detached container (items append to this.msgs), then move the result to the top.
    const shown = this.msgs;
    const staging = h('div');
    const saved = { turnTexts: this.turnTexts, title: this.title, aiTitle: this.aiTitle, customTitle: this.customTitle,
      lastReply: this.lastReply, ctx: { ...this.ctx }, replaying: this.replaying };
    this.msgs = staging;
    this.turnTexts = [];
    this.replaying = true;
    this.replayingEarlier = true;
    try {
      // One synchronous pass, so no live message can arrive while this.msgs points at the staging area.
      for (const line of lines) {
        try { this.handle(JSON.parse(line), true); } catch { /* skip malformed line */ }
      }
      this.closeTurn(true);
    } finally {
      this.replaying = saved.replaying;
      this.replayingEarlier = false;
      this.msgs = shown;
      this.turnTexts = saved.turnTexts;
      this.lastReply = saved.lastReply;
      this.ctx = saved.ctx; // the older messages' usage says nothing about the context now
      Object.assign(this, { title: saved.title, aiTitle: saved.aiTitle, customTitle: saved.customTitle });
    }
    const before = shown.scrollHeight;
    shown.prepend(...staging.childNodes);
    shown.scrollTop += shown.scrollHeight - before;
    this.historyStart = Number(chunk.start) || 0;
  }

  // ---------- folder missing ----------

  applyMissing() {
    this.el.classList.toggle('missing', this.missing);
    this.missingBar.classList.toggle('hidden', !this.missing);
    this.missingBar.querySelector('.missing-text').textContent = `Folder missing: ${this.cwd}`;
    this.input.disabled = this.missing;
    this.stateEl.textContent = stateOf(this)[1];
  }

  /** Points a cell whose folder is gone at a folder that exists, and brings its history back. */
  async locateFolder() {
    const path = await invoke('pick_folder').catch(() => null);
    if (!path) return;
    this.cwd = path;
    this.color = projectColor(path);
    this.el.style.setProperty('--proj', this.color);
    for (const name of this.el.querySelectorAll('.proj-name, .cell-project .sweep-name')) name.textContent = baseName(path) || path;
    this.el.querySelector('.cell-path').textContent = path;
    this.missing = false;
    this.applyMissing();
    this.renderTitle();
    this.renderCodeButton();
    placeCell(this);
    layout();
    rememberFolder(path);
    saveState();
    this.notice(`Folder set to ${path}. If Claude cannot resume the session from this folder, copy the project's session files across or start a new session.`);
    focusCell(this, true);
  }

  // ---------- context window ----------

  /** The last request's size is what the next one starts with: input plus both cache counters. */
  noteUsage(m) {
    const msg = m.message || {};
    const u = msg.usage;
    if (!u || m.parent_tool_use_id || msg.model === '<synthetic>') return;
    const tokens = (u.input_tokens || 0) + (u.cache_read_input_tokens || 0) + (u.cache_creation_input_tokens || 0);
    if (!tokens) return;
    this.ctx.tokens = tokens;
    this.ctx.stale = false;
    if (msg.model) this.ctx.model = msg.model;
    if (!this.replaying) this.renderContext();
  }

  /** A result carries the window size per model (its token counts are cumulative, so only `contextWindow` is read) and the cost. */
  noteResult(m) {
    const win = pickContextWindow(m.modelUsage, this.ctx.model || this.resolvedModel);
    if (win) this.ctx.window = win;
    const total = Number(m.total_cost_usd);
    if (Number.isFinite(total)) {
      if (total > this.cost.procUsd) this.cost.usd += total - this.cost.procUsd;
      this.cost.procUsd = Math.max(total, this.cost.procUsd);
    }
    let tokens = 0;
    for (const u of Object.values(m.modelUsage && typeof m.modelUsage === 'object' ? m.modelUsage : {})) {
      tokens += (u.inputTokens || 0) + (u.outputTokens || 0) + (u.cacheReadInputTokens || 0) + (u.cacheCreationInputTokens || 0);
    }
    if (tokens > this.cost.procTokens) this.cost.tokens += tokens - this.cost.procTokens;
    this.cost.procTokens = Math.max(tokens, this.cost.procTokens);
    this.renderContext();
  }

  renderMeta() {
    const parts = [this.lastDuration, this.cost.usd > 0 ? formatCost(this.cost.usd) : ''].filter(Boolean);
    if (this.metaEl.textContent === 'Updating Claude Code…') return;
    this.metaEl.textContent = parts.join(' · ');
    this.metaEl.title = this.cost.usd > 0 ? `Last turn${this.lastDuration ? `: ${this.lastDuration}` : ''}\nCost reported by Claude Code in this HQ run: ${formatCost(this.cost.usd)}` : '';
  }

  /** The ring: how close the context is to the auto-compact point (or to the window's end when auto-compact is off). */
  renderContext() {
    const c = this.ctx;
    const known = c.tokens != null;
    this.ctxBtn.classList.toggle('hidden', !known && !this.compacting);
    if (!known && !this.compacting) return;
    const info = c.info;
    const max = (info && info.max) || c.window;
    const limit = (info && info.autoCompact && info.threshold) || max;
    const percent = known && limit ? Math.min(100, (c.tokens / limit) * 100) : 0;
    this.ctxBtn.style.setProperty('--ctx-pct', String(percent.toFixed(1)));
    this.ctxBtn.style.setProperty('--ctx-color', limit ? heatCss(percent) : 'var(--fg-faint)');
    this.ctxBtn.querySelector('.ctx-text').textContent = !known ? '' : max ? `${Math.round((c.tokens / max) * 100)}%` : formatTokens(c.tokens);
    this.ctxBtn.classList.toggle('compacting', this.compacting);
    const blocked = this.status === 'running' && !this.compacting;
    this.ctxBtn.classList.toggle('blocked', blocked);
    this.ctxBtn.setAttribute('aria-disabled', String(blocked || this.compacting));
    this.ctxBtn.title = this.compacting ? 'Compacting…' : blocked ? 'Wait for the turn to finish' : '';
    if (ctxPopCell === this) renderContextPop();
  }

  /** Asks Claude for the breakdown. The reply is large, so it is asked for on hover only, and kept for 30 seconds. */
  async refreshContext(force) {
    const c = this.ctx;
    if (this.proc == null) return null;
    if (c.loading) return c.loading;
    if (!force && Date.now() - c.infoAt < CONTEXT_INFO_TTL_MS) return null;
    c.loading = (async () => {
      const reply = await withTimeout(this.control({ subtype: 'get_context_usage', detail: 'summary' }), 15000);
      const r = reply && reply.subtype === 'success' && reply.response;
      if (r) {
        c.info = sanitizeContext(r);
        c.infoAt = Date.now();
        if (c.stale && c.info.total != null) { c.tokens = c.info.total; c.stale = false; }
        if (c.info.max) c.window = c.info.max;
      }
    })().catch(() => {}).finally(() => {
      c.loading = null;
      this.renderContext();
    });
    this.renderContext();
    return c.loading;
  }

  /** `/compact` is a local command, so it goes in as a prompt (a plain string). The CLI has no compact control request. */
  async compact(askInstructions) {
    if (this.missing || this.compacting || this.status === 'running') return;
    if (askInstructions) {
      openMenu(this.ctxBtn, this, menuEl => this.buildCompactMenu(menuEl));
      return;
    }
    await this.runCompact('');
  }

  async runCompact(instructions) {
    if (this.historyLoading) await this.historyLoading;
    if (this.status === 'running' || this.compacting) return;
    this.setCompacting(true); // the line over the prompt box says so; its result brings the cell back to idle
    if (!(await this.ensureProcess())) { this.setCompacting(false); return; }
    if (instructions) this.notice(`Compacting with instructions: ${instructions}`);
    this.stickToBottom = true;
    this.write({ type: 'user', message: { role: 'user', content: instructions ? `/compact ${instructions}` : '/compact' }, parent_tool_use_id: null });
  }

  buildCompactMenu(menuEl) {
    const field = h('input', { type: 'text', placeholder: 'What to keep in the summary (optional)', spellcheck: 'false' });
    const go = () => { closeMenu(); this.runCompact(field.value.replace(/\s+/g, ' ').trim()); };
    field.addEventListener('keydown', e => {
      e.stopPropagation();
      if (e.key === 'Enter') { e.preventDefault(); go(); }
    });
    menuEl.append(h('div', { class: 'menu-title', text: 'Compact the conversation' }),
      h('div', { class: 'menu-note' }, field),
      h('button', { class: 'menu-item', onclick: go }, icon('fold'), h('span', { class: 'menu-main' }, h('span', { class: 'name', text: 'Compact' }))));
    setTimeout(() => field.focus(), 0);
  }

  /** status: 'compacting' while it runs; then compact_result 'success' or 'failed' (with compact_error). */
  onStatus(m) {
    if (m.status === 'compacting') {
      this.setCompacting(true);
    } else if (m.compact_result === 'failed') {
      this.setCompacting(false);
      const why = typeof m.compact_error === 'string' && m.compact_error ? m.compact_error : 'Compaction failed.';
      this.notice(why, !/not enough messages/i.test(why));
    } else if (m.status == null && m.compact_result) {
      this.setCompacting(false);
    }
  }

  onCompactBoundary(m) {
    const meta = m.compact_metadata || {};
    this.setCompacting(false);
    this.notice(meta.trigger === 'auto' ? 'Conversation compacted automatically' : 'Conversation compacted');
    this.ctx.info = null;
    this.ctx.infoAt = 0;
    const post = Number(meta.post_tokens);
    if (Number.isFinite(post) && post > 0) {
      this.ctx.tokens = post;
      this.ctx.stale = false;
      this.renderContext();
    } else {
      this.ctx.stale = true; // the last request's size no longer holds; ask
      this.refreshContext(true);
    }
  }

  setCompacting(on) {
    if (this.compacting === on) return;
    this.compacting = on;
    if (on && this.status !== 'running') this.setStatus('running');
    this.nextBusyWord();
    this.renderContext();
  }

  // ---------- Remote Control ----------

  /** The title Claude Code gives the claude.ai session. Sent only when the user chose it or Claude titled it: a name locks the title. */
  remoteName() {
    if (this.customTitle) return this.customTitle;
    if (this.aiTitle && this.title) return this.title;
    return null;
  }

  setRemoteState(state, detail = '') {
    this.remoteState = state;
    this.remoteDetail = detail;
    if (state === 'off') this.remoteUrl = null;
    this.renderRemote();
  }

  /** Turns Remote Control on once per process, without the user asking: a failure is shown once, not at every prompt. */
  autoEnableRemote() {
    if (this.remoteAsked) return;
    this.remoteAsked = true;
    this.enableRemote();
  }

  async enableRemote() {
    if (this.proc == null || this.backend === 'hermes' || app.remoteAvailable === false || this.remoteState === 'policy' || this.remoteState === 'connecting' || this.remoteState === 'on') return;
    this.remote = true;
    const seq = ++this.remoteSeq;
    this.setRemoteState('connecting');
    const request = { subtype: 'remote_control', enabled: true };
    const name = this.remoteName();
    if (name) request.name = name;
    if (this.bridgeSessionId) request.reattach_session_id = this.bridgeSessionId;
    let reply = await this.control(request);
    if (seq !== this.remoteSeq) return; // turned off meanwhile
    if (reply && reply.subtype === 'error' && request.reattach_session_id && /not found|no such|unknown|invalid|expired|archiv|ended|gone|exist/i.test(String(reply.error))) {
      this.bridgeSessionId = null; // the old remote session is gone: start a new one
      delete request.reattach_session_id;
      reply = await this.control(request);
      if (seq !== this.remoteSeq) return;
    }
    if (reply && reply.subtype === 'success') {
      const r = reply.response || {};
      this.remoteUrl = r.session_url || r.connect_url || null;
      if (r.bridge_session_id) this.bridgeSessionId = r.bridge_session_id;
      if (this.remoteState === 'connecting') this.setRemoteState('on');
      saveState();
    } else if (reply) {
      const why = reply.error || 'rejected by Claude';
      this.setRemoteState('failed', why);
      this.notice(`Remote Control could not start: ${why}`, true);
    } else {
      this.setRemoteState('off'); // the process ended first
    }
  }

  async setRemote(on) {
    this.remote = on;
    saveState();
    if (on) {
      if (this.proc != null) await this.enableRemote();
      else this.renderRemote(); // starts with Claude
      return;
    }
    this.remoteSeq++; // drops an enable that is still waiting for its reply
    const wasOn = this.remoteState !== 'off';
    this.bridgeSessionId = null;
    this.setRemoteState('off');
    saveState();
    if (this.proc != null && wasOn) await this.control({ subtype: 'remote_control', enabled: false });
  }

  /** system/bridge_state: connected and ready mean the session can be reached from claude.ai; failed carries the cause. */
  onBridgeState(m) {
    const detail = [m.detail, m.failure_kind ? `(${m.failure_kind})` : ''].filter(Boolean).join(' ');
    if (this.remote === false && m.state !== 'policy_disabled') return; // a late event from before the user turned it off
    if (m.state === 'connected' || m.state === 'ready') {
      this.setRemoteState('on');
    } else if (m.state === 'failed') {
      this.setRemoteState('failed', detail || 'The connection failed.');
    } else if (m.state === 'policy_disabled') {
      this.setRemoteState('policy', detail || 'Remote Control is turned off by your organization.');
      this.notice(`Remote Control is not available: ${this.remoteDetail}`, true);
    }
  }

  renderRemote() {
    const btn = this.remoteBtn;
    const hidden = this.backend === 'hermes' || app.remoteAvailable === false || this.remoteState === 'policy';
    btn.classList.toggle('hidden', hidden);
    if (hidden) return;
    const armed = this.remote && this.proc == null;
    btn.classList.toggle('on', this.remoteState === 'on');
    btn.classList.toggle('connecting', this.remoteState === 'connecting');
    btn.classList.toggle('failed', this.remoteState === 'failed');
    btn.title = this.remoteState === 'on' ? 'Remote Control is on: continue this session from claude.ai/code or the Claude app\nClick for options'
      : this.remoteState === 'connecting' ? 'Connecting Remote Control…'
      : this.remoteState === 'failed' ? `Remote Control failed: ${this.remoteDetail}\nClick for options`
      : armed ? 'Remote Control turns on when Claude starts for this session\nClick for options'
      : 'Remote Control: off. Click to turn it on';
    btn.querySelector('.codicon').className = `codicon codicon-${this.remoteState === 'on' || armed ? 'broadcast' : 'circle-slash'}`;
  }

  buildRemoteMenu(menuEl) {
    menuEl.append(h('div', { class: 'menu-title', text: 'Remote Control' }));
    const item = (name, desc, run, ico) => h('button', { class: 'menu-item', onclick: () => { closeMenu(); run(); } },
      icon(ico), h('span', { class: 'menu-main' }, h('span', { class: 'name', text: name }), desc ? h('span', { class: 'desc', text: desc }) : null));
    if (this.remoteState === 'on') {
      if (this.remoteUrl) {
        menuEl.append(item('Open on claude.ai', this.remoteUrl, () => invoke('open_url', { url: this.remoteUrl }).catch(err => this.notice(String(err), true)), 'link-external'));
        menuEl.append(item('Copy link', '', () => copyText(this.remoteUrl), 'copy'));
      } else {
        menuEl.append(h('div', { class: 'menu-note', text: 'On. Claude gave no link for this session; open it from the list on claude.ai/code.' }));
      }
      menuEl.append(item('Turn off', '', () => this.setRemote(false), 'circle-slash'));
    } else if (this.remoteState === 'connecting') {
      menuEl.append(h('div', { class: 'menu-note', text: 'Connecting…' }));
    } else {
      if (this.remoteState === 'failed') menuEl.append(h('div', { class: 'menu-note error', text: this.remoteDetail || 'The connection failed.' }));
      if (this.remote && this.proc == null) {
        menuEl.append(h('div', { class: 'menu-note', text: 'Turns on when Claude starts for this session.' }));
        menuEl.append(item('Start Claude now', '', () => this.ensureProcess(), 'play'));
        menuEl.append(item('Turn off', '', () => this.setRemote(false), 'circle-slash'));
      } else {
        menuEl.append(item(this.remoteState === 'failed' ? 'Try again' : 'Turn on', 'Continue this session from claude.ai/code or the Claude app', () => {
          this.remoteState = 'off';
          this.setRemote(true);
        }, 'broadcast'));
      }
    }
    const def = !!app.remoteDefault;
    menuEl.append(h('div', { class: 'menu-title', text: 'New sessions' }));
    menuEl.append(h('button', { class: `menu-item${def ? ' checked' : ''}`, onclick: () => { app.remoteDefault = !def; saveState(); refreshMenu(); } },
      icon(def ? 'check' : 'blank'), h('span', { class: 'menu-main' }, h('span', { class: 'name', text: 'Start with Remote Control on' }))));
  }

  // ---------- handoff ----------

  /**
   * Asks Claude, beside the conversation, for a Markdown handoff another session can continue
   * from, then asks where to save it (HANDOFF.md in the session's folder by default). A side
   * question adds no turn to the session.
   */
  async writeHandoff() {
    if (this.missing || this.handoffBusy) return;
    if (!this.sessionId && !this.promptWritten) {
      this.notice('Nothing to hand off yet: send Claude a message first.');
      return;
    }
    this.setHandoffBusy(true);
    const epoch = this.epoch;
    try {
      if (this.proc == null && !(await this.ensureProcess())) return;
      const reply = await this.control({ subtype: 'side_question', question: HANDOFF_QUESTION });
      if (this.epoch !== epoch || this.closing) return; // /clear started the cell over meanwhile
      const r = reply && reply.subtype === 'success' && reply.response;
      const text = r && typeof r.response === 'string' ? r.response.trim() : '';
      if (!text) {
        this.notice(`Could not write the handoff: ${reply && reply.subtype === 'error' ? reply.error || 'rejected by Claude' : 'Claude gave no answer.'}`, true);
        return;
      }
      const path = await invoke('save_text_file', { defaultName: 'HANDOFF.md', text: `${text}\n`, directory: this.cwd });
      if (path) this.notice(`Handoff saved to ${path}`);
    } catch (err) {
      this.notice(`Could not save the handoff: ${err}`, true);
    } finally {
      this.setHandoffBusy(false);
    }
  }

  setHandoffBusy(on) {
    this.handoffBusy = on;
    this.handoffBtn.disabled = on;
    this.handoffBtn.querySelector('.codicon').className = `codicon codicon-${on ? 'loading codicon-modifier-spin' : 'note'}`;
  }
}

function choice(key, label, desc, run) {
  const b = h('button', { class: 'choice', onclick: run },
    h('span', { class: 'key', text: key }),
    h('span', null, h('span', { text: label }), desc ? h('div', { class: 'desc', text: desc }) : null));
  b.run = run;
  return b;
}

function suggestionLabel(suggestions) {
  for (const s of suggestions) {
    if (s.type === 'setMode' && s.mode === 'acceptEdits') return 'Yes, allow all edits this session';
    if (s.type === 'addDirectories') return `Yes, and allow access to ${(s.directories || []).join(', ')}`;
    if (s.type === 'addRules' && Array.isArray(s.rules)) {
      const rules = s.rules.map(r => (r.ruleContent ? `${r.toolName}(${r.ruleContent})` : r.toolName)).join(', ');
      const where = s.destination === 'session' ? ' this session' : s.destination === 'localSettings' ? ' in this project' : '';
      return `Yes, and don't ask again for ${rules}${where}`;
    }
  }
  return "Yes, and don't ask again";
}

// Claude Code's synthetic reply when the plan's usage is out: "You've hit your session limit · resets 9:50pm (...)".
const LIMIT_TEXT = /\bhit your (?:\w+ )?limit\b|\busage limit reached\b/i;

/** The text of a usage-limit message, or null. Newer CLIs tag it with error 'rate_limit'; older ones only by its words. */
function usageLimitText(m) {
  const msg = m.message || {};
  const text = (Array.isArray(msg.content) ? msg.content : []).filter(b => b && b.type === 'text' && b.text).map(b => b.text).join('\n').trim();
  if (!text) return null;
  if (m.error === 'rate_limit') return text;
  return (msg.model === '<synthetic>' || m.isApiErrorMessage) && LIMIT_TEXT.test(text) ? text : null;
}

function autoGrow(ta) {
  ta.style.height = 'auto';
  if (!ta.scrollHeight) return; // not laid out yet (a detached or collapsed cell): keep the natural height
  ta.style.height = Math.min(ta.scrollHeight, 200) + 'px';
}

function onLinkClick(e) {
  const a = e.target.closest('a');
  if (!a) return;
  e.preventDefault();
  if (a.dataset.external) invoke('open_url', { url: a.getAttribute('href') }).catch(() => {});
}

/** The copy button on a code block copies the code, without the button's own label. */
function onCodeCopyClick(e) {
  const btn = e.target.closest('.code-copy');
  if (!btn) return;
  const code = btn.closest('.code-block')?.querySelector('code');
  if (!code) return;
  copyText(code.textContent);
  btn.classList.add('done');
  btn.querySelector('.codicon').className = 'codicon codicon-check';
  clearTimeout(btn._timer);
  btn._timer = setTimeout(() => {
    btn.classList.remove('done');
    btn.querySelector('.codicon').className = 'codicon codicon-copy';
  }, 1500);
}

/** A code block's Preview (Markdown shown rendered, click again for the source) and Run buttons. */
function onCodeActionClick(e, cell) {
  const btn = e.target.closest('.code-preview, .code-run');
  if (!btn) return;
  const block = btn.closest('.code-block');
  const code = block && block.querySelector('pre code');
  if (!code) return;
  if (btn.classList.contains('code-preview')) {
    const on = !block.classList.contains('previewing');
    let view = block.querySelector(':scope > .code-preview-body');
    if (on && !view) {
      view = h('div', { class: 'md code-preview-body' });
      renderMarkdown(view, code.textContent);
      block.append(view);
    }
    block.classList.toggle('previewing', on);
    btn.classList.toggle('on', on);
    btn.title = on ? 'Show the Markdown source' : 'Preview the Markdown';
    return;
  }
  // A block still streaming in may be cut short mid-command, so it cannot run until it is finished.
  if (block.closest('.streaming')) {
    cell.notice('That command is still being written; run it once the reply has finished.', true);
    return;
  }
  const command = shellCommand(code.textContent, block.dataset.lang);
  if (!command) return;
  // Invisible or direction-changing characters can make a command run differently from how it reads.
  if (/[\u00AD\u061C\u200B-\u200F\u2028-\u202E\u2060-\u2069\uFEFF\x00-\x08\x0B\x0C\x0E-\x1F\x7F]/.test(command)) {
    cell.notice('Not run: the command contains invisible or text-direction characters. Copy it and check it instead.', true);
    return;
  }
  invoke('run_in_terminal', { path: cell.cwd, command, shell: /^(cmd|bat|batch)$/.test(block.dataset.lang) ? 'cmd' : 'powershell', lang: IS_MAC ? macShell(block.dataset.lang) : null })
    .catch(err => cell.notice(`Could not open a terminal: ${err}`, true));
}

/**
 * The command a shell block runs. `console` blocks mix prompts with output, so only their prompt
 * lines are kept; elsewhere a leading `$ ` or `PS> ` prompt is dropped. A bare `> ` counts as a
 * prompt only in console blocks, since elsewhere it can be a redirect or a continuation.
 */
function shellCommand(text, lang) {
  const console = lang === 'console' || lang === 'terminal';
  const PROMPT = console ? /^\s*(?:\$|>|PS(?: [^>]*)?>)\s+/ : /^\s*(?:\$|PS(?: [^>]*)?>)\s+/;
  let lines = String(text || '').replace(/\r\n?/g, '\n').split('\n');
  if (console && lines.some(l => PROMPT.test(l))) lines = lines.filter(l => PROMPT.test(l));
  return lines.map(l => l.replace(PROMPT, '')).join('\n').trim();
}

// ---------- auto-copy: selected chat text goes to the clipboard, as in a terminal ----------

let autoCopyTip = null;
function autoCopySelection(e, area) {
  if (!app.settings.autoCopy || e.button !== 0) return;
  if (e.target.closest('textarea, input, select, button')) return;
  const sel = window.getSelection();
  const text = sel ? sel.toString() : '';
  if (!text.trim() || !area.contains(sel.anchorNode) || !area.contains(sel.focusNode)) return;
  copyText(text);
  if (!autoCopyTip) autoCopyTip = document.body.appendChild(h('div', { class: 'copy-tip', text: 'Copied' }));
  autoCopyTip.style.left = `${e.clientX + 12}px`;
  autoCopyTip.style.top = `${e.clientY - 26}px`;
  autoCopyTip.classList.remove('show');
  void autoCopyTip.offsetWidth; // restarts the fade
  autoCopyTip.classList.add('show');
}

// ---------- image viewer: a sent or attached image, full size ----------

const lightbox = $('#lightbox');
const lightboxImg = lightbox.querySelector('img');

/** Opens an image from the chat or the composer full size. Restored images that were not loaded have no source to show. */
function onImageClick(e) {
  const img = e.target.closest('.bubble-images .thumb, .attachment img');
  if (!img || !/^(data|blob):/i.test(img.currentSrc || img.src || '')) return;
  lightboxImg.src = img.currentSrc || img.src;
  lightbox.classList.remove('hidden');
}

function closeLightbox() {
  lightbox.classList.add('hidden');
  lightboxImg.removeAttribute('src');
}

lightbox.addEventListener('click', closeLightbox);
document.addEventListener('keydown', e => {
  if (e.key === 'Escape' && !lightbox.classList.contains('hidden')) {
    e.preventDefault();
    e.stopImmediatePropagation(); // not an interrupt for Claude
    closeLightbox();
  }
}, true);

// The only models HQ offers; the aliases resolve to the current 5.x releases. Anything else the
// session reports (dated or older model ids) is hidden.
const ALLOWED_MODELS = ['default', 'fable', 'opus', 'sonnet', 'haiku'];

function modelList() {
  const list = app.models.filter(m => ALLOWED_MODELS.includes(m.value));
  return list.length ? list : FALLBACK_MODELS;
}

/** Keeps only the fields HQ shows, so nothing unexpected is written to board.json. */
function sanitizeModels(models) {
  return (models || []).filter(m => m && typeof m.value === 'string').map(m => ({
    value: m.value,
    displayName: typeof m.displayName === 'string' ? m.displayName : m.value,
    description: typeof m.description === 'string' ? m.description : '',
    resolvedModel: typeof m.resolvedModel === 'string' ? m.resolvedModel : null,
    supportsEffort: !!m.supportsEffort,
    supportedEffortLevels: Array.isArray(m.supportedEffortLevels) ? m.supportedEffortLevels.filter(l => typeof l === 'string') : [],
  }));
}

/** Keeps the saved permission mode, model and effort only when they are strings. */
function sanitizeDefaults(d) {
  const out = {};
  for (const key of ['permissionMode', 'model', 'effort']) {
    if (d && typeof d[key] === 'string' && d[key]) out[key] = d[key];
  }
  return out;
}

/** Which of a question's options an answer ("label, label, typed text") picked, and any typed text left over. */
function splitAnswer(answer, options) {
  let rest = `, ${answer}, `;
  const chosen = new Set();
  const labels = options.map(o => o.label).filter(l => typeof l === 'string' && l).sort((a, b) => b.length - a.length);
  for (const label of labels) {
    if (rest.includes(`, ${label}, `)) {
      chosen.add(label);
      rest = rest.replace(`, ${label}, `, ', ');
    }
  }
  return { chosen, other: rest.replace(/^(, )+|(, )+$/g, '').trim() };
}

/** Reads `"question"="answer"` pairs from the tool's text result, for transcripts without a structured one. */
function parseAnswers(text) {
  const answers = {};
  for (const [, q, a] of String(text).matchAll(/"((?:[^"\\]|\\.)*)"="((?:[^"\\]|\\.)*)"/g)) answers[q] = a;
  return Object.keys(answers).length ? answers : null;
}

const BG_LABELS = { local_agent: 'Agent', local_bash: 'Shell', local_monitor: 'Monitor', monitor: 'Monitor' };
/** Waiting mode ends once a cell has stayed idle this long (a background task's notification can start a new turn). */
const WAIT_SETTLE_MS = 4000;
/** Trails a prompt asked to keep a task list; the chat hides it, live and on resume. */
const TASK_HINT = '(HQ: track this work with TaskCreate/TaskUpdate so the user can follow progress.)';
/** Sent to a session that was working when an update restarted HQ, which stopped its turn and its background tasks. */
const RESUME_PROMPT = 'HQ restarted to install an update while you were working. That stopped your last turn, including any '
  + 'command still running and any background tasks or agents. Carry on from where you left off: check what finished, '
  + 'rerun what was cut short, and restart any background task you still need.';
const RESUME_NOTICE = 'HQ restarted to install an update while this session was working, so it carried on by itself.';
/** Hermes' wording of TASK_HINT; hermes.rs (COMPOSER_TASK_HINT) cuts it out of replayed history. */
const HERMES_TASK_HINT = '(HQ: track this work with your todo tool so the user can follow progress.)';
const TASK_HINT_PREFIX = '(HQ: track this work';

// Sent as a side question (like /btw), so Claude answers beside the running turn without stopping it.
const HANDOFF_QUESTION = 'Write a handoff document for this session in Markdown, so that a fresh Claude session with none of '
  + 'this context can pick the work up. Reply with the document only, no preamble. Start with a "# Handoff: <topic>" heading, '
  + 'then these sections: Goal; Current state (what is done and verified, what is not); Key decisions and why; Files touched '
  + '(paths, one line each); Open problems and gotchas; Next steps (numbered, most important first). Be concrete: name files, '
  + 'commands, errors and IDs. Skip a section that has nothing in it.';
const STATUS_QUESTION = 'Progress check from the user\'s dashboard. Reply in exactly this form: a first line "Progress: N%" '
  + 'with your rough estimate of how much of the current request is done, then at most three short sentences on what '
  + 'you are doing now and what is left. If nothing is in progress, say so.';

/** 45s, 3m 12s, 1h 04m. */
function formatDuration(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
  return `${Math.floor(s / 3600)}h ${String(Math.floor(s / 60) % 60).padStart(2, '0')}m`;
}

function sanitizeTasks(tasks) {
  return (Array.isArray(tasks) ? tasks : []).filter(t => t && t.task_id).map(t => ({
    id: String(t.task_id),
    type: typeof t.task_type === 'string' ? t.task_type : '',
    description: firstLine(t.description || '', 120),
  }));
}

/**
 * Keeps only names and states from mcp_status. Server configs can hold tokens in headers,
 * env or args, so they are never kept, shown or saved.
 */
function sanitizeMcp(servers) {
  return (servers || []).filter(s => s && typeof s.name === 'string').map(s => ({
    name: s.name,
    status: typeof s.status === 'string' ? s.status : 'unknown',
    scope: typeof s.scope === 'string' ? s.scope : '',
    source: typeof s.source === 'string' ? s.source : '',
    tools: Array.isArray(s.tools) ? s.tools.length : 0,
    error: typeof s.error === 'string' ? firstLine(s.error, 200) : null,
  }));
}

// ======================================================================================
// Context indicator, slash commands, notifications, small helpers
// ======================================================================================

const CONTEXT_INFO_TTL_MS = 30 * 1000;
const ctxPop = $('#context-pop');
let ctxPopCell = null;
let ctxHideTimer = null;
let ctxAskTimer = null;

/** 120k, 1M, 22.4k. */
function formatTokens(n) {
  if (n >= 1e6) return `${(n / 1e6).toFixed(1).replace(/\.0$/, '')}M`;
  if (n >= 1e4) return `${Math.round(n / 1e3)}k`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1).replace(/\.0$/, '')}k`;
  return String(Math.round(n));
}

function formatCost(usd) {
  return usd < 0.01 ? '<$0.01' : `$${usd.toFixed(2)}`;
}

/**
 * The window size of the model that answered. `modelUsage` can list several models (title and side
 * calls use small ones), so the entry is matched by name rather than taken first.
 */
function pickContextWindow(modelUsage, model) {
  if (!modelUsage || typeof modelUsage !== 'object') return null;
  const entries = Object.entries(modelUsage).filter(([, u]) => u && Number.isFinite(u.contextWindow) && u.contextWindow > 0);
  if (!entries.length) return null;
  const bare = name => String(name).replace(/\[[^\]]*\]$/, '');
  const m = model ? bare(model) : '';
  const hit = m && (entries.find(([k]) => k === model) || entries.find(([k]) => bare(k) === m) || entries.find(([k]) => bare(k).startsWith(m) || m.startsWith(bare(k))));
  if (hit) return hit[1].contextWindow;
  return entries.length === 1 ? entries[0][1].contextWindow : null;
}

// get_context_usage names its colors after Claude Code's theme, so they are mapped to HQ's own.
const CONTEXT_COLORS = { claude: 'var(--claude)', warning: 'var(--waiting)', permission: '#93d0d8', inactive: 'var(--fg-faint)', promptborder: '#c3a6e8',
  cyan: '#8fd3c7', green: '#a8dba0', purple: '#b6a8f0', blue: '#8ab4f8', red: '#f0a6a0', orange: '#f5b78e', yellow: '#e6d38f', pink: '#f2a7c3' };

/** Keeps the numbers and the used categories of a get_context_usage reply; the rest of the 87 KB is dropped. */
function sanitizeContext(r) {
  const num = v => (Number.isFinite(v) ? v : null);
  return {
    total: num(r.totalTokens),
    max: num(r.maxTokens),
    percentage: num(r.percentage),
    threshold: num(r.autoCompactThreshold),
    autoCompact: r.isAutoCompactEnabled !== false,
    model: typeof r.model === 'string' ? r.model : '',
    categories: (Array.isArray(r.categories) ? r.categories : []).filter(c => c && c.kind === 'used' && Number.isFinite(c.tokens) && c.tokens > 0)
      .map(c => ({ name: String(c.name), tokens: c.tokens, color: CONTEXT_COLORS[String(c.color || '').split('_')[0].toLowerCase()] || 'var(--fg-muted)' })),
  };
}

function renderContextPop() {
  const cell = ctxPopCell;
  if (!cell || ctxPop.classList.contains('hidden')) return;
  const c = cell.ctx;
  const info = c.info;
  const max = (info && info.max) || c.window;
  const parts = [h('div', { class: 'usage-head' }, h('span', { text: 'Context window' }), info && info.model ? h('span', { class: 'usage-plan', text: info.model }) : null)];
  if (c.tokens != null) {
    const percent = max ? Math.round((c.tokens / max) * 100) : null;
    parts.push(h('div', { class: 'ctx-line' }, h('span', { text: max ? `${formatTokens(c.tokens)} / ${formatTokens(max)} tokens` : `${formatTokens(c.tokens)} tokens` }),
      percent != null ? h('span', { class: 'usage-pct', text: `${percent}%` }) : null));
  }
  if (info) {
    parts.push(h('div', { class: 'ctx-line muted' }, h('span', { text: info.autoCompact && info.threshold ? `Auto-compact at ${formatTokens(info.threshold)}` : 'Auto-compact off' })));
    if (info.categories.length) {
      parts.push(h('div', { class: 'ctx-sep' }));
      for (const cat of info.categories) {
        parts.push(h('div', { class: 'ctx-cat' }, h('span', { class: 'swatch', style: `background:${cat.color}` }), h('span', { class: 'name', text: cat.name }), h('span', { class: 'tokens', text: formatTokens(cat.tokens) })));
      }
    }
  } else if (cell.proc != null) {
    parts.push(h('div', { class: 'ctx-hint', text: c.loading ? 'Loading details…' : 'Details load while Claude is running.' }));
  } else {
    parts.push(h('div', { class: 'ctx-hint', text: 'Details are available while Claude is running.' }));
  }
  if (cell.cost.usd > 0) parts.push(h('div', { class: 'ctx-line muted' }, h('span', { text: 'Cost this run' }), h('span', { text: formatCost(cell.cost.usd) })));
  parts.push(h('div', { class: 'ctx-sep' }));
  parts.push(h('div', { class: 'ctx-hint', text: cell.compacting ? 'Compacting…' : cell.status === 'running' ? 'Wait for the turn to finish' : 'Click to compact · Shift+click to add instructions' }));
  ctxPop.replaceChildren(...parts);
  const r = cell.ctxBtn.getBoundingClientRect();
  let top = r.top - ctxPop.offsetHeight - 6;
  if (top < 8) top = Math.min(r.bottom + 6, window.innerHeight - ctxPop.offsetHeight - 8);
  ctxPop.style.top = `${top}px`;
  ctxPop.style.left = `${Math.max(8, Math.min(r.left, window.innerWidth - ctxPop.offsetWidth - 8))}px`;
}

function showContextPop(cell) {
  clearTimeout(ctxHideTimer);
  clearTimeout(ctxAskTimer);
  ctxPopCell = cell;
  ctxPop.classList.remove('hidden');
  renderContextPop();
  // Asked after a short hover, so passing over the button costs nothing; refreshContext keeps the answer for 30 s.
  ctxAskTimer = setTimeout(() => cell.refreshContext(false), 250);
}

function hideContextPop() {
  clearTimeout(ctxAskTimer);
  clearTimeout(ctxHideTimer);
  ctxHideTimer = setTimeout(() => { ctxPop.classList.add('hidden'); ctxPopCell = null; }, 150);
}

/** Whether a restored transcript stops in the middle of a turn: Claude's tool call or the user's prompt is the last line. */
function endsMidTurn(m) {
  const msg = m.message || {};
  const content = typeof msg.content === 'string' ? [{ type: 'text', text: msg.content }] : msg.content || [];
  if (m.type === 'assistant') return content.some(b => b.type === 'tool_use' || b.type === 'server_tool_use');
  if (m.isMeta || m.isCompactSummary) return false;
  return content.some(b => b.type === 'tool_result' || b.type === 'image'
    || (b.type === 'text' && b.text && !b.text.startsWith('[Request interrupted') && !/^\s*<(command-|local-command-|system-reminder|task-notification|ide_)/.test(b.text)));
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const area = h('textarea', { style: 'position:fixed;opacity:0' });
    area.value = text;
    document.body.append(area);
    area.select();
    document.execCommand('copy');
    area.remove();
  }
}

function renderRemoteButtons() {
  for (const c of app.cells) c.renderRemote();
}

// ---------- OS notifications ----------

const notified = { cell: null, at: 0 };

function notify(cell, body) {
  const title = cell.displayTitle() || baseName(cell.cwd) || 'HQ';
  notified.cell = cell;
  notified.at = Date.now();
  invoke('plugin:notification|notify', { options: { title, body } }).catch(err => console.warn('notification failed', err));
}

/** A finished turn while HQ is not the active window: the taskbar flashes, and a notification names the session and its reply. */
function notifyTurnDone(cell, m) {
  if (!app.settings.notify || document.hasFocus()) return;
  const failed = m.is_error || m.subtype !== 'success';
  notify(cell, failed ? 'The turn stopped with an error.' : cell.lastReply || 'Finished.');
}

function notifyWaiting(cell, req) {
  if (!app.settings.notify || document.hasFocus()) return;
  notify(cell, `Needs your answer: ${req.display_name || req.tool_name || 'a question'}`);
}

// A notification click only brings the window forward, so coming back to HQ soon after one opens its cell.
window.addEventListener('focus', () => {
  const { cell, at } = notified;
  notified.cell = null;
  if (cell && Date.now() - at < 60 * 1000 && app.cells.includes(cell)) revealCell(cell);
});

// ---------- slash commands ----------

const slashPop = $('#slash-pop');
// start..end: the "/name" being typed. `picked`: an arrow key chose an item, so Enter completes
// a "/name" typed mid-prompt; otherwise Enter there sends the prompt (it may end in a path).
const slash = { cell: null, items: [], index: 0, start: 0, end: 0, picked: false };
// Commands the CLI lists but answers with "not available here" in a headless session.
const HIDDEN_COMMANDS = new Set(['focus']);

/** Keeps name, argument hint and a short description of each command the CLI listed. */
function sanitizeCommands(list) {
  const seen = new Set();
  const out = [];
  for (const c of Array.isArray(list) ? list : []) {
    if (!c || typeof c.name !== 'string' || !c.name || seen.has(c.name) || HIDDEN_COMMANDS.has(c.name)) continue;
    seen.add(c.name);
    out.push({
      name: c.name,
      hint: typeof c.argumentHint === 'string' ? c.argumentHint : '',
      description: typeof c.description === 'string' ? firstLine(c.description, 140) : '',
      aliases: Array.isArray(c.aliases) ? c.aliases.filter(a => typeof a === 'string') : [],
    });
  }
  return out;
}

function slashClose(cell) {
  if (cell && slash.cell !== cell) return;
  slash.cell = null;
  slashPop.classList.add('hidden');
}

/**
 * The "/name" the caret is in, at the start of the composer or after a space or line break, so
 * a skill can be named mid-prompt. Null when the caret is not in one.
 */
function slashToken(input) {
  const caret = input.selectionStart;
  if (caret == null || caret !== input.selectionEnd) return null;
  const value = input.value;
  const m = /(^|\s)\/([^\s/]*)$/.exec(value.slice(0, caret));
  if (!m) return null;
  const start = m.index + m[1].length;
  const rest = /^[^\s]*/.exec(value.slice(caret))[0];
  return { start, end: caret + rest.length, query: m[2] + rest };
}

/** Shows the commands matching the "/name" the caret is in. */
function slashUpdate(cell) {
  const token = slashToken(cell.input);
  // After other text a lone "/" is not taken as a command until a letter follows it.
  const commands = cell.backend === 'hermes' ? hermes.commands : app.commands;
  if (!token || !commands.length || cell.collapsed || (token.start > 0 && !token.query)) { slashClose(cell); return; }
  const q = token.query.toLowerCase();
  const rank = c => {
    const n = c.name.toLowerCase();
    if (n === q) return 0;
    if (n.startsWith(q)) return 1;
    if (c.aliases.some(a => a.toLowerCase().startsWith(q))) return 2;
    return n.includes(q) ? 3 : 9;
  };
  // After other text, "/x" is as likely a path as a command, so only names starting with it are offered.
  const worst = token.start === 0 ? 9 : 3;
  const items = commands.map(c => [rank(c), c]).filter(([r]) => r < worst)
    .sort((a, b) => a[0] - b[0] || a[1].name.localeCompare(b[1].name)).map(x => x[1]).slice(0, 40);
  if (!items.length) { slashClose(cell); return; }
  Object.assign(slash, { cell, items, index: 0, start: token.start, end: token.end, picked: false });
  renderSlash();
}

function renderSlash() {
  const cell = slash.cell;
  if (!cell) return;
  slashPop.replaceChildren(...slash.items.map((c, i) => {
    const row = h('button', { class: `slash-item${i === slash.index ? ' active' : ''}`, tabindex: '-1', role: 'option' },
      h('span', { class: 'name', text: `/${c.name}` }),
      c.hint ? h('span', { class: 'hint', text: c.hint }) : null,
      h('span', { class: 'desc', text: c.description, title: c.description }));
    row.addEventListener('mousedown', e => { e.preventDefault(); slashComplete(cell, c); }); // mousedown: the textarea keeps its focus
    return row;
  }));
  slashPop.classList.remove('hidden');
  const r = cell.input.getBoundingClientRect();
  slashPop.style.width = `${Math.min(420, Math.max(240, r.width))}px`;
  let top = r.top - slashPop.offsetHeight - 4;
  if (top < 8) top = Math.min(r.bottom + 4, window.innerHeight - slashPop.offsetHeight - 8);
  slashPop.style.top = `${top}px`;
  slashPop.style.left = `${Math.max(8, Math.min(r.left, window.innerWidth - slashPop.offsetWidth - 8))}px`;
  slashPop.querySelector('.active').scrollIntoView({ block: 'nearest' });
}

function slashComplete(cell, command) {
  const value = cell.input.value;
  const after = value.slice(slash.end).replace(/^ /, '');
  const insert = `/${command.name} `;
  cell.input.value = value.slice(0, slash.start) + insert + after;
  const caret = slash.start + insert.length;
  cell.input.setSelectionRange(caret, caret);
  slashClose(cell);
  autoGrow(cell.input);
  cell.input.focus();
  saveState();
}

/** Arrow keys pick, Tab or Enter completes, Esc closes. Enter on a name that is already complete sends it. */
function slashKeydown(cell, e) {
  if (slash.cell !== cell || slashPop.classList.contains('hidden') || e.isComposing) return false;
  const current = slash.items[slash.index];
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    slash.index = (slash.index + (e.key === 'ArrowDown' ? 1 : slash.items.length - 1)) % slash.items.length;
    slash.picked = true;
    renderSlash();
    return true;
  }
  if (e.key === 'Escape') {
    e.preventDefault();
    e.stopPropagation(); // not an interrupt for Claude
    slashClose(cell);
    return true;
  }
  const enterCompletes = e.key === 'Enter' && !e.shiftKey && (slash.start === 0 || slash.picked)
    && cell.input.value.slice(slash.start, slash.end) !== `/${current.name}`;
  if (e.key === 'Tab' || enterCompletes) {
    e.preventDefault();
    slashComplete(cell, current);
    return true;
  }
  return false;
}

// ======================================================================================
// Popup menu (model/effort, MCP servers)
// ======================================================================================

const menu = $('#menu');
const menuState = { anchor: null, cell: null, build: null };

function openMenu(anchor, cell, build) {
  if (menuState.anchor === anchor) { closeMenu(); return; }
  Object.assign(menuState, { anchor, cell, build });
  menu.classList.remove('hidden');
  refreshMenu();
}

function refreshMenu() {
  if (!menuState.anchor) return;
  if (!menuState.anchor.isConnected) { closeMenu(); return; }
  menu.replaceChildren();
  menuState.build(menu);
  positionMenu();
}

/** Opens above the anchor (the composer sits at the bottom of a cell), or below when there is no room. */
function positionMenu() {
  const r = menuState.anchor.getBoundingClientRect();
  const width = menu.offsetWidth;
  const height = menu.offsetHeight;
  const left = Math.max(8, Math.min(r.left, window.innerWidth - width - 8));
  let top = r.top - height - 4;
  if (top < 8) top = Math.max(8, Math.min(r.bottom + 4, window.innerHeight - height - 8));
  menu.style.left = `${left}px`;
  menu.style.top = `${top}px`;
}

function closeMenu() {
  menu.classList.add('hidden');
  Object.assign(menuState, { anchor: null, cell: null, build: null });
}

document.addEventListener('mousedown', e => {
  if (menuState.anchor && !menu.contains(e.target) && !menuState.anchor.contains(e.target)) closeMenu();
});

// ======================================================================================
// Board: add / remove / focus / layout
// ======================================================================================

/** The project name's menu: the palette, any colour, a random one, or back to the default. */
function buildColorMenu(menuEl, path) {
  const current = projectColor(path).toLowerCase();
  const swatches = h('div', { class: 'swatches' }, PROJECT_COLORS.map(color => h('button', {
    class: `swatch${color.toLowerCase() === current ? ' on' : ''}`, style: `--swatch:${color}`, title: color,
    onclick: () => { setProjectColor(path, color); refreshMenu(); },
  })));
  // Live while the picker is dragged; the menu is redrawn only when the picker closes, so it stays open.
  const custom = h('input', { type: 'color', value: current, title: 'Any colour' });
  custom.addEventListener('input', () => setProjectColor(path, custom.value));
  custom.addEventListener('change', () => refreshMenu());
  const item = (name, ico, run) => h('button', { class: 'menu-item', onclick: () => { run(); refreshMenu(); } }, icon(ico), h('span', { class: 'menu-main' }, h('span', { class: 'name', text: name })));
  menuEl.append(...[h('div', { class: 'menu-title', text: `Colour · ${baseName(path) || path}` }), swatches,
    h('label', { class: 'menu-item color-custom' }, icon('symbol-color'), h('span', { class: 'menu-main' }, h('span', { class: 'name', text: 'Custom…' })), custom),
    item('Shuffle', 'refresh', () => shuffleProjectColor(path)),
    Object.hasOwn(projectColorPicks, normPath(path).toLowerCase()) ? item('Reset to default', 'discard', () => setProjectColor(path, null)) : null].filter(Boolean));
}

/** Cells in the same folder belong to one project; they share a color and sit together. */
const projectKey = path => normPath(path).toLowerCase();

/**
 * Adds a cell right after the last open cell of its project, so the grid, the sessions list
 * and the Ctrl+1..9 keys show sessions grouped by project, projects in the order first opened.
 */
function addCell(opts, { restore = false, batch = false } = {}) {
  const cell = new Cell(opts);
  placeCell(cell);
  // A batch (the restore at startup) lays the board out once, when it is done.
  if (!batch) layout();
  focusCell(cell, !batch);
  // Claude starts on the first message, so an empty cell costs no claude process.
  if (restore) {
    if (cell.collapsed) cell.historyPending = true; // drawn when the cell is first opened
    else cell.loadHistory();
  } else {
    rememberFolder(cell.cwd);
  }
  if (!batch) {
    saveState();
    renderSidebar();
  }
  return cell;
}

/** Puts a cell right after the last open cell of its project, so projects stay grouped in first-opened order. */
function placeCell(cell) {
  const i = app.cells.indexOf(cell);
  if (i >= 0) app.cells.splice(i, 1);
  const key = projectKey(cell.cwd);
  const last = app.cells.findLastIndex(c => projectKey(c.cwd) === key);
  if (last >= 0) app.cells.splice(last + 1, 0, cell);
  else app.cells.push(cell);
}

/** Opens a new session with the permission mode, model and effort the user picked last. */
function addNewCell(opts, flags) {
  const { permissionMode = 'default', model = null, effort = null } = opts.backend === 'hermes' ? hermes.defaults : app.defaults;
  return addCell({ permissionMode, model, effort, ...opts }, flags);
}

/** Remembers a permission mode, model or effort picked in a cell, as the default for new sessions. */
function rememberChoice(choice) {
  app.defaults = { ...app.defaults, ...choice };
  saveState();
}

/** The .code-workspace file the VS Code button opens for a folder, if one was picked. */
function workspaceFor(cwd) {
  return app.workspaces[projectKey(cwd)] || null;
}

function setWorkspace(cwd, path) {
  const key = projectKey(cwd);
  delete app.workspaces[key]; // re-added last, so workspaceDirs() sees it as the newest pick
  if (path) app.workspaces[key] = path;
  for (const c of app.cells) if (projectKey(c.cwd) === key) c.renderCodeButton();
  saveState();
}

/** The folders holding the workspace files picked so far, most recent pick first. */
function workspaceDirs() {
  const dirs = new Map();
  for (const path of Object.values(app.workspaces).reverse()) {
    const dir = parentDir(path);
    if (dir && !dirs.has(projectKey(dir))) dirs.set(projectKey(dir), dir);
  }
  return [...dirs.values()];
}

function parentDir(path) {
  const p = normPath(path);
  const i = p.lastIndexOf('\\');
  // A Unix path keeps its slashes, and the parent of /Users is the root.
  return i > 0 ? nativeSep(p.slice(0, i), path) : i === 0 && String(path).startsWith('/') && p.length > 1 ? '/' : '';
}

/** Keeps string-to-string entries only. */
function sanitizeWorkspaces(w) {
  const out = {};
  for (const [key, path] of Object.entries(w && typeof w === 'object' ? w : {})) {
    if (typeof path === 'string' && path) out[key] = path;
  }
  return out;
}

function sanitizeProjectColors(c) {
  const out = {};
  for (const [key, color] of Object.entries(c && typeof c === 'object' ? c : {})) {
    if (typeof color === 'string' && /^#[0-9a-f]{6}$/i.test(color)) out[key] = color;
  }
  return out;
}

/** Gives a folder another colour from the palette, in every cell and list that shows it. */
function shuffleProjectColor(path) {
  const current = projectColor(path);
  const others = PROJECT_COLORS.filter(c => c.toLowerCase() !== current.toLowerCase());
  setProjectColor(path, others[Math.floor(Math.random() * others.length)]);
}

/** A `#rrggbb` colour for the folder, or null for its default; repaints its cells and list rows. */
function setProjectColor(path, color) {
  const key = normPath(path).toLowerCase();
  if (color && /^#[0-9a-f]{6}$/i.test(color)) projectColorPicks[key] = color.toLowerCase();
  else delete projectColorPicks[key];
  for (const c of app.cells) {
    if (projectKey(c.cwd) !== projectKey(path)) continue;
    c.color = projectColor(c.cwd);
    c.el.style.setProperty('--proj', c.color);
  }
  renderSidebar();
  saveState();
}

function closeCell(cell) {
  cell.dispose();
  if (peeked === cell) peeked = null;
  const i = app.cells.indexOf(cell);
  if (i >= 0) app.cells.splice(i, 1);
  cell.el.remove();
  if (app.maximized === cell) app.maximized = null;
  if (app.focused === cell) {
    app.focused = null;
    const open = app.cells.filter(c => !c.collapsed);
    const next = open.find(c => app.cells.indexOf(c) >= i) || open.at(-1) || app.cells[Math.min(i, app.cells.length - 1)];
    if (next) focusCell(next, true);
  }
  layout();
  updateSummary();
  saveState();
}

function focusCell(cell, focusInput) {
  if (app.focused && app.focused !== cell) app.focused.el.classList.remove('focused');
  const changed = app.focused !== cell;
  app.focused = cell;
  cell.el.classList.add('focused');
  if (cell.unread) { cell.unread = false; cell.refreshState(); }
  if (changed) renderSidebar();
  if (focusInput) {
    const firstInput = cell.promptsMin ? null : cell.prompts.querySelector('input, textarea');
    (firstInput || cell.input).focus();
  }
}

/** Focuses a cell from the sessions list, brings it into view if another is maximized, and flashes it. */
function revealCell(cell) {
  if (cell.collapsed) cell.setCollapsed(false);
  if (app.maximized && app.maximized !== cell) {
    app.maximized = cell;
    layout();
  }
  focusCell(cell, true);
  cell.el.classList.remove('flash');
  void cell.el.offsetWidth; // restart the animation
  cell.el.classList.add('flash');
  clearTimeout(cell.flashTimer);
  cell.flashTimer = setTimeout(() => cell.el.classList.remove('flash'), 1300);
}

function toggleMaximize(cell) {
  if (cell.collapsed) cell.setCollapsed(false);
  app.maximized = app.maximized === cell ? null : cell;
  layout();
  focusCell(cell, true);
}

/** Collapsing the focused cell hands focus to the nearest open one, so keys never act on a hidden chat. */
function collapseCell(cell, on) {
  cell.setCollapsed(on);
  if (!on) { focusCell(cell, true); return; }
  if (app.focused !== cell) return;
  const i = app.cells.indexOf(cell);
  const open = app.cells.filter(c => !c.collapsed);
  const next = open.find(c => app.cells.indexOf(c) > i) || open.at(-1);
  if (next) focusCell(next, true);
}

/**
 * Picks how many rows to use so cells stay as close to square as possible, then fills the
 * rows left to right. Rows can hold different counts, so there are never empty slots.
 */
function chooseRows(n, width, height) {
  let best = 1;
  let bestScore = Infinity;
  for (let rows = 1; rows <= n; rows++) {
    const perRow = distribute(n, rows);
    const cellH = height / rows;
    let score = 0;
    for (const count of perRow) {
      const cellW = width / count;
      score += count * Math.abs(Math.log(cellW / cellH));
      if (cellW < 380) score += count * 2; // chat becomes unreadable when too narrow
      if (cellH < 260) score += count * 2;
    }
    score /= n;
    if (score < bestScore - 1e-9) { bestScore = score; best = rows; }
  }
  return best;
}

function distribute(n, rows) {
  const base = Math.floor(n / rows);
  const extra = n % rows;
  return Array.from({ length: rows }, (_, i) => base + (i < extra ? 1 : 0));
}

function layout() {
  $('#empty').classList.toggle('hidden', app.cells.length > 0);
  grid.classList.toggle('maximized', !!app.maximized);

  // Collapsed cells sit in the dock as bare headers; the grid lays out the rest.
  const shown = app.cells.filter(c => !c.collapsed);
  const docked = app.cells.filter(c => c.collapsed).map(c => c.el);
  if (dock.children.length !== docked.length || docked.some((el, i) => dock.children[i] !== el)) dock.replaceChildren(...docked);
  grid.classList.toggle('all-collapsed', !shown.length && app.cells.length > 0);

  // Preserve scroll positions: re-parenting a scroller can reset it.
  const scrolls = shown.map(c => [c, c.msgs.scrollTop, c.stickToBottom]);

  const n = shown.length;
  const rect = grid.getBoundingClientRect(); // after the dock is filled, which takes its height
  const rowsWanted = n ? chooseRows(n, rect.width, rect.height) : 0;
  const counts = distribute(n, rowsWanted);
  while (grid.children.length > rowsWanted) grid.lastElementChild.remove();
  while (grid.children.length < rowsWanted) grid.append(h('div', { class: 'row' }));

  let k = 0;
  counts.forEach((count, r) => {
    const row = grid.children[r];
    const want = shown.slice(k, k + count).map(c => c.el);
    k += count;
    const same = row.children.length === want.length && want.every((el, i) => row.children[i] === el);
    if (!same) row.replaceChildren(...want);
    row.classList.toggle('has-max', !!app.maximized && want.includes(app.maximized.el));
    for (const el of want) el.classList.toggle('max', !!app.maximized && el === app.maximized.el);
  });

  // Writes only: a scrollTop past the end is clamped, so scrollHeight is not read once per cell.
  for (const [c, top, stick] of scrolls) {
    c.msgs.scrollTop = stick ? 1e9 : top;
  }
  if (menuState.anchor) refreshMenu();
}

function updateSummary() {
  let waiting = 0, running = 0, done = 0, background = 0;
  for (const c of app.cells) {
    background += c.bgTasks.length;
    if (c.pending.size) waiting++;
    else if (c.status === 'running') running++;
    else if (c.unread) done++;
  }
  const pills = [];
  if (waiting) pills.push(h('span', { class: 'pill waiting', text: `${waiting} waiting on you` }));
  if (done) pills.push(h('span', { class: 'pill done', text: `${done} finished` }));
  if (running) pills.push(h('span', { class: 'pill running', text: `${running} working` }));
  if (background) pills.push(h('span', { class: 'pill background', text: `${background} background task${background === 1 ? '' : 's'}` }));
  $('#summary').replaceChildren(...pills);
  const name = build.profile ? `HQ (${build.profile})` : 'HQ';
  document.title = waiting ? `(${waiting}) ${name}` : name;
  renderSidebar();
}

// ======================================================================================
// Sessions list (collapsible sidebar)
// ======================================================================================

const sidebar = $('#sidebar');
const sessionList = $('#session-list');
const searchInput = $('#session-search');
let sidebarFrame = 0;

function stateOf(cell) {
  if (cell.missing) return ['state-exited', 'Folder missing'];
  if (cell.pending.size) return ['state-waiting', 'Waiting on you'];
  if (cell.status === 'running') return ['state-running', 'Working'];
  if (cell.bgTasks.length) return ['state-bg', `${cell.bgTasks.length} in background`];
  if (cell.unread) return ['unread', 'Finished'];
  if (cell.status === 'exited') return ['state-exited', 'Stopped'];
  if (!cell.sessionId && cell.proc == null) return ['', 'New'];
  return ['', 'Idle'];
}

function matchesQuery(cell, terms) {
  if (!terms.length) return true;
  const hay = [baseName(cell.cwd), cell.cwd, cell.customTitle, cell.title, cell.lastReply].join('\n').toLowerCase();
  return terms.every(t => hay.includes(t));
}

/** Redraws the list at most once per frame; cells call this on every state change. */
function renderSidebar() {
  if (!app.sidebarOpen || sidebarFrame) return;
  sidebarFrame = requestAnimationFrame(() => {
    sidebarFrame = 0;
    const terms = app.query.toLowerCase().split(/\s+/).filter(Boolean);
    const rows = [];
    app.cells.forEach((cell, i) => {
      if (!matchesQuery(cell, terms)) return;
      const [cls, label] = stateOf(cell);
      const row = h('button', { class: `session ${cls}${cell === app.focused ? ' active' : ''}`, title: cell.cwd },
        h('span', { class: 'status-dot' }),
        h('span', { class: 'session-main' },
          h('span', { class: 'session-line' },
            h('span', { class: 'session-name', text: baseName(cell.cwd) || cell.cwd }),
            h('span', { class: 'session-state', text: label })),
          h('span', { class: 'session-title', text: cell.displayTitle() || 'No messages yet' }),
          cell.sideTaskLine ? h('span', { class: 'session-tasks', text: cell.sideTaskLine }) : null,
          cell.lastReply ? h('span', { class: 'session-reply', text: cell.lastReply }) : null,
          h('span', { class: 'session-path', text: cell.cwd })),
        i < 9 ? h('span', { class: 'session-key', text: `^${i + 1}` }) : null);
      row.style.setProperty('--proj', cell.color);
      row.cell = cell;
      row.addEventListener('click', () => revealCell(cell));
      rows.push(row);
    });
    if (!rows.length) rows.push(h('div', { class: 'session-empty', text: app.cells.length ? 'No sessions match.' : 'No sessions yet.' }));
    sessionList.replaceChildren(...rows);
    $('#session-count').textContent = app.cells.length ? String(app.cells.length) : '';
  });
}

/** Lightly marks the cell a sessions-list row points at while the row is hovered or focused. */
let peeked = null;
function peekCell(cell) {
  if (peeked === cell) return;
  if (peeked) peeked.el.classList.remove('peek');
  peeked = cell;
  if (cell) cell.el.classList.add('peek');
}
const rowCell = e => {
  const row = e.target.closest && e.target.closest('.session');
  return (row && row.cell) || null;
};
sessionList.addEventListener('mouseover', e => peekCell(rowCell(e)));
sessionList.addEventListener('mouseleave', () => peekCell(null));
sessionList.addEventListener('focusin', e => peekCell(rowCell(e)));
sessionList.addEventListener('focusout', () => peekCell(null));

function setSidebar(open, focusSearch) {
  app.sidebarOpen = open;
  if (!open) peekCell(null);
  sidebar.classList.toggle('hidden', !open);
  $('#toggle-sidebar').classList.toggle('on', open);
  if (open) {
    renderSidebar();
    if (focusSearch) searchInput.focus();
  }
  saveState();
}

$('#toggle-sidebar').addEventListener('click', () => setSidebar(!app.sidebarOpen, false));
searchInput.addEventListener('input', () => {
  app.query = searchInput.value;
  renderSidebar();
});
searchInput.addEventListener('keydown', e => {
  if (e.key === 'Escape') {
    e.stopPropagation();
    if (searchInput.value) { searchInput.value = ''; app.query = ''; renderSidebar(); }
    else if (app.focused) focusCell(app.focused, true);
  } else if (e.key === 'Enter') {
    e.preventDefault();
    const first = sessionList.querySelector('.session');
    if (first) first.click();
  } else if (e.key === 'ArrowDown') {
    e.preventDefault();
    const first = sessionList.querySelector('.session');
    if (first) first.focus();
  }
});
sessionList.addEventListener('keydown', e => {
  if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
  e.preventDefault();
  const rows = [...sessionList.querySelectorAll('.session')];
  const i = rows.indexOf(document.activeElement);
  if (e.key === 'ArrowUp' && i <= 0) { searchInput.focus(); return; }
  const next = rows[Math.min(rows.length - 1, i + 1 - (e.key === 'ArrowUp' ? 2 : 0))];
  if (next) next.focus();
});

// ======================================================================================
// New-session picker
// ======================================================================================

const picker = $('#picker');
let pickerIndex = -1;

async function openPicker() {
  closeResumer();
  const list = $('#picker-recent');
  list.replaceChildren();
  const exists = await Promise.all(app.recent.map(p => invoke('folder_exists', { path: p }).catch(() => false)));
  app.recent.forEach((path, i) => {
    if (!exists[i]) return;
    const forget = h('span', { class: 'icon-btn forget', title: 'Remove from list' }, icon('close'));
    forget.addEventListener('click', e => {
      e.stopPropagation();
      app.recent = app.recent.filter(p => p !== path);
      saveState();
      openPicker();
    });
    list.append(h('button', { class: 'picker-item', 'data-path': path, onclick: () => { closePicker(); addNewCell({ cwd: path, backend: pickerBackend() }); } },
      icon('folder'), h('span', { class: 'name', text: baseName(path) || path }), h('span', { class: 'path', text: path }), forget));
  });
  pickerIndex = -1;
  hermesPickerSwitch(picker);
  picker.classList.remove('hidden');
  if (!list.children.length) browseFolder();
  else picker.querySelector('.picker-item').focus();
}

function closePicker() {
  picker.classList.add('hidden');
}

async function browseFolder() {
  closePicker();
  const path = await invoke('pick_folder').catch(() => null);
  if (path) addNewCell({ cwd: path, backend: pickerBackend() });
}

function rememberFolder(path) {
  app.recent = [path, ...app.recent.filter(p => p.toLowerCase() !== path.toLowerCase())].slice(0, MAX_RECENT);
}

picker.addEventListener('keydown', e => {
  const items = [...picker.querySelectorAll('.picker-item')];
  if (e.key === 'Escape') { e.stopPropagation(); closePicker(); return; }
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    const cur = items.indexOf(document.activeElement);
    const next = e.key === 'ArrowDown' ? Math.min(cur + 1, items.length - 1) : Math.max(cur - 1, 0);
    items[next].focus();
  }
});
document.addEventListener('mousedown', e => {
  if (!picker.classList.contains('hidden') && !picker.contains(e.target) && !e.target.closest('#new-session, #empty-new')) closePicker();
});
$('#picker-browse').addEventListener('click', browseFolder);
$('#new-session').addEventListener('click', () => (picker.classList.contains('hidden') ? openPicker() : closePicker()));
$('#empty-new').addEventListener('click', openPicker);

// ======================================================================================
// Resume a session started elsewhere (VS Code, the terminal, or an earlier HQ cell)
// ======================================================================================

const resumer = $('#resumer');
const resumeList = $('#resume-list');
const resumeSearch = $('#resume-search');
const SOURCE_LABELS = { 'claude-vscode': 'VS Code', cli: 'Terminal', 'sdk-cli': 'HQ', 'claude-desktop': 'Desktop' };
// A transcript written this recently may still be open somewhere else.
const RECENTLY_ACTIVE_MS = 5 * 60 * 1000;
const resumeProject = $('#resume-project');
// `all`: the newest sessions from every folder, which the folder list is built from.
// `project`: the folder picked in the list ('' for all); its sessions are fetched on their own,
// so older ones show too. `seq` drops a reply that a newer request has overtaken.
const resumeState = { sessions: [], all: [], project: '', source: '', loading: false, error: null, seq: 0,
  deep: false, hits: [], deepLoading: false, deepError: null, deepSeq: 0 }; // deep: search inside the conversations

async function openResumer() {
  closePicker();
  closeSettings();
  closeMenu();
  resumer.classList.remove('hidden');
  resumeSearch.value = '';
  resumeState.project = '';
  hermesResumeFilter($('#resume-filters'));
  renderProjectOptions();
  resumeSearch.focus();
  await loadResumeSessions();
}

async function loadResumeSessions() {
  const seq = ++resumeState.seq;
  const cwd = resumeState.project || null;
  resumeState.loading = true;
  resumeState.error = null;
  renderResumeList();
  let sessions = null;
  let error = null;
  const others = hermesSessions(cwd); // read alongside, added once Claude's are shown
  resumeState.othersLoading = !!others;
  try {
    sessions = await invoke('list_sessions', { limit: 80, cwd });
  } catch (err) {
    error = String(err);
  }
  if (seq !== resumeState.seq) return;
  if (sessions) {
    resumeState.sessions = sessions;
    if (!cwd) {
      resumeState.all = sessions;
      renderProjectOptions();
    }
  }
  resumeState.error = error;
  resumeState.loading = false;
  renderResumeList();
  if (!others) return;
  const extra = await others;
  if (seq !== resumeState.seq) return;
  resumeState.othersLoading = false;
  if (!extra.length) { renderResumeList(); return; }
  resumeState.error = null;
  resumeState.sessions = [...(sessions || []), ...extra].sort((a, b) => b.modifiedMs - a.modifiedMs);
  if (!cwd) {
    resumeState.all = resumeState.sessions;
    renderProjectOptions();
  }
  renderResumeList();
}

/** Every folder with a recent session, an open cell or a place in the recent list, by name. */
function renderProjectOptions() {
  const folders = new Map();
  const add = path => { if (path && !folders.has(projectKey(path))) folders.set(projectKey(path), path); };
  resumeState.all.forEach(s => add(s.cwd));
  app.cells.forEach(c => add(c.cwd));
  app.recent.forEach(add);
  add(resumeState.project);
  const name = path => baseName(path) || path;
  const counts = new Map();
  for (const path of folders.values()) counts.set(name(path).toLowerCase(), (counts.get(name(path).toLowerCase()) || 0) + 1);
  const sorted = [...folders.values()].sort((a, b) => name(a).localeCompare(name(b), undefined, { sensitivity: 'base' }) || a.localeCompare(b));
  resumeProject.replaceChildren(h('option', { value: '', text: 'All folders' }), ...sorted.map(path => h('option', {
    value: path, title: path,
    text: counts.get(name(path).toLowerCase()) > 1 ? `${name(path)} — ${path}` : name(path), // two folders share a name
  })));
  resumeProject.value = resumeState.project;
  resumeProject.classList.toggle('on', !!resumeState.project);
}

function closeResumer() {
  resumer.classList.add('hidden');
}

/** "14:05" today, "Oct 3 14:05" this year, else with the year. */
function shortDateTime(ms) {
  const d = new Date(ms);
  const now = new Date();
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  if (d.toDateString() === now.toDateString()) return time;
  const date = d.toLocaleDateString([], d.getFullYear() === now.getFullYear() ? { month: 'short', day: 'numeric' } : { year: 'numeric', month: 'short', day: 'numeric' });
  return `${date} ${time}`;
}

function timeAgo(ms) {
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  if (s < 7 * 86400) return `${Math.floor(s / 86400)}d ago`;
  return new Date(ms).toLocaleDateString();
}

/** Matches of the search inside conversations, streamed in by the backend. */
function renderDeepHits() {
  const q = resumeSearch.value.trim();
  const rows = [];
  for (const hit of resumeState.hits) {
    const at = Date.parse(hit.timestamp) || 0;
    const info = { sessionId: hit.sessionId, cwd: hit.cwd, title: hit.title, lastPrompt: hit.snippet, modifiedMs: at };
    const open = app.cells.find(c => c.sessionId === hit.sessionId);
    const snippet = h('span', { class: 'snippet' });
    const lower = hit.snippet.toLowerCase();
    const i = lower.indexOf(q.toLowerCase());
    if (i >= 0) snippet.append(hit.snippet.slice(0, i), h('mark', { text: hit.snippet.slice(i, i + q.length) }), hit.snippet.slice(i + q.length));
    else snippet.textContent = hit.snippet;
    const row = h('button', { class: 'hit', disabled: hit.cwdExists === false, title: `${hit.cwd}\n${hit.sessionId}` },
      h('span', { class: 'resume-line' }, h('span', { class: 'resume-title', text: hit.title || '(no title)' }),
        open ? h('span', { class: 'badge on', text: 'Open in HQ' }) : null,
        hit.cwdExists === false ? h('span', { class: 'badge err', text: 'Folder missing' }) : null,
        at ? h('span', { class: 'resume-time', text: timeAgo(at) }) : null),
      snippet,
      h('span', { class: 'resume-path', text: `${hit.role === 'user' ? 'You' : 'Claude'} · ${hit.cwd}` }));
    row.style.setProperty('--proj', projectColor(hit.cwd));
    row.addEventListener('click', () => resumeSession(info, false));
    rows.push(row);
  }
  if (!rows.length) {
    const text = resumeState.deepError || (q.length < 2 ? 'Type at least two characters to search inside conversations.' : resumeState.deepLoading ? 'Searching…' : 'No conversation contains that text.');
    rows.push(h('div', { class: `session-empty${resumeState.deepError ? ' error' : ''}`, text }));
  } else if (resumeState.deepLoading) {
    rows.push(h('div', { class: 'session-empty', text: 'Searching…' }));
  }
  resumeList.replaceChildren(...rows);
}

/** Starts the search inside conversations; a newer search (or leaving the mode) drops the older one's hits. */
function runDeepSearch() {
  const q = resumeSearch.value.trim();
  const seq = ++resumeState.deepSeq;
  resumeState.hits = [];
  resumeState.deepError = null;
  resumeState.deepLoading = q.length >= 2;
  renderResumeList();
  if (q.length < 2) return;
  let frame = 0;
  const channel = new Channel();
  channel.onmessage = hit => {
    if (seq !== resumeState.deepSeq) return;
    for (const one of Array.isArray(hit) ? hit : [hit]) if (one && typeof one.snippet === 'string') resumeState.hits.push(one);
    if (!frame) frame = requestAnimationFrame(() => { frame = 0; renderResumeList(); });
  };
  invoke('search_sessions', { query: q, cwd: resumeState.project || null, limit: 100, onHit: channel })
    .catch(err => { if (seq === resumeState.deepSeq) resumeState.deepError = String(err); })
    .finally(() => { if (seq === resumeState.deepSeq) { resumeState.deepLoading = false; renderResumeList(); } });
}

function renderResumeList() {
  if (resumeState.deep) { renderDeepHits(); return; }
  if (resumeState.loading && !resumeState.sessions.length) {
    resumeList.replaceChildren(h('div', { class: 'session-empty', text: 'Reading sessions…' }));
    return;
  }
  if (resumeState.error) {
    resumeList.replaceChildren(h('div', { class: 'session-empty error', text: resumeState.error }));
    return;
  }
  const terms = resumeSearch.value.toLowerCase().split(/\s+/).filter(Boolean);
  const project = resumeState.project ? projectKey(resumeState.project) : null;
  const rows = [];
  for (const info of resumeState.sessions) {
    if (resumeState.source && info.entrypoint !== resumeState.source) continue;
    if (project && projectKey(info.cwd) !== project && !(info.lastCwd && projectKey(info.lastCwd) === project)) continue;
    const title = info.title || info.lastPrompt || '(no prompt)';
    const hay = [title, info.lastPrompt, info.cwd, info.lastCwd].join('\n').toLowerCase();
    if (!terms.every(t => hay.includes(t))) continue;

    const open = app.cells.find(c => c.sessionId === info.sessionId);
    const recent = Date.now() - info.modifiedMs < RECENTLY_ACTIVE_MS;
    const source = SOURCE_LABELS[info.entrypoint] || info.entrypoint || '';
    const badges = [
      source ? h('span', { class: 'badge', text: source }) : null,
      open ? h('span', { class: 'badge on', text: 'Open in HQ' }) : null,
      recent && !open ? h('span', { class: 'badge warn', text: 'Active', title: 'Changed in the last few minutes. If it is still open in VS Code or a terminal, close it there first, or fork it instead.' }) : null,
      !info.cwdExists ? h('span', { class: 'badge err', text: 'Folder missing' }) : null,
    ];
    const fork = h('button', { class: 'icon-btn fork', title: 'Fork: continue in a copy and leave the original untouched' }, icon('repo-forked'));
    fork.addEventListener('click', e => { e.stopPropagation(); resumeSession(info, true); });
    const startedMs = Date.parse(info.started || '');
    const started = Number.isFinite(startedMs) ? h('span', { class: 'resume-started', text: `started ${shortDateTime(startedMs)}`, title: `Started ${new Date(startedMs).toLocaleString()}` }) : null;
    // The folder it started in: the session is resumed there. A click shows only that folder's sessions.
    const path = h('span', { class: 'resume-path filter-link', text: info.cwd, title: 'Show only sessions from this folder' });
    path.addEventListener('click', e => { e.stopPropagation(); setResumeProject(info.cwd); });
    const row = h('button', { class: 'resume-item', disabled: !info.cwdExists, title: `${info.cwd}\n${info.sessionId}` },
      h('span', { class: 'resume-main' },
        h('span', { class: 'resume-line' }, h('span', { class: 'resume-title', text: title }), started,
          h('span', { class: 'resume-time', text: `· ${timeAgo(info.modifiedMs)}`, title: `Last used ${new Date(info.modifiedMs).toLocaleString()}` })),
        info.title && info.lastPrompt ? h('span', { class: 'resume-prompt', text: info.lastPrompt }) : null,
        h('span', { class: 'resume-line' }, path, badges),
        info.lastCwd ? h('span', { class: 'resume-moved', text: `last used in ${info.lastCwd}`, title: info.lastCwd }) : null),
      open || !info.cwdExists ? null : fork);
    row.style.setProperty('--proj', projectColor(info.cwd));
    row.addEventListener('click', () => resumeSession(info, false));
    rows.push(row);
  }
  if (!rows.length) {
    const text = resumeState.loading ? 'Reading sessions…' : resumeState.sessions.length ? 'No sessions match.' : 'No sessions found.';
    rows.push(h('div', { class: 'session-empty', text }));
  }
  if (resumeState.othersLoading && !resumeState.loading) rows.unshift(hermesSessionsLoading());
  resumeList.replaceChildren(...rows);
}

function resumeSession(info, fork) {
  closeResumer();
  const open = app.cells.find(c => c.sessionId === info.sessionId);
  if (open && !fork) { revealCell(open); return; }
  rememberFolder(info.cwd);
  const cell = addNewCell({ cwd: info.cwd, sessionId: info.sessionId, title: info.title || info.lastPrompt || '', fork, backend: info.backend }, { restore: true });
  // Read by loadHistory once the transcript is drawn, so it lands at the bottom where it is seen.
  if (!fork && Date.now() - info.modifiedMs < RECENTLY_ACTIVE_MS) {
    cell.resumeWarning = `This session changed ${timeAgo(info.modifiedMs)}. If it is still open in VS Code or a terminal, close it there before you send a message here.`;
  }
  revealCell(cell);
}

let deepTimer = 0;
resumeSearch.addEventListener('input', () => {
  if (!resumeState.deep) { renderResumeList(); return; }
  clearTimeout(deepTimer);
  deepTimer = setTimeout(runDeepSearch, 300);
});
$('#resume-deep').addEventListener('click', e => {
  resumeState.deep = !resumeState.deep;
  e.currentTarget.classList.toggle('on', resumeState.deep);
  resumeSearch.placeholder = resumeState.deep ? 'Search inside conversations' : 'Search by title, prompt or folder';
  resumeSearch.focus();
  if (resumeState.deep) runDeepSearch();
  else { resumeState.deepSeq++; renderResumeList(); }
});
$('#resume-filters').addEventListener('click', e => {
  const btn = e.target.closest('button');
  if (!btn || btn.id === 'resume-deep') return;
  resumeState.source = btn.dataset.source;
  for (const b of $('#resume-filters').querySelectorAll('button')) b.classList.toggle('on', b === btn);
  renderResumeList();
});
/** Shows only the sessions from one folder ('' for every folder). */
function setResumeProject(path) {
  // The list keeps one spelling per folder, so a path written another way picks that option.
  const listed = [...resumeProject.options].find(o => o.value && projectKey(o.value) === projectKey(path));
  if (listed) path = listed.value;
  resumeState.project = path;
  if (path && resumeProject.value !== path) renderProjectOptions(); // adds the folder if the list lacks it
  resumeProject.value = path;
  resumeProject.classList.toggle('on', !!path);
  if (resumeState.deep) { runDeepSearch(); return; }
  if (!path) resumeState.sessions = resumeState.all;
  renderResumeList(); // the sessions already loaded for that folder show at once
  loadResumeSessions();
}

resumeProject.addEventListener('change', () => setResumeProject(resumeProject.value));
resumer.addEventListener('keydown', e => {
  const items = [...resumeList.querySelectorAll('.resume-item:not([disabled]), .hit:not([disabled])')];
  if (e.key === 'Escape') { e.stopPropagation(); closeResumer(); return; }
  if (e.target === resumeProject) return; // the arrow keys pick a folder there
  if (e.key === 'Enter' && document.activeElement === resumeSearch) {
    e.preventDefault();
    if (items[0]) items[0].click();
  } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    const cur = items.indexOf(document.activeElement);
    if (e.key === 'ArrowUp' && cur <= 0) { resumeSearch.focus(); return; }
    const next = items[e.key === 'ArrowDown' ? Math.min(cur + 1, items.length - 1) : cur - 1];
    if (next) next.focus();
  }
});
document.addEventListener('mousedown', e => {
  if (!resumer.classList.contains('hidden') && !resumer.contains(e.target) && !e.target.closest('#resume-session')) closeResumer();
});
$('#resume-session').addEventListener('click', () => (resumer.classList.contains('hidden') ? openResumer() : closeResumer()));

// ======================================================================================
// Plan usage (5-hour and weekly limits)
// ======================================================================================

const usagePill = $('#usage');
const usagePop = $('#usage-pop');
const usage = { data: null, fetchedAt: 0, loading: null, error: null, timer: null };
const USAGE_EVERY_MS = 5 * 60 * 1000;       // while a session is running, asking it is free
const USAGE_PROBE_EVERY_MS = 15 * 60 * 1000; // otherwise a short-lived claude is started to ask
const USAGE_STALE_MS = 2 * 60 * 1000;

/** Green at 0%, yellow at 60%, red at 100%. */
const HEAT_STOPS = [[0, [76, 255, 46]], [60, [245, 239, 71]], [100, [240, 14, 14]]];

function heat(percent) {
  const p = Math.max(0, Math.min(100, percent));
  let i = 0;
  while (i < HEAT_STOPS.length - 2 && p > HEAT_STOPS[i + 1][0]) i++;
  const [p0, c0] = HEAT_STOPS[i];
  const [p1, c1] = HEAT_STOPS[i + 1];
  const t = (p - p0) / (p1 - p0);
  return c0.map((v, k) => Math.round(v + (c1[k] - v) * t));
}

function heatCss(percent) {
  return `rgb(${heat(percent).join(',')})`;
}

/** Dark text on the bright end of the scale, white on the red end. */
function heatText(percent) {
  const [r, g, b] = heat(percent);
  return 0.2126 * r + 0.7152 * g + 0.0722 * b > 140 ? '#1b1b1b' : '#ffffff';
}

const LIMIT_LABELS = {
  five_hour: '5-hour session',
  seven_day: 'Weekly · all models',
  seven_day_opus: 'Weekly · Opus',
  seven_day_sonnet: 'Weekly · Sonnet',
};

/** Keeps only the limits HQ shows. Unknown or missing fields are skipped, never guessed. */
function sanitizeUsage(r) {
  const rows = [];
  const rl = (r && r.rate_limits) || {};
  for (const [key, label] of Object.entries(LIMIT_LABELS)) {
    const l = rl[key];
    if (l && typeof l.utilization === 'number') rows.push({ key, label, percent: Math.round(l.utilization), resetsAt: Date.parse(l.resets_at) || null });
  }
  // Newer CLIs also list per-model weekly limits.
  for (const l of Array.isArray(r && r.limits) ? r.limits : []) {
    if (l.kind !== 'weekly_scoped' || typeof l.percent !== 'number') continue;
    const name = l.scope && l.scope.model && l.scope.model.display_name;
    if (!name || rows.some(x => x.label === `Weekly · ${name}`)) continue;
    rows.push({ key: `scoped-${name}`, label: `Weekly · ${name}`, percent: Math.round(l.percent), resetsAt: Date.parse(l.resets_at) || null });
  }
  return { plan: typeof (r && r.subscription_type) === 'string' ? r.subscription_type : null, available: !!(r && r.rate_limits_available), rows };
}

function withTimeout(promise, ms) {
  return Promise.race([promise, new Promise(resolve => setTimeout(() => resolve(null), ms))]);
}

/** Asks a running session for usage when there is one, otherwise starts a short-lived probe. */
function refreshUsage(preferred) {
  if (usage.loading) return usage.loading;
  renderUsage();
  usage.loading = (async () => {
    let reply = null;
    const cell = [preferred, ...app.cells].find(c => c && c.backend === 'claude' && c.proc != null && !c.closing);
    if (cell) {
      const r = await withTimeout(cell.control({ subtype: 'get_usage' }), 15000);
      if (r && r.subtype === 'success') reply = r.response;
    }
    if (!reply) {
      await withTimeout(updateClaude(), CLAUDE_UPDATE_WAIT_MS); // the probe must not start claude mid-update
      reply = JSON.parse(await invoke('probe_usage'));
    }
    usage.data = sanitizeUsage(reply);
    usage.error = null;
    usage.fetchedAt = Date.now();
  })().catch(err => {
    usage.error = String(err);
  }).finally(() => {
    usage.loading = null;
    renderUsage();
    scheduleNextUsage();
  });
  renderUsage();
  return usage.loading;
}

/** A finished turn changes usage; asks the session that just finished, a few seconds later. */
function scheduleUsageRefresh(cell) {
  clearTimeout(usage.soon);
  usage.soon = setTimeout(() => refreshUsage(cell), 3000);
}

function scheduleNextUsage() {
  clearTimeout(usage.timer);
  const anyRunning = app.cells.some(c => c.proc != null);
  usage.timer = setTimeout(() => {
    // Starting a probe for a window nobody is looking at is waste; catch up when it is shown.
    if (document.hidden && !app.cells.some(c => c.proc != null)) return;
    refreshUsage();
  }, anyRunning ? USAGE_EVERY_MS : USAGE_PROBE_EVERY_MS);
}

document.addEventListener('visibilitychange', () => {
  document.documentElement.classList.toggle('doc-hidden', document.hidden); // pauses the waiting view's animation
  for (const cell of app.cells) cell.syncProgressTimer();
  if (!document.hidden && Date.now() - usage.fetchedAt > USAGE_STALE_MS) refreshUsage();
});

function untilText(ms) {
  const mins = Math.max(0, Math.round((ms - Date.now()) / 60000));
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ${mins % 60}m`;
  return `${Math.floor(hours / 24)}d ${hours % 24}h`;
}

function resetText(ms) {
  const d = new Date(Math.round(ms / 60000) * 60000); // resets land a few ms either side of the minute
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  const sameDay = d.toDateString() === new Date().toDateString();
  return `Resets in ${untilText(ms)} · ${sameDay ? time : `${d.toLocaleDateString([], { weekday: 'short' })} ${time}`}`;
}

function renderUsage() {
  const five = usage.data && usage.data.rows.find(r => r.key === 'five_hour');
  const text = usagePill.querySelector('.usage-text');
  usagePill.classList.toggle('empty', !five);
  usagePill.classList.toggle('loading', !!usage.loading);
  if (five) {
    // A window that has already reset shows 0% until the next refresh brings the new numbers.
    const expired = five.resetsAt && five.resetsAt < Date.now();
    const percent = expired ? 0 : five.percent;
    text.textContent = expired || !five.resetsAt ? `${percent}%` : `${percent}% · ${untilText(five.resetsAt)}`;
    usagePill.style.background = heatCss(percent);
    usagePill.style.color = heatText(percent);
  } else {
    text.textContent = usage.error ? 'Usage unavailable' : '…';
    usagePill.style.background = '';
    usagePill.style.color = '';
  }
  // The weekly limit as a thin bar along the bottom, on the pill's colour scale.
  const week = usage.data && usage.data.rows.find(r => r.key === 'seven_day');
  const weekBar = usagePill.querySelector('.usage-week');
  const showWeek = !!(five && week && app.settings.weeklyBar);
  weekBar.classList.toggle('hidden', !showWeek);
  if (showWeek) {
    const percent = week.resetsAt && week.resetsAt < Date.now() ? 0 : week.percent;
    weekBar.firstElementChild.style.width = `${Math.min(100, percent)}%`;
    weekBar.firstElementChild.style.backgroundColor = heatCss(percent);
    usagePill.title = `Plan usage · weekly ${percent}%`;
  } else {
    usagePill.title = 'Plan usage';
  }
  if (!usagePop.classList.contains('hidden')) renderUsagePop();
}

function renderUsagePop() {
  const parts = [h('div', { class: 'usage-head' },
    h('span', { text: 'Plan usage' }),
    usage.data && usage.data.plan ? h('span', { class: 'usage-plan', text: usage.data.plan }) : null)];
  if (usage.data && !usage.data.rows.length) {
    parts.push(h('div', { class: 'usage-note', text: usage.data.available ? 'No limits reported.' : 'Usage limits are not available for this account.' }));
  }
  for (const row of (usage.data && usage.data.rows) || []) {
    const expired = row.resetsAt && row.resetsAt < Date.now();
    const percent = expired ? 0 : row.percent;
    parts.push(h('div', { class: 'usage-row' },
      h('div', { class: 'usage-line' }, h('span', { text: row.label }), h('span', { class: 'usage-pct', text: `${percent}%` })),
      h('div', { class: 'usage-bar' }, h('span', { style: `width:${Math.min(100, percent)}%;background:${heatCss(percent)}` })),
      row.resetsAt ? h('div', { class: 'usage-reset', text: expired ? 'Reset; refreshing…' : resetText(row.resetsAt) }) : null));
  }
  if (usage.error) parts.push(h('div', { class: 'usage-note error', text: usage.error }));
  const updated = usage.loading ? 'Updating…' : usage.fetchedAt ? `Updated ${timeAgo(usage.fetchedAt)}` : '';
  parts.push(h('div', { class: 'usage-foot', text: `${updated}${updated ? ' · ' : ''}click to refresh` }));
  usagePop.replaceChildren(...parts);
  const r = usagePill.getBoundingClientRect();
  usagePop.style.top = `${r.bottom + 6}px`;
  usagePop.style.left = `${Math.max(8, Math.min(r.right - usagePop.offsetWidth, window.innerWidth - usagePop.offsetWidth - 8))}px`;
}

let usageHideTimer = null;
function showUsagePop() {
  clearTimeout(usageHideTimer);
  usagePop.classList.remove('hidden');
  renderUsagePop();
  if (Date.now() - usage.fetchedAt > USAGE_STALE_MS) refreshUsage();
}
function hideUsagePop() {
  clearTimeout(usageHideTimer);
  usageHideTimer = setTimeout(() => usagePop.classList.add('hidden'), 200);
}
usagePill.addEventListener('mouseenter', showUsagePop);
usagePill.addEventListener('mouseleave', hideUsagePop);
usagePill.addEventListener('focus', showUsagePop);
usagePill.addEventListener('blur', hideUsagePop);
usagePop.addEventListener('mouseenter', () => clearTimeout(usageHideTimer));
usagePop.addEventListener('mouseleave', hideUsagePop);
usagePill.addEventListener('click', () => refreshUsage());
// Keeps the countdown current, and fetches fresh numbers once the 5-hour window resets.
setInterval(() => {
  const five = usage.data && usage.data.rows.find(r => r.key === 'five_hour');
  if (five && five.resetsAt && five.resetsAt < Date.now() && Date.now() - usage.fetchedAt > 60000) refreshUsage();
  else renderUsage();
}, 30000);

// ======================================================================================
// Settings: what used to be environment variables, plus the theme and notifications
// ======================================================================================

const settingsPanel = $('#settings');

// Built-in themes. Auto follows Windows between Dark and Light; the others set every colour.
const THEMES = [['auto', 'Auto'], ['dark', 'Dark'], ['light', 'Light'], ['midnight', 'Midnight'], ['nord', 'Nord'],
  ['dracula', 'Dracula'], ['solarized', 'Solarized dark'], ['forest', 'Forest'], ['paper', 'Paper (light)'], ['rose', 'Rosé (light)']];
// The colours that can be changed on top of a theme: [CSS variable, label].
const CUSTOM_COLORS = [['--accent', 'Accent'], ['--link', 'Links'], ['--claude', 'Claude (busy line)'], ['--md-strong', 'Bold'],
  ['--md-em', 'Italic'], ['--md-code', 'Inline code'], ['--md-h1', 'Heading 1'], ['--md-h2', 'Headings 2+'], ['--md-marker', 'List markers'],
  ['--hl-keyword', 'Code: keywords'], ['--hl-string', 'Code: strings'], ['--hl-function', 'Code: functions'], ['--hl-comment', 'Code: comments']];
// The cell header's buttons that can be hidden: [class, label].
const HEAD_BUTTONS = [['act-handoff', 'Handoff'], ['act-detail', 'Detail level'], ['act-progress', 'Progress'], ['act-waiting', 'Waiting mode'], ['act-code', 'VS Code'],
  ['act-terminal', 'Terminal'], ['act-reveal', FILE_MANAGER], ['act-collapse', 'Collapse'], ['act-max', 'Maximize'], ['act-close', 'Close']];
const HEX = /^#[0-9a-f]{6}$/i;
const lightQuery = window.matchMedia('(prefers-color-scheme: light)');

/** Keeps known keys with the right types, so a hand-edited board.json cannot break the app. */
function sanitizeSettings(x) {
  const v = x && typeof x === 'object' ? x : {};
  const colors = {};
  for (const [theme, set] of Object.entries(v.colors && typeof v.colors === 'object' ? v.colors : {})) {
    if (!THEMES.some(([t]) => t === theme) || !set || typeof set !== 'object') continue;
    const kept = Object.fromEntries(CUSTOM_COLORS.map(([name]) => [name, set[name]]).filter(([, c]) => typeof c === 'string' && HEX.test(c)));
    if (Object.keys(kept).length) colors[theme] = kept;
  }
  return {
    theme: THEMES.some(([t]) => t === v.theme) ? v.theme : 'auto',
    notify: v.notify !== false,
    autoUpdate: v.autoUpdate !== false,
    autoTasks: v.autoTasks !== false, // read when a session starts: Claude or Hermes is asked to keep a task list
    claudePath: typeof v.claudePath === 'string' ? v.claudePath.trim().slice(0, 500) : '',
    autoCopy: v.autoCopy !== false,
    weeklyBar: v.weeklyBar !== false,
    // How the header buttons show: always, fading in as the pointer nears, or only while the header is hovered.
    buttonReveal: ['always', 'near', 'hover'].includes(v.buttonReveal) ? v.buttonReveal : v.fadeButtons === true ? 'near' : 'always',
    hiddenButtons: Array.isArray(v.hiddenButtons) ? HEAD_BUTTONS.map(([b]) => b).filter(b => v.hiddenButtons.includes(b)) : [],
    colors,
  };
}
app.settings = sanitizeSettings(null);

/** The theme in force: Auto resolves to Dark or Light. Custom colours are kept per resolved theme. */
function activeTheme() {
  const { theme } = app.settings;
  return theme === 'auto' ? (lightQuery.matches ? 'light' : 'dark') : theme;
}

function applySettings() {
  applyAppearance();
  invoke('set_claude_path', { path: app.settings.claudePath || null }).catch(() => {}); // the backend uses it for every claude it starts
}

/** Theme, custom colours, hidden header buttons and the fade. Cheap, so a colour picker can call it while it is dragged. */
function applyAppearance() {
  const root = document.documentElement;
  const { theme, colors, hiddenButtons, buttonReveal } = app.settings;
  if (theme === 'auto') delete root.dataset.theme;
  else root.dataset.theme = theme;
  const custom = colors[activeTheme()] || {};
  for (const [name] of CUSTOM_COLORS) {
    if (custom[name]) root.style.setProperty(name, custom[name]);
    else root.style.removeProperty(name);
  }
  // A custom accent has no hover shade of its own; the same colour beats the theme's unrelated one.
  if (custom['--accent']) root.style.setProperty('--accent-hover', custom['--accent']);
  else root.style.removeProperty('--accent-hover');
  let style = document.getElementById('appearance-style');
  if (!style) style = document.head.appendChild(h('style', { id: 'appearance-style' }));
  style.textContent = hiddenButtons.map(b => `.cell-head > .${b} { display: none; }`).join('\n');
  document.body.classList.toggle('fade-buttons', buttonReveal === 'near');
  document.body.classList.toggle('hover-buttons', buttonReveal === 'hover');
  if (buttonReveal !== 'near') clearButtonFade();
  renderUsage();
}
lightQuery.addEventListener('change', () => { if (app.settings.theme === 'auto') { applyAppearance(); if (!settingsPanel.classList.contains('hidden')) renderSettings(); } });

function changeSetting(key, value) {
  app.settings = { ...app.settings, [key]: value };
  if (key === 'claudePath') applySettings();
  else applyAppearance();
  saveState();
}

/** One custom colour for the theme in force; null puts the theme's own back. */
function changeColor(name, value) {
  const theme = activeTheme();
  const set = { ...(app.settings.colors[theme] || {}) };
  if (value) set[name] = value;
  else delete set[name];
  const colors = { ...app.settings.colors, [theme]: set };
  if (!Object.keys(set).length) delete colors[theme];
  changeSetting('colors', colors);
}

/** The value a colour has now (custom or the theme's), as #rrggbb for a colour input. */
function currentColor(name) {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  if (HEX.test(v)) return v.toLowerCase();
  const short = /^#([0-9a-f])([0-9a-f])([0-9a-f])$/i.exec(v);
  return short ? `#${short[1]}${short[1]}${short[2]}${short[2]}${short[3]}${short[3]}`.toLowerCase() : '#000000';
}

const PREVIEW_MARKDOWN = '## Heading\nSome **bold** text, some *italic*, `inline code` and a [link](https://claude.com).\n\n'
  + '- a list item\n- another\n\n```js\n// a comment\nfunction greet(name) {\n  return `Hello, ${name}`;\n}\n```';

let settingsTab = 'general';

function renderSettings() {
  const row = (name, desc, control) => h('div', { class: 'setting' }, h('div', { class: 'setting-main' }, h('span', { class: 'name', text: name }), desc ? h('span', { class: 'desc', text: desc }) : null), control);
  const toggle = (on, onChange) => {
    const sw = h('button', { class: `switch${on ? ' on' : ''}`, role: 'switch', 'aria-checked': String(on) });
    sw.addEventListener('click', () => { onChange(!on); renderSettings(); });
    return sw;
  };
  const select = (value, options, onChange) => {
    const el = h('select', null, options.map(([v, label]) => h('option', { value: v, text: label })));
    el.value = value;
    el.addEventListener('change', () => onChange(el.value));
    return el;
  };
  const tabs = [['general', 'General'], ['appearance', 'Appearance']];
  settingsPanel.querySelector('#settings-tabs').replaceChildren(...tabs.map(([key, label]) => h('button', {
    class: `settings-tab${settingsTab === key ? ' on' : ''}`, role: 'tab', 'aria-selected': String(settingsTab === key), text: label,
    onclick: () => { settingsTab = key; renderSettings(); },
  })));
  const body = settingsPanel.querySelector('#settings-body');
  if (settingsTab === 'appearance') {
    body.replaceChildren(...renderAppearanceSettings(row, toggle, select));
    return;
  }
  const path = h('input', { type: 'text', value: app.settings.claudePath, placeholder: 'claude', spellcheck: 'false' });
  path.addEventListener('change', () => changeSetting('claudePath', path.value.trim()));
  const models = modelList().map(m => [m.value, m.displayName || m.value]);
  body.replaceChildren(
    row('Notifications', 'A notification when a turn finishes, or Claude needs you, while HQ is in the background.', toggle(app.settings.notify, v => changeSetting('notify', v))),
    row('Remote Control for new sessions', 'Continue sessions from claude.ai/code or the Claude app.', toggle(!!app.remoteDefault, v => { app.remoteDefault = v; saveState(); renderRemoteButtons(); })),
    row('Default model for new sessions', 'The same as picking a model in a session. HQ_MODEL still applies when none is picked.',
      select(app.defaults.model || 'default', models, v => rememberChoice({ model: v }))),
    row('Automatic task lists', 'Ask Claude and Hermes to keep a task list on multi-step work. Off saves a few tokens per long job.', toggle(app.settings.autoTasks, v => changeSetting('autoTasks', v))),
    row('Check for HQ updates automatically', 'The Check for updates button in the sessions list always works.', toggle(app.settings.autoUpdate, v => changeSetting('autoUpdate', v))),
    row('Claude command', 'Path to the claude program, for sessions started from now on. Empty uses HQ_CLAUDE, else claude on PATH.', path));
}

function renderAppearanceSettings(row, toggle, select) {
  const theme = activeTheme();
  const custom = app.settings.colors[theme] || {};
  const preview = h('div', { class: 'md theme-preview' });
  renderMarkdown(preview, PREVIEW_MARKDOWN);
  // Colour inputs change the page as they are dragged and are never redrawn by it, so the picker stays open.
  const colorRow = ([name, label]) => {
    const input = h('input', { type: 'color', value: currentColor(name), title: label });
    input.addEventListener('input', () => changeColor(name, input.value.toLowerCase()));
    const reset = h('button', { class: 'icon-btn', title: 'Back to the theme\'s colour', disabled: !custom[name] }, icon('discard'));
    reset.addEventListener('click', () => { changeColor(name, null); renderSettings(); });
    input.addEventListener('change', () => { reset.disabled = false; });
    return h('label', { class: `color-row${custom[name] ? ' custom' : ''}` }, input, h('span', { class: 'name', text: label }), reset);
  };
  const resetAll = h('button', { class: 'link-btn', text: 'Reset all', disabled: !Object.keys(custom).length });
  resetAll.addEventListener('click', () => { changeSetting('colors', Object.fromEntries(Object.entries(app.settings.colors).filter(([t]) => t !== theme))); renderSettings(); });
  const buttons = h('div', { class: 'button-checks' }, HEAD_BUTTONS.map(([cls, label]) => {
    const box = h('input', { type: 'checkbox' });
    box.checked = !app.settings.hiddenButtons.includes(cls);
    box.addEventListener('change', () => changeSetting('hiddenButtons',
      box.checked ? app.settings.hiddenButtons.filter(b => b !== cls) : [...app.settings.hiddenButtons, cls]));
    return h('label', null, box, label);
  }));
  const themeName = (THEMES.find(([t]) => t === theme) || [])[1] || theme;
  return [
    h('div', { class: 'settings-section', text: 'Theme' }),
    row('Theme', `Auto follows ${IS_MAC ? 'macOS' : 'Windows'}.`, select(app.settings.theme, THEMES, v => { changeSetting('theme', v); renderSettings(); })),
    h('div', { class: 'setting-colors' },
      h('div', { class: 'setting-colors-head' }, h('span', { text: `Colours for ${themeName}` }), resetAll),
      h('div', { class: 'color-grid' }, CUSTOM_COLORS.map(colorRow)),
      preview),
    h('div', { class: 'settings-section', text: 'Top bar' }),
    row('Weekly usage bar', 'A thin bar along the bottom of the usage button showing the weekly limit.', toggle(app.settings.weeklyBar, v => changeSetting('weeklyBar', v))),
    h('div', { class: 'settings-section', text: 'Sessions' }),
    row('Copy on select', 'Selecting text in a chat copies it, as in a terminal.', toggle(app.settings.autoCopy, v => changeSetting('autoCopy', v))),
    row('Header buttons show', 'Fade in: faint until the pointer comes near. On hover: hidden until the pointer is over the header.',
      select(app.settings.buttonReveal, [['always', 'Always'], ['near', 'Fade in near the pointer'], ['hover', 'Only on hover']], v => changeSetting('buttonReveal', v))),
    h('div', { class: 'setting column' }, h('div', { class: 'setting-main' }, h('span', { class: 'name', text: 'Header buttons' }),
      h('span', { class: 'desc', text: 'Unticked buttons are hidden; their shortcuts still work.' })), buttons),
  ];
}

// ---------- header buttons that fade in as the pointer comes near ----------

const FADE_NEAR_PX = 40;   // full strength within this distance of a button
const FADE_FAR_PX = 160;   // faintest beyond this
const FADE_MIN = 0.12;
const faded = new Set();   // headers whose buttons have a --near value set
let fadeFrame = 0;
let fadePoint = null;

function clearButtonFade() {
  for (const head of faded) for (const b of head.querySelectorAll(':scope > .icon-btn')) b.style.removeProperty('--near');
  faded.clear();
}

document.addEventListener('pointermove', e => {
  if (app.settings.buttonReveal !== 'near') return;
  fadePoint = [e.clientX, e.clientY];
  if (fadeFrame) return;
  fadeFrame = requestAnimationFrame(() => {
    fadeFrame = 0;
    const [x, y] = fadePoint;
    const near = new Set();
    for (const head of document.querySelectorAll('.cell-head')) {
      const r = head.getBoundingClientRect();
      if (y < r.top - FADE_FAR_PX || y > r.bottom + FADE_FAR_PX || x < r.left - FADE_FAR_PX || x > r.right + FADE_FAR_PX) continue;
      near.add(head);
      for (const b of head.querySelectorAll(':scope > .icon-btn')) {
        const br = b.getBoundingClientRect();
        if (!br.width) continue;
        const d = Math.hypot(Math.max(br.left - x, 0, x - br.right), Math.max(br.top - y, 0, y - br.bottom));
        const t = Math.min(1, Math.max(0, (d - FADE_NEAR_PX) / (FADE_FAR_PX - FADE_NEAR_PX)));
        b.style.setProperty('--near', (1 - t * (1 - FADE_MIN)).toFixed(2));
      }
    }
    for (const head of faded) {
      if (near.has(head)) continue;
      for (const b of head.querySelectorAll(':scope > .icon-btn')) b.style.removeProperty('--near');
    }
    faded.clear();
    for (const head of near) faded.add(head);
  });
});
document.documentElement.addEventListener('mouseleave', clearButtonFade);

function openSettings() {
  closePicker();
  closeResumer();
  closeMenu();
  renderSettings();
  settingsPanel.classList.remove('hidden');
}

function closeSettings() {
  settingsPanel.classList.add('hidden');
}

$('#open-settings').addEventListener('click', () => (settingsPanel.classList.contains('hidden') ? openSettings() : closeSettings()));
$('#terminal-btn').addEventListener('click', () => invoke('open_terminal', {}).catch(err => console.error('terminal failed', err)));
$('#settings-btn').addEventListener('click', () => (settingsPanel.classList.contains('hidden') ? openSettings() : closeSettings()));
settingsPanel.addEventListener('keydown', e => {
  if (e.key === 'Escape') { e.stopPropagation(); closeSettings(); }
});
document.addEventListener('mousedown', e => {
  if (!settingsPanel.classList.contains('hidden') && !settingsPanel.contains(e.target) && !e.target.closest('#open-settings, #settings-btn')) closeSettings();
});

// ======================================================================================
// Self-update: download in the background, install when nothing would be interrupted
// ======================================================================================

const updatePill = $('#update');
const UPDATE_FIRST_CHECK_MS = 20 * 1000;
const UPDATE_EVERY_MS = 4 * 60 * 60 * 1000;
// No clicks, keys or scrolling for this long before HQ restarts itself; longer while HQ is the
// active window, since the user may be reading it.
const UPDATE_QUIET_MS = 2 * 60 * 1000;
const UPDATE_QUIET_FOCUSED_MS = 15 * 60 * 1000;
// `wanted`: the user asked to update while a restart would still lose something (see restartLosesNothing),
// so it installs as soon as it would not.
const updater = { ready: null, checking: null, installing: false, wanted: false, lastInput: Date.now() };

/** Resolves with the error text when the check fails, else null. */
function checkForUpdate() {
  if (updater.installing) return Promise.resolve(null);
  if (updater.checking) return updater.checking;
  updater.checking = (async () => {
    try {
      const info = await invoke('check_update');
      if (info) updater.ready = info;
      return null;
    } catch (err) {
      console.warn('update check failed', err); // offline, or no release yet: the next check tries again
      return String(err);
    } finally {
      updater.checking = null;
      renderUpdate();
    }
  })();
  return updater.checking;
}

/**
 * Installing closes HQ, which ends every claude process, so it waits until that loses nothing:
 * no turn running, nothing to answer, no draft, no reply the user has not seen yet, and no
 * input for a while. Cells come back from board.json and resume when next used.
 */
function safeToRestart() {
  if (Date.now() - updater.lastInput < (document.hasFocus() ? UPDATE_QUIET_FOCUSED_MS : UPDATE_QUIET_MS)) return false;
  if (!picker.classList.contains('hidden') || !resumer.classList.contains('hidden') || menuState.anchor) return false;
  return sessionsIdle() && app.cells.every(c => !c.unread);
}

/** No turn, prompt or background task running, and no unsent text or images that a restart would lose. */
function sessionsIdle() {
  return app.cells.every(c => !c.busy() && !c.spawning && !c.renaming && !c.input.value.trim() && !c.attachments.length);
}

/**
 * What an update the user asks for waits on. Sessions that are working are fine: they are marked
 * in the board saved before the restart and carry on by themselves after it (resumeAfterRestart).
 * Images not yet sent or read, a session still starting, a first turn and a rename in flight are not saved.
 */
function restartLosesNothing() {
  // A first turn with no session ID yet, or a fork not yet made, has no transcript of its own to resume from.
  return app.cells.every(c => !c.spawning && !c.renaming && !c.attachments.length && !(c.busy() && (!c.sessionId || c.fork))
    // Images the agent may not have read yet: a resume carries text only.
    && !(c.lastSent && c.lastSent.images.length) && !c.queuedTexts.some(q => q.images));
}

async function installUpdate() {
  if (!updater.ready || updater.installing) return;
  updater.installing = true;
  renderUpdate();
  // Decided now, while the cells still show their work: installing stops every session, after which none looks busy.
  for (const c of app.cells) {
    c.resumeMark = c.busy() && !c.missing && c.sessionId ? { queued: c.queuedTexts.map(q => q.text).filter(Boolean) } : null;
    // A prompt the agent has not answered may not be in its transcript yet; it goes with the resume, not back to the composer.
    const sent = c.lastSent;
    if (c.resumeMark && sent) {
      const text = sent.hq ? sent.restore : sent.text;
      if (text && !c.resumeMark.queued.includes(text)) c.resumeMark.queued.push(text);
      c.lastSent = null;
    }
  }
  await flushState();
  try {
    await invoke('install_update'); // on success HQ exits here and the installer restarts it
  } catch (err) {
    console.error('update failed', err);
    updater.installing = false;
    // Installing may have stopped the sessions before it failed; those carry on as after a restart.
    const marked = app.cells.filter(c => c.resumeMark);
    for (const c of marked) {
      c.resumeAfterUpdate = true;
      c.resumeQueued = c.resumeMark.queued;
    }
    for (const c of app.cells) c.resumeMark = null;
    flushState(); // drops the resume marks the board was saved with
    // After the stopped processes have reported their exit; a session still running is left alone.
    marked.forEach((c, i) => setTimeout(() => c.resumeAfterRestart(), 2000 + i * 1000));
    updater.ready = null; // the download is used up; the next check fetches it again
    renderUpdate();
  }
}

function renderUpdate() {
  const text = updatePill.querySelector('.update-text');
  updatePill.classList.toggle('hidden', !updater.ready);
  if (!updater.ready) return;
  const { version, currentVersion } = updater.ready;
  if (updater.installing) {
    text.textContent = 'Updating…';
    updatePill.title = `Installing HQ ${version}. HQ restarts when it is done.`;
  } else if (updater.wanted) {
    text.textContent = 'Updates in a moment';
    updatePill.title = `HQ ${version} installs once no image is waiting to be sent and no session is starting. Click to cancel.`;
  } else {
    text.textContent = `Update to ${version}`;
    updatePill.title = `HQ ${version} is ready (you have ${currentVersion}). It installs by itself when no session is busy, or when you close HQ. `
      + 'Click to install now: sessions that are working carry on by themselves after the restart.';
  }
}

// A click installs now, and working sessions carry on after the restart; the update installs by itself only when all are idle.
updatePill.addEventListener('click', () => {
  if (updater.installing) return;
  if (updater.wanted || !restartLosesNothing()) {
    updater.wanted = !updater.wanted;
    renderUpdate();
    return;
  }
  installUpdate();
});

const checkUpdateBtn = $('#check-update');
checkUpdateBtn.addEventListener('click', async () => {
  clearTimeout(checkUpdateBtn.resetTimer);
  if (!build.updatable) {
    checkUpdateBtn.textContent = 'This copy does not update';
  } else {
    checkUpdateBtn.disabled = true;
    checkUpdateBtn.textContent = 'Checking…';
    const err = await checkForUpdate();
    checkUpdateBtn.disabled = false;
    checkUpdateBtn.textContent = err ? 'Check failed' : updater.ready ? `${updater.ready.version} is ready` : 'Up to date';
    checkUpdateBtn.title = err || '';
  }
  checkUpdateBtn.resetTimer = setTimeout(() => { checkUpdateBtn.textContent = 'Check for updates'; checkUpdateBtn.title = ''; }, 6000);
});

for (const type of ['keydown', 'pointerdown', 'wheel']) {
  document.addEventListener(type, () => { updater.lastInput = Date.now(); }, { capture: true, passive: true });
}
function autoCheckForUpdate() {
  if (app.settings.autoUpdate) checkForUpdate();
}
setInterval(autoCheckForUpdate, UPDATE_EVERY_MS);
setInterval(() => {
  if (updater.ready && !updater.installing && (updater.wanted ? restartLosesNothing() : safeToRestart())) installUpdate();
}, 5 * 1000);

// ======================================================================================
// Keyboard
// ======================================================================================

document.addEventListener('keydown', e => {
  const cell = app.focused;
  const typing = e.target.matches?.('input, textarea, select'); // the target is document when nothing has focus
  // Dev builds read the UI from disk, so a reload shows edits. It also stops every claude.
  // Cmd on a Mac (Ctrl+B, N, O there are text-field cursor keys), Ctrl elsewhere.
  const mod = IS_MAC ? e.metaKey : e.ctrlKey;
  if (build.liveUi && (e.key === 'F5' || (mod && (e.key === 'r' || e.key === 'R')))) {
    e.preventDefault();
    flushState().then(() => location.reload());
    return;
  }
  if (e.key === 'Escape' && menuState.anchor) {
    e.preventDefault();
    closeMenu();
    return;
  }
  if (mod && e.shiftKey && !e.altKey && (e.key === 'm' || e.key === 'M') && cell) {
    e.preventDefault();
    collapseCell(cell, !cell.collapsed);
    return;
  }
  if (mod && !e.shiftKey && !e.altKey) {
    if (e.key === 'n' || e.key === 'N') { e.preventDefault(); openPicker(); return; }
    if (e.key === 'o' || e.key === 'O') { e.preventDefault(); openResumer(); return; }
    if (e.key === ',') { e.preventDefault(); openSettings(); return; }
    if (e.key === 'b' || e.key === 'B') { e.preventDefault(); setSidebar(!app.sidebarOpen, !app.sidebarOpen); return; }
    if ((e.key === 'm' || e.key === 'M') && cell) { e.preventDefault(); toggleMaximize(cell); return; }
    if ((e.key === 'w' || e.key === 'W') && cell) { e.preventDefault(); cell.requestClose(cell.el.querySelector('.act-close')); return; }
    if (/^[1-9]$/.test(e.key)) {
      const target = app.cells[Number(e.key) - 1];
      if (target) {
        e.preventDefault();
        if (target.collapsed) target.setCollapsed(false);
        if (app.maximized) { app.maximized = target; layout(); }
        focusCell(target, true);
      }
      return;
    }
  }
  // A collapsed chat is not on screen, so Esc and 1/2/3 must not stop it or answer its prompts.
  if (!cell || cell.collapsed) return;
  if (e.key === 'F2') {
    e.preventDefault();
    cell.startRename();
    return;
  }
  if (e.key === 'Escape' && picker.classList.contains('hidden') && resumer.classList.contains('hidden') && (cell.status === 'running' || cell.pending.size)) {
    e.preventDefault();
    cell.interrupt();
    return;
  }
  // 1/2/3 answer the top prompt when the focus is not in a text field.
  if (!typing && !e.ctrlKey && !e.metaKey && !e.altKey && /^[1-9]$/.test(e.key)) {
    const card = !cell.promptsMin && cell.prompts.firstElementChild; // minimized prompts take no hotkeys
    const run = card && card.hotkeys && card.hotkeys[Number(e.key) - 1];
    if (run) { e.preventDefault(); run(); }
  }
});

// One layout per frame while the window is being resized.
let layoutFrame = 0;
new ResizeObserver(() => {
  if (layoutFrame) return;
  layoutFrame = requestAnimationFrame(() => { layoutFrame = 0; layout(); });
}).observe(board);

// A file dropped outside a cell would otherwise make the webview navigate to it.
document.addEventListener('dragover', e => e.preventDefault());
document.addEventListener('drop', e => e.preventDefault());

// ======================================================================================
// Persistence
// ======================================================================================

let saveTimer = null;
function saveState() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(flushState, 300);
}

/** Saves now instead of after the debounce; returns once the board is on disk. */
let restoring = true; // the board is not saved until the cells are back
function flushState() {
  clearTimeout(saveTimer);
  if (restoring) return Promise.resolve();
  const state = {
    version: 2,
    cells: app.cells.map(c => ({
      cwd: c.cwd,
      sessionId: c.sessionId,
      permissionMode: c.permissionMode,
      model: c.modelChoice,
      effort: c.effort,
      title: c.title,
      aiTitle: c.aiTitle || undefined,
      customTitle: c.customTitle || undefined,
      renamePending: c.renamePending || undefined,
      detail: c.detail === 2 ? undefined : c.detail, // 2 (no tool calls) is the default
      progress: c.progressView || undefined,
      fork: c.fork || undefined,
      collapsed: c.collapsed || undefined,
      remote: c.remote === true ? true : undefined,
      bridgeSessionId: c.bridgeSessionId || undefined,
      draft: c.input.value.trim() ? c.input.value : undefined,
      backend: c.backend === 'claude' ? undefined : c.backend,
      // Only in the save just before an update restarts HQ; the first save after it drops the mark.
      resumeAfterUpdate: c.resumeMark ? true : undefined,
      resumeQueued: c.resumeMark && c.resumeMark.queued.length ? c.resumeMark.queued : undefined,
    })),
    recent: app.recent,
    models: app.models,
    commands: app.commands,
    defaults: app.defaults,
    hermes: hermesState(),
    workspaces: app.workspaces,
    projectColors: projectColorPicks,
    claudeUpdateAt: app.claudeUpdateAt || undefined,
    remoteDefault: app.remoteDefault == null ? undefined : app.remoteDefault,
    settings: app.settings,
    sidebarOpen: app.sidebarOpen,
  };
  return invoke('save_state', { json: JSON.stringify(state) }).catch(err => console.error('save failed', err));
}

async function restore() {
  await invoke('kill_all_sessions').catch(() => {});
  build = await invoke('build_info').catch(() => build);
  $('#hq-version').textContent = build.version ? `HQ ${build.version}` : 'HQ';
  if (build.profile) {
    const tag = $('#profile');
    tag.textContent = build.profile;
    tag.title = `This copy keeps its own sessions, separate from the installed HQ (profile "${build.profile}", version ${build.version}).`;
    tag.classList.remove('hidden');
  }
  let state = null;
  try {
    const raw = await invoke('load_state');
    state = raw ? JSON.parse(raw) : null;
  } catch (err) {
    console.error('load failed', err);
  }
  app.recent = Array.isArray(state && state.recent) ? state.recent : [];
  app.models = sanitizeModels(state && state.models);
  app.commands = sanitizeCommands(state && state.commands);
  app.claudeUpdateAt = state && Number.isFinite(state.claudeUpdateAt) ? state.claudeUpdateAt : 0;
  app.remoteDefault = state && typeof state.remoteDefault === 'boolean' ? state.remoteDefault : null;
  app.settings = sanitizeSettings(state && state.settings);
  applySettings();
  // (updateClaude runs at the end: its first save must not write a board that has no cells yet)
  app.defaults = sanitizeDefaults(state && (state.defaults || (state.cells || []).at(-1)));
  hermesRestore(state && state.hermes);
  app.workspaces = sanitizeWorkspaces(state && state.workspaces);
  projectColorPicks = sanitizeProjectColors(state && state.projectColors);
  app.sidebarOpen = !!(state && state.sidebarOpen);
  sidebar.classList.toggle('hidden', !app.sidebarOpen);
  $('#toggle-sidebar').classList.toggle('on', app.sidebarOpen);
  // Cells whose folder is gone stay on the board, marked, so a rename or an unplugged drive loses nothing.
  const saved = ((state && state.cells) || []).filter(c => c && c.cwd);
  const exists = await Promise.all(saved.map(c => invoke('folder_exists', { path: c.cwd }).catch(() => false)));
  saved.forEach((c, i) => {
    // Version 1 saved the model Claude reported, not one the user picked; drop it so the cell follows the default.
    if ((state.version || 1) < 2) c.model = null;
    addCell({ ...c, missing: !exists[i], remote: c.remote === true }, { restore: true, batch: true });
  });
  // Each restored cell took focus in turn; a collapsed one must not keep it.
  if (app.focused && app.focused.collapsed) {
    const open = app.cells.find(c => !c.collapsed);
    if (open) focusCell(open, true);
  }
  layout();
  updateSummary();
  renderSidebar();
  for (const c of app.cells) if (!c.collapsed) autoGrow(c.input); // restored drafts, now that the cells are laid out
  restoring = false;
  saveState(); // anything saved while the board was coming back (and drops the resume marks)
  // A second apart, so a board of working sessions does not start every agent at once.
  app.cells.filter(c => c.resumeAfterUpdate).forEach((c, i) => setTimeout(() => c.resumeAfterRestart(), i * 1000));
  updateClaude(); // after the cells are back: it saves the time of the run
  if (app.focused) focusCell(app.focused, true);
  setTimeout(() => refreshUsage(), 1500);
  setTimeout(autoCheckForUpdate, UPDATE_FIRST_CHECK_MS);
  openLaunches();
}

/**
 * Opens the sessions that `hq.exe --new-session <folder> [--model m] [--effort e]
 * [--permission-mode p] [--prompt text]` asked for, and sends each one's prompt.
 */
async function openLaunches() {
  if (restoring) return; // restore() opens them once the board is back
  const requests = await invoke('take_launches').catch(() => []);
  for (const r of requests) await openLaunch(r).catch(err => console.error('launch failed', r.cwd, err));
}

const LAUNCH_MODES = ['default', 'acceptEdits', 'plan', 'auto'];

async function openLaunch(r) {
  const missing = !(await invoke('folder_exists', { path: r.cwd }).catch(() => false));
  const opts = { cwd: r.cwd, missing };
  if (r.model) opts.model = r.model;
  if (r.effort) opts.effort = r.effort;
  // Never bypassPermissions from a command line: whoever runs hq.exe must not skip the prompts.
  if (LAUNCH_MODES.includes(r.permissionMode)) opts.permissionMode = r.permissionMode;
  const cell = addNewCell(opts);
  if (r.prompt) {
    cell.input.value = r.prompt;
    autoGrow(cell.input);
    if (!missing) cell.submit(); // a missing folder keeps the prompt as a draft
  }
}

window.__TAURI__.event.listen('launch', openLaunches).catch(err => console.error('launch listener failed', err));

restore();
