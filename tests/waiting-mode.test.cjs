const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const start = source.indexOf('  setWaitingMode(on) {');
const methods = source.slice(start, source.indexOf('  setWmView(view) {', start));

function harness() {
  let pending = null;
  const schedule = [];
  const context = vm.createContext({
    WAIT_SETTLE_MS: 4000, mcp: { schedule },
    setTimeout: fn => { pending = fn; return 1; },
    clearTimeout: () => { pending = null; },
  });
  vm.runInContext(`class WaitCell { ${methods} }`, context);
  const cell = vm.runInContext('new WaitCell()', context);
  const classes = new Set();
  Object.assign(cell, {
    hqId: 'me', closing: false, wmArmed: false, wmWorked: false, wmTimer: 0, wmDone: false, wmView: false, wmNews: false, lastTurnOk: false,
    status: 'idle', pending: new Map(), bgTasks: [], limitHit: false, limitResume: null,
    waitBtn: { classList: { toggle() {} } },
    el: { classList: { toggle: (name, on) => { if (on) classes.add(name); else classes.delete(name); } } },
    syncWaitingBusy() {}, renderProgress() {},
    setWmView(view) { this.wmView = view; },
  });
  const fire = () => { const fn = pending; pending = null; assert.ok(fn, 'a settle timeout is running'); fn(); };
  const set = changes => { Object.assign(cell, changes); cell.syncWaiting(); };
  return { cell, classes, schedule, fire, set, timing: () => pending != null };
}

/** Armed during a turn that then ended cleanly, so the settle timeout is running. */
function finished() {
  const t = harness();
  t.set({ status: 'running' });
  t.cell.setWaitingMode(true);
  t.set({ status: 'idle', lastTurnOk: true });
  return t;
}

test('waiting mode stays on after the cell settles, and the view says done', () => {
  const { cell, classes, fire } = finished();
  assert.equal(cell.wmDone, false, 'not done until it has stayed idle');
  fire();
  assert.equal(cell.wmArmed, true);
  assert.equal(cell.wmView, true);
  assert.equal(cell.wmDone, true);
  assert.ok(classes.has('wm-done'));
});

test('a refresh while done does not start the settle count again', () => {
  const { cell, fire, timing } = finished();
  fire();
  cell.syncWaiting();
  assert.equal(timing(), false);
  assert.equal(cell.wmDone, true);
});

test('new work takes done away and keeps the view', () => {
  const { cell, classes, fire, set } = finished();
  fire();
  set({ status: 'running' });
  assert.equal(cell.wmDone, false);
  assert.ok(!classes.has('wm-done'));
  assert.equal(cell.wmView, true);
  assert.equal(cell.wmArmed, true);
});

test('work starting during the settle count cancels it', () => {
  const { cell, set, timing } = finished();
  set({ status: 'running' });
  assert.equal(timing(), false);
  assert.equal(cell.wmDone, false);
});

test('background work is not done', () => {
  const { cell, set, timing } = finished();
  set({ bgTasks: [{ id: 1 }] });
  assert.equal(timing(), false);
  assert.equal(cell.wmDone, false);
  assert.equal(cell.wmView, true);
});

test('a question shows the chat, then the view returns without a stale done', () => {
  const { cell, fire, set } = finished();
  fire();
  set({ pending: new Map([['q', {}]]) });
  assert.equal(cell.wmView, false);
  assert.equal(cell.wmDone, false);
  assert.equal(cell.wmArmed, true);
  set({ pending: new Map() });
  assert.equal(cell.wmView, true);
  assert.equal(cell.wmDone, false);
});

test('a failed or stopped turn, an exit or a usage limit shows the chat, not done, and stays armed', () => {
  for (const changes of [{ lastTurnOk: false }, { status: 'exited' }, { limitHit: true }, { limitResume: { resetsAt: 1 } }]) {
    const { cell, set, timing } = finished();
    set(changes);
    assert.equal(cell.wmView, false);
    assert.equal(cell.wmDone, false);
    assert.equal(timing(), false);
    assert.equal(cell.wmArmed, true);
  }
});

test('a message scheduled for this cell holds done back until it is gone', () => {
  const { cell, schedule, fire } = finished();
  schedule.push({ to: 'me' });
  fire();
  assert.equal(cell.wmDone, false);
  schedule.length = 0;
  fire();
  assert.equal(cell.wmDone, true);
});

test('a message scheduled for another cell does not hold done back', () => {
  const { cell, schedule, fire } = finished();
  schedule.push({ to: 'other' });
  fire();
  assert.equal(cell.wmDone, true);
});

test('the button turns it off, done included', () => {
  const { cell, classes, fire } = finished();
  fire();
  cell.setWaitingMode(false);
  assert.equal(cell.wmArmed, false);
  assert.equal(cell.wmView, false);
  assert.equal(cell.wmDone, false);
  assert.ok(!classes.has('wm-done'));
});

test('arming an idle cell waits for work and never says done first', () => {
  const { cell, set, timing } = harness();
  cell.setWaitingMode(true);
  assert.equal(cell.wmView, false);
  assert.equal(timing(), false);
  set({ lastTurnOk: true });
  assert.equal(cell.wmView, false);
  assert.equal(cell.wmDone, false);
});
