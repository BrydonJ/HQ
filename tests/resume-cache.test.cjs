const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const start = source.indexOf('const OTHER_SESSIONS_TTL_MS');
const code = source.slice(start, source.indexOf('/** Every folder with a recent session', start));

function harness() {
  const context = vm.createContext({ Date: { now: () => context.now }, now: 1000 });
  vm.runInContext(code + '\nthis.cachedSessions = cachedSessions; this.forgetCachedSessions = forgetCachedSessions;', context);
  let reads = 0;
  const read = result => () => { reads++; return result === null ? null : Promise.resolve(result); };
  return { context, read, reads: () => reads };
}

const settle = () => new Promise(r => setImmediate(r));

test('a reopened resume dialog reuses the last list for that agent and folder', async () => {
  const { context, read, reads } = harness();
  const first = await context.cachedSessions('Hermes', 'C:\\a', read([{ id: 1 }]));
  const again = await context.cachedSessions('Hermes', 'C:\\a', read([{ id: 2 }]));
  assert.deepEqual(again, first);
  assert.equal(reads(), 1);
  await context.cachedSessions('Hermes', 'C:\\b', read([{ id: 3 }]));
  await context.cachedSessions('Codex', 'C:\\a', read([{ id: 4 }]));
  assert.equal(reads(), 3, 'another folder or agent reads its own');
});

test('the list is read again once it is old, or its agent closed a cell', async () => {
  const { context, read, reads } = harness();
  await context.cachedSessions('Hermes', '', read([{ id: 1 }]));
  context.now += context.OTHER_SESSIONS_TTL_MS ?? 5 * 60 * 1000;
  await context.cachedSessions('Hermes', '', read([{ id: 1 }]));
  assert.equal(reads(), 2, 'expired');
  await context.cachedSessions('Codex', '', read([{ id: 2 }]));
  context.forgetCachedSessions('Hermes');
  await context.cachedSessions('Hermes', '', read([{ id: 1 }]));
  await context.cachedSessions('Codex', '', read([{ id: 2 }]));
  assert.equal(reads(), 4, 'Hermes forgotten, Codex kept');
});

test('an empty or failed read is not kept, and an agent not asked for gives null', async () => {
  const { context, read, reads } = harness();
  await context.cachedSessions('Hermes', '', read([]));
  await settle();
  await context.cachedSessions('Hermes', '', read([{ id: 1 }]));
  assert.equal(reads(), 2);
  assert.equal(context.cachedSessions('Codex', '', read(null)), null);
});
