const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const methods = source.slice(source.indexOf('  showUsageLimit(text, replay) {'), source.indexOf('  /** Text followed by a tool call'));
const scheduler = source.slice(source.indexOf('const LIMIT_RESUME_PROMPT ='), source.indexOf('/** A finished turn changes usage'));

function harness() {
  let now = 1_000_000, reset = now + 1000, refreshes = 0;
  const app = { cells: [] };
  const usage = { fetchedAt: now };
  class Clock extends Date { static now() { return now; } }
  const context = vm.createContext({ app, usage, Date: Clock, USAGE_EVERY_MS: 300_000,
    setInterval: () => 1, clearInterval() {}, labelLimitApi() {}, scheduleUsageRefresh() {},
    usageResetAt: () => reset > now ? reset : 0,
    refreshUsage: async () => { refreshes++; usage.fetchedAt = now; },
  });
  vm.runInContext(`class LimitCell { ${methods} }\n${scheduler}`, context);
  const make = (options = {}) => {
    const control = { classList: { add() {}, remove() {}, toggle() {} }, textContent: '' };
    const card = { dataset: {}, querySelector: () => control };
    const cell = vm.runInContext('new LimitCell()', context);
    Object.assign(cell, { backend: 'claude', apiMode: false, procOnApi: false, apiSource: null,
      limitAutoContinue: true, limitResume: null, limitResetAt: reset, status: 'idle', pending: new Map(),
      msgs: { querySelectorAll: selector => selector.startsWith(':scope') ? [card] : [] },
      append: node => node, sent: [], notice() {}, submit(text) { this.sent.push(text); }, ...options });
    app.cells.push(cell);
    return cell;
  };
  return { make, app, usage, context, set now(value) { now = value; }, get now() { return now; },
    set reset(value) { reset = value; }, get refreshes() { return refreshes; },
    tick: () => vm.runInContext('checkLimitResumes()', context) };
}

test('live usage errors automatically arm each Claude chat and resume once', async () => {
  const h = harness(), a = h.make(), b = h.make();
  for (const c of [a, b]) c.showUsageLimit('Usage limit reached', false);
  assert.ok(a.limitResume && b.limitResume);
  await h.tick();
  assert.equal(h.refreshes, 0);
  h.now += 1000;
  await h.tick(); await h.tick();
  assert.equal(a.sent.length, 1); assert.equal(b.sent.length, 1);
  assert.equal(a.limitAutoContinue, true);
});

test('unchecking is per-chat and survives repeated usage errors while open', async () => {
  const h = harness(), a = h.make(), b = h.make();
  a.showUsageLimit('Usage limit reached', false);
  a.setLimitAutoContinue(false);
  a.showUsageLimit('Usage limit reached again', false);
  b.showUsageLimit('Usage limit reached', false);
  assert.equal(a.limitResume, null);
  h.now += 1000; await h.tick();
  assert.equal(a.sent.length, 0); assert.equal(b.sent.length, 1);
  a.setLimitAutoContinue(true);
  assert.ok(a.limitResume);
});

for (const options of [{ backend: 'codex' }, { backend: 'hermes' }, { apiMode: true }, { procOnApi: true }, { apiSource: 'ANTHROPIC_API_KEY' }]) {
  test(`does not arm non-plan chats: ${JSON.stringify(options)}`, () => {
    const h = harness(), cell = h.make(options);
    cell.showUsageLimit('Usage limit reached', false);
    assert.equal(cell.limitResume, null);
  });
}

test('historical usage errors never automatically resume', () => {
  const h = harness(), cell = h.make();
  cell.showUsageLimit('Usage limit reached', true);
  assert.equal(cell.limitResume, null);
});

test('internal cancellation does not change the checkbox preference', () => {
  const h = harness(), cell = h.make();
  cell.showUsageLimit('Usage limit reached', false);
  cell.setLimitResume(false);
  assert.equal(cell.limitAutoContinue, true);
  cell.showUsageLimit('Usage limit reached again', false);
  assert.ok(cell.limitResume);
});

for (const options of [{ status: 'running' }, { pending: new Map([['approval', {}]]) }, { spawning: true }, { closing: true }]) {
  test(`does not resume busy or closing chats: ${JSON.stringify(options)}`, async () => {
    const h = harness(), cell = h.make(options);
    cell.showUsageLimit('Usage limit reached', false);
    h.now += 1000; await h.tick();
    assert.equal(cell.sent.length, 0);
  });
}

test('unchecking or closing while usage refreshes prevents continuation', async () => {
  const h = harness(), a = h.make(), b = h.make();
  a.showUsageLimit('Usage limit reached', false); b.showUsageLimit('Usage limit reached', false);
  h.context.refreshUsage = async () => { a.setLimitAutoContinue(false); h.app.cells.splice(h.app.cells.indexOf(b), 1); h.usage.fetchedAt = h.now; };
  h.now += 1000; await h.tick();
  assert.equal(a.sent.length + b.sent.length, 0);
});

test('new limit received during refresh is not resumed at the old deadline', async () => {
  const h = harness(), cell = h.make();
  cell.showUsageLimit('Usage limit reached', false);
  h.context.refreshUsage = async () => {
    cell.limitResetAt = h.now + 10_000;
    cell.showUsageLimit('New limit', false); h.usage.fetchedAt = h.now;
  };
  h.now += 1000; await h.tick();
  assert.equal(cell.sent.length, 0);
});

test('a still-exhausted limit extends the wait', async () => {
  const h = harness(), cell = h.make();
  cell.showUsageLimit('Usage limit reached', false);
  h.reset = h.now + 10_000;
  h.now += 1000; await h.tick();
  assert.equal(cell.sent.length, 0);
  assert.equal(cell.limitResume.resetsAt, 1_010_000);
});

test('unknown reset waits before checking usage', async () => {
  const h = harness(); h.reset = 0;
  const cell = h.make({ limitResetAt: 0 });
  cell.showUsageLimit('Usage limit reached', false);
  assert.equal(cell.limitResume.resetsAt, h.now + 300_000);
  await h.tick(); assert.equal(h.refreshes, 0);
});
