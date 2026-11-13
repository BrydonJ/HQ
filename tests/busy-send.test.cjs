// The composer's Steer/Queue toggle: what a message sent while Claude or Hermes works does.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const slice = (from, to) => {
  const start = source.indexOf(from);
  assert.ok(start >= 0, from);
  return source.slice(start, source.indexOf(to, start));
};
// setStatus, then the steer-or-queue methods that follow it.
const methods = slice('  setStatus(status) {', '  refreshState() {');

function harness(backend, busySend = null) {
  const node = cls => {
    const set = new Set(cls ? [cls] : []);
    return { classList: { add: c => set.add(c), toggle: (c, on) => (on ? set.add(c) : set.delete(c)), has: c => set.has(c) },
      append() {}, remove() { this.removed = true; }, setAttribute() {}, querySelector: () => ({ textContent: '', className: '' }), set };
  };
  const context = vm.createContext({
    h: (tag, attrs) => Object.assign(node(attrs && attrs.class), { addEventListener() {} }), icon: () => ({}),
    userBubble: text => Object.assign(node('user'), { text }), autoGrow() {}, slashClose() {}, saveState() {},
    queueMicrotask: fn => fn(),
  });
  vm.runInContext(`class BusyCell { ${methods} }`, context);
  const cell = vm.runInContext('new BusyCell()', context);
  const sent = [];
  let interrupts = 0;
  Object.assign(cell, {
    backend, agent: backend === 'hermes' ? 'Hermes' : 'Claude', busySend, held: [], status: 'running', proc: 1, closing: false, steering: false,
    input: { value: '' }, attachments: [], busyChip: node(), lastTurnOk: false,
    startTurn() {}, refreshState() {}, renderAttachments() {},
    append(el) { return el; },
    interrupt() { interrupts++; },
    async submit(prompt, restore, typed) { sent.push(typed); },
  });
  return { cell, sent, interrupts: () => interrupts };
}

test('each agent starts with what it always did: Claude steers, Hermes queues', () => {
  assert.equal(harness('claude').cell.busySendMode(), 'steer');
  assert.equal(harness('hermes').cell.busySendMode(), 'queue');
  assert.equal(harness('codex').cell.busySendOffered(), false);
  assert.equal(harness('cloud').cell.busySendOffered(), false);
});

test('Claude steering and Hermes queueing pass straight to the agent', () => {
  assert.equal(harness('claude', 'steer').cell.holdsMessage(), false);
  assert.equal(harness('hermes', 'queue').cell.holdsMessage(), false);
});

test('nothing is held between turns', () => {
  const { cell } = harness('claude', 'queue');
  cell.status = 'idle';
  assert.equal(cell.holdsMessage(), false);
});

test('Claude queue holds messages until the turn ends, then sends them as one prompt', async () => {
  const { cell, sent, interrupts } = harness('claude', 'queue');
  assert.equal(cell.holdsMessage(), true);
  cell.holdMessage('first', []);
  cell.holdMessage('second', [{ url: 'x' }]);
  assert.equal(interrupts(), 0, 'queue never stops the turn');
  assert.equal(sent.length, 0);
  cell.setStatus('idle');
  await Promise.resolve();
  assert.equal(sent.length, 1);
  assert.equal(sent[0].text, 'first\n\nsecond');
  assert.equal(sent[0].images.length, 1);
  assert.equal(cell.held.length, 0);
});

test('Hermes steer stops the turn once and sends what was held when it ends', async () => {
  const { cell, sent, interrupts } = harness('hermes', 'steer');
  assert.equal(cell.holdsMessage(), true);
  cell.holdMessage('change course', []);
  cell.holdMessage('and this', []);
  assert.equal(interrupts(), 1);
  cell.setStatus('idle');
  await Promise.resolve();
  assert.deepEqual(sent.map(s => s.text), ['change course\n\nand this']);
  assert.equal(cell.steering, false);
});

test('a turn that starts again before the flush keeps the messages for the next end', async () => {
  const { cell, sent } = harness('claude', 'queue');
  cell.holdMessage('later', []);
  cell.status = 'running';
  await cell.sendHeld();
  assert.equal(sent.length, 0);
  assert.equal(cell.held.length, 1);
});

test('the toggle flips and is shown only for Claude and Hermes', () => {
  const { cell } = harness('claude');
  cell.setBusySend('queue');
  assert.equal(cell.busySendMode(), 'queue');
  cell.renderBusySend();
  assert.equal(cell.busyChip.set.has('hidden'), false);
  const codex = harness('codex').cell;
  codex.renderBusySend();
  assert.equal(codex.busyChip.set.has('hidden'), true);
});
