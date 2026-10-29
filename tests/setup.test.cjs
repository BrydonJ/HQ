const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const setupFunctions = source.slice(source.indexOf('async function installClaude()'), source.indexOf('\nfunction finishSetup()'));

function harness(status) {
  const calls = [];
  const setup = { step: 'waiting', since: Date.now(), skipped: false };
  const context = vm.createContext({
    setup, app: { settings: {} }, Date, apiAlways: () => false, SETUP_POLL_MS: 2500, SETUP_GIVE_UP_MS: 900000,
    $: () => ({}), saveState() {}, clearInterval() {}, setInterval: () => 1,
    setSetupStatus(text) { calls.push(text); },
    showSetup(step) { setup.step = step; },
    finishSetup() { calls.push('finished'); setup.step = null; },
    invoke: async command => { calls.push(command); return command === 'claude_status' ? status : undefined; },
  });
  vm.runInContext(setupFunctions, context);
  return { calls, setup, run: code => vm.runInContext(code, context) };
}

for (const loggedIn of [null, false, true]) {
  test(`poll only completes for confirmed authentication: ${loggedIn}`, async () => {
    const h = harness({ installed: true, loggedIn });
    await h.run('pollSignIn()');
    assert.equal(h.calls.includes('finished'), loggedIn === true);
    assert.equal(h.setup.step, loggedIn === true ? null : 'waiting');
    if (loggedIn === null) assert.ok(h.calls.some(c => c.includes('keep checking')));
  });
  test(`install only skips sign-in for confirmed authentication: ${loggedIn}`, async () => {
    const h = harness({ installed: true, loggedIn });
    await h.run('installClaude()');
    // installClaude starts signInClaude without awaiting it.
    await Promise.resolve();
    assert.equal(h.calls.includes('finished'), loggedIn === true);
    assert.equal(h.calls.includes('login_claude'), loggedIn !== true);
  });
}

test('status command failure keeps the sign-in window alive', async () => {
  const h = harness(null);
  await h.run("invoke = async () => { throw new Error('timeout'); }; pollSignIn()");
  assert.equal(h.calls.includes('finished'), false);
  assert.equal(h.setup.step, 'waiting');
});

test('unconfirmed sign-in times out to a retry without claiming success', async () => {
  const h = harness({ installed: true, loggedIn: null });
  h.setup.since = 0;
  await h.run('pollSignIn()');
  assert.equal(h.calls.includes('finished'), false);
  assert.equal(h.setup.step, 'login');
});

test('late successful status does not complete a cancelled sign-in', async () => {
  const h = harness({ installed: true, loggedIn: true });
  h.setup.step = null;
  await h.run('pollSignIn()');
  assert.equal(h.calls.includes('finished'), false);
});
