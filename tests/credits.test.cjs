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
  between('function apiAlways()', '\n/** Turns "always on the API key"'),
].join('\n');

function harness({ settings = {}, apiKeyHint = null, status = null } = {}) {
  const shown = [];
  const context = vm.createContext({
    Date,
    app: { settings, apiKeyHint },
    setup: { step: null, busy: false, skipped: false },
    invoke: async () => status,
    showSetup: step => shown.push(step),
    closeSetup: () => shown.push('closed'),
    console,
  });
  vm.runInContext(code, context);
  return { shown, run: expr => vm.runInContext(expr, context) };
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

test('with every session on the API key, a signed-out Claude Code is not asked to sign in', async () => {
  const signedOut = { installed: true, loggedIn: false };
  const onKey = harness({ settings: { apiKeyAlways: true }, apiKeyHint: 'sk-ant-…-123', status: signedOut });
  await onKey.run('checkClaudeSetup()');
  assert.deepEqual(onKey.shown, []);
  const noKey = harness({ settings: { apiKeyAlways: true }, apiKeyHint: null, status: signedOut });
  await noKey.run('checkClaudeSetup()');
  assert.deepEqual(noKey.shown, ['login']);
});
