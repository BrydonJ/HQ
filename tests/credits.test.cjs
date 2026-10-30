const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8').replace(/\r\n/g, '\n');
const between = (start, end) => source.slice(source.indexOf(start), source.indexOf(end, source.indexOf(start)));
const code = [
  between('const LIMIT_LABELS = {', '\n/**\n * Names for the dollar credits'),
  between('const CREDIT_LABELS = {', '\n/** Keeps only the limits HQ shows.'),
  between('async function checkClaudeSetup(', '\nfunction showSetup('),
  `class KeyCell {\n${between("  /** Whether the session is on HQ's API key, or switched to it", "  /** The session runs on HQ's API key only while")}\n}\nglobalThis.KeyCell = KeyCell;`,
].join('\n');

function harness({ settings = {}, apiKeyHint = null, status = null } = {}) {
  const shown = [];
  let opened = 0;
  const context = vm.createContext({
    Date,
    app: { settings, apiKeyHint },
    setup: { step: null, busy: false, skipped: false },
    invoke: async () => status,
    showSetup: step => shown.push(step),
    closeSetup: () => shown.push('closed'),
    openSettings: () => { opened++; },
    saveState() {},
    console,
  });
  vm.runInContext(code, context);
  return { shown, run: expr => vm.runInContext(expr, context), settingsOpened: () => opened };
}

const reply = {
  rate_limits: {
    five_hour: { utilization: 28, resets_at: '2026-10-29T13:30:00Z', limit_dollars: null },
    iguana_necktie: { utilization: 74.7, resets_at: '2026-11-05T07:59:00Z', limit_dollars: 250, used_dollars: 186.76, remaining_dollars: 63.24 },
    wattle_ember: { label: 'Promo credit', limit_dollars: 20, used_dollars: 5 },
    cinder_cove: { limit_dollars: 50, used_dollars: 50, remaining_dollars: 0 },
    extra_usage: { is_enabled: false, monthly_limit: 0 },
  },
  spend: { balance: null },
};

test('credits with something left are listed, with known names', () => {
  const { run } = harness();
  const credits = JSON.parse(run(`JSON.stringify(sanitizeCredits(${JSON.stringify(reply)}))`));
  assert.deepEqual(credits.map(c => [c.label, c.left, c.limit]), [['Cloud credits', 63.24, 250], ['Promo credit', 15, 20]]);
  assert.equal(credits[0].resetsAt, Date.parse('2026-11-05T07:59:00Z'));
});

test('a prepaid balance above zero is listed; zero or missing is not', () => {
  const { run } = harness();
  assert.deepEqual(JSON.parse(run(`JSON.stringify(sanitizeCredits({ spend: { balance: { amount_minor: 1234, exponent: 2 } } }))`)).map(c => c.left), [12.34]);
  assert.deepEqual(JSON.parse(run(`JSON.stringify(sanitizeCredits({ spend: { balance: { amount_minor: 0, exponent: 2 } } }))`)), []);
  assert.equal(run("sanitizeCredits(null).length"), 0);
});

test('a signed-out Claude Code is asked to sign in, even with an API key saved', async () => {
  const signedOut = harness({ apiKeyHint: 'sk-ant-…-123', status: { installed: true, loggedIn: false } });
  await signedOut.run('checkClaudeSetup()');
  assert.deepEqual(signedOut.shown, ['login']);
});

test('only a session marked before its first message runs on the API key', () => {
  const { run, settingsOpened } = harness({ apiKeyHint: 'sk-ant-…-123' });
  run(`globalThis.make = o => Object.assign(new KeyCell(), { backend: 'claude', apiMode: false, procOnApi: false, apiKeyed: false,
    sessionId: null, promptWritten: false, fork: false, modelChoice: 'claude-opus-5-5', notes: [],
    notice(t) { this.notes.push(t); }, renderApi() {} }, o);
    globalThis.marked = make(); globalThis.plain = make(); marked.toggleApiKeyed();`);
  assert.equal(run('marked.onApiNext()'), true);
  assert.equal(run('marked.apiSwitchDue()'), true);
  assert.equal(run('plain.onApiNext()'), false);
  assert.equal(run('plain.apiSwitchDue()'), false);
  // Once the first message is out the choice is closed: the button does nothing.
  run('marked.promptWritten = true; marked.toggleApiKeyed(); plain.sessionId = "abc"; plain.toggleApiKeyed();');
  assert.equal(run('marked.apiKeyed && !marked.keyChoiceOpen()'), true);
  assert.equal(run('plain.apiKeyed'), false);
  assert.equal(run('make({ backend: "codex" }).keyChoiceOpen()'), false);
  assert.equal(run('make({ fork: true }).keyChoiceOpen()'), false);
  assert.equal(settingsOpened(), 0);
});

test('with no key saved the key button opens Settings and marks nothing', () => {
  const { run, settingsOpened } = harness({ apiKeyHint: null });
  run(`globalThis.cell = Object.assign(new KeyCell(), { backend: 'claude', apiMode: false, procOnApi: false, apiKeyed: false,
    sessionId: null, promptWritten: false, fork: false, notice() {}, renderApi() {} }); cell.toggleApiKeyed();`);
  assert.equal(run('cell.apiKeyed'), false);
  assert.equal(settingsOpened(), 1);
  // A session marked earlier whose key was removed goes back to the plan.
  assert.equal(run('cell.apiKeyed = true; cell.onApiNext()'), false);
});
