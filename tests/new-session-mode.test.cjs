// The mode a new session starts in (NEW_SESSION_MODE), the one an agent's spawn_session gets
// (mcpSpawnMode), and the note a mode switch shows while Hermes works (hermesBusyModeNote).
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
// Windows checkouts have CRLF line ends; the slices below match on \n.
const read = file => fs.readFileSync(path.join(__dirname, '../ui', file), 'utf8').replace(/\r\n/g, '\n');
const app = read('app.js');
const mcp = read('mcp.js');
const hermes = read('hermes.js');
const slice = (source, from, to) => {
  const start = source.indexOf(from);
  assert.ok(start >= 0, from);
  const end = source.indexOf(to, start);
  assert.ok(end > start, to);
  return source.slice(start, end);
};

const context = vm.createContext({});
vm.runInContext([
  slice(app, 'const AGENT_NAMES =', '\n'),
  slice(app, 'const NEW_SESSION_MODE =', '/** Opens a new session'),
  slice(app, 'const LAUNCH_MODES =', '\n'),
  'function fail(message) { throw new Error(message); }',
  slice(mcp, 'const MCP_MODE_RANK =', '/**\n * Shows a card'),
  slice(hermes, 'const HERMES_MODES =', '\n];') + '\n];',
  slice(hermes, 'function hermesBusyModeNote(', '\n}\n') + '\n}',
  'this.out = { NEW_SESSION_MODE, AUTO_MODES_MOVE, mcpSpawnMode, hermesBusyModeNote };',
].join('\n'), context);
const { NEW_SESSION_MODE, AUTO_MODES_MOVE, mcpSpawnMode, hermesBusyModeNote } = context.out;

test("new Hermes sessions start in Don't ask, Claude's in auto", () => {
  assert.equal(NEW_SESSION_MODE.hermes, 'bypassPermissions');
  assert.equal(NEW_SESSION_MODE.claude, 'auto');
});

test('the one-off restore move keeps the modes of its own release', () => {
  assert.equal(AUTO_MODES_MOVE.hermes, 'acceptEdits');
  assert.equal(AUTO_MODES_MOVE.claude, 'auto');
});

test('a Hermes session an agent starts still gets no more than Edit automatically', () => {
  for (const own of ['auto', 'bypassPermissions', 'acceptEdits']) {
    assert.equal(mcpSpawnMode({ permissionMode: own }, 'hermes', null), 'acceptEdits', own);
  }
  assert.equal(mcpSpawnMode({ permissionMode: 'default' }, 'hermes', null), 'default');
  assert.equal(mcpSpawnMode({ external: true }, 'hermes', null), 'default');
  assert.throws(() => mcpSpawnMode({ permissionMode: 'auto' }, 'hermes', 'bypassPermissions'), /must be one of/);
});

test('Claude and Codex spawns are unchanged', () => {
  assert.equal(mcpSpawnMode({ permissionMode: 'auto' }, 'claude', null), 'auto');
  assert.equal(mcpSpawnMode({ permissionMode: 'acceptEdits' }, 'claude', null), 'acceptEdits');
  assert.equal(mcpSpawnMode({ permissionMode: 'auto' }, 'codex', null), 'default');
});

test('a switch while Hermes works says when it lands', () => {
  const busy = { backend: 'hermes', status: 'running' };
  assert.match(hermesBusyModeNote(busy, 'acceptEdits', 'bypassPermissions'), /^Don't ask .*once this turn ends; until then HQ approves/);
  assert.match(hermesBusyModeNote(busy, 'bypassPermissions', 'default'), /^Ask before edits: .*may still make edits without asking/);
  assert.equal(hermesBusyModeNote({ backend: 'hermes', status: 'idle' }, 'acceptEdits', 'bypassPermissions'), null);
  assert.equal(hermesBusyModeNote({ backend: 'claude', status: 'running' }, 'acceptEdits', 'auto'), null);
  assert.equal(hermesBusyModeNote(busy, 'default', 'default'), null);
});
