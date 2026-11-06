// The approval reviewer's timing and races (ui/afk.js), on the real afk.js and the real prompt
// methods of ui/app.js, with a clock the tests move by hand.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const afkSource = fs.readFileSync(path.join(__dirname, '../ui/afk.js'), 'utf8');
const appSource = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const slice = (from, to) => appSource.slice(appSource.indexOf(from), appSource.indexOf(to));
const methods = slice('  /** Starts AFK\'s watch over a prompt card', '  /** Folds the prompts area') + slice('  dropPrompt(requestId) {', '  permissionCard(requestId, req) {');

const GRACE = 60_000;
const APPROVE = { approve: true, reason: 'a test run' };
const ESCALATE = { approve: false, reason: 'not sure' };
const flush = async () => { for (let i = 0; i < 10; i++) await Promise.resolve(); };

function element(cls) {
  const el = { class: cls, style: {}, dataset: {}, children: [], textContent: '', listeners: {},
    addEventListener(name, fn) { this.listeners[name] = fn; },
    append(...kids) { this.children.push(...kids); }, prepend(kid) { this.children.unshift(kid); kid.parent = this; },
    remove() { if (this.parent) this.parent.children = this.parent.children.filter(c => c !== this); this.removed = true; },
    classList: { toggle() {} }, querySelector: () => element(), setAttribute() {} };
  return el;
}

function harness() {
  let now = 1_000_000, nextId = 1;
  const timers = new Map();
  class Clock extends Date { static now() { return now; } }
  const reviews = [], attention = [];
  const app = { cells: [], afk: null };
  const set = (fn, ms, repeat) => { timers.set(nextId, { fn, at: now + ms, repeat: repeat ? ms : 0 }); return nextId++; };
  const h = (tag, attrs, ...kids) => {
    const el = element(attrs && attrs.class);
    if (attrs && attrs.text) el.textContent = attrs.text;
    el.attrs = attrs;
    el.children = kids.flat().filter(Boolean);
    return el;
  };
  const context = vm.createContext({ app, Date: Clock, h, icon: () => element('icon'), console,
    setTimeout: (fn, ms) => set(fn, ms, false), setInterval: (fn, ms) => set(fn, ms, true),
    clearTimeout: id => timers.delete(id), clearInterval: id => timers.delete(id),
    document: { getElementById: () => element('pill') },
    invoke: (name, args) => {
      if (name === 'request_attention') { attention.push(1); return Promise.resolve(); }
      return new Promise((resolve, reject) => reviews.push({ name, req: args.req, resolve, reject }));
    },
    saveState() {}, closeMenu() {}, notifyWaiting() {}, firstLine: s => String(s).split('\n')[0], relPath: p => p,
    openMenu() {}, afkPillStub: null,
  });
  vm.runInContext(`${afkSource}\nclass CardCell { ${methods}
    syncPromptsBar() {} refreshState() {}
    respond(id, response) { this.answers.push({ id, response }); }
  }`, context);
  const make = (options = {}) => {
    const cell = vm.runInContext('new CardCell()', context);
    Object.assign(cell, { backend: 'claude', cwd: '/repo', proc: 1, pending: new Map(), answers: [], notices: [], afk: null, afkOn: false, afkLog: [], review: false, status: 'idle',
      lastUserText: 'run the tests', agent: 'Claude', bgTasks: [], renderAfk() {}, append() {}, markUnread() {}, refreshState() {}, submit: async () => {}, notice(t) { this.notices.push(t); }, ...options });
    app.cells.push(cell);
    return cell;
  };
  const card = (cell, id = 'r1', kind = 'permission', req = { tool_name: 'Bash', input: { command: 'cargo test', description: 'agent words' }, description: 'agent words' }) => {
    const c = element('prompt');
    c.dataset.kind = kind;
    c.afkReq = req;
    cell.pending.set(id, c);
    return c;
  };
  const arm = (cell, id, c) => cell.armPrompt(id, c.afkReq, c);
  const tick = async ms => {
    const end = now + ms;
    for (;;) {
      const due = [...timers.entries()].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
      if (!due) break;
      now = Math.max(now, due[1].at);
      if (due[1].repeat) due[1].at += due[1].repeat; else timers.delete(due[0]);
      due[1].fn();
      await flush();
    }
    now = end;
    await flush();
  };
  const run = name => vm.runInContext(name, context);
  return { app, make, card, arm, tick, reviews, attention, context, run, get now() { return now; } };
}

const text = el => [el.textContent, ...el.children.map(text)].join(' ');

test('off by default: nothing is reviewed and nothing counts down', async () => {
  const t = harness(), cell = t.make(), c = t.card(cell);
  t.arm(cell, 'r1', c);
  assert.equal(c.children.length, 0);
  await t.tick(GRACE * 2);
  assert.equal(t.reviews.length, 0);
  assert.equal(cell.answers.length, 0);
});

test('the reviewer never steps in before the card has waited 60 s, and the card counts it down', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  t.arm(cell, 'r1', c);
  assert.match(text(c), /Reviewing in 1:00/);
  await t.tick(18_000);
  assert.match(text(c), /Reviewing in 0:42/);
  await t.tick(GRACE - 18_001);
  assert.equal(t.reviews.length, 0);
  await t.tick(1);
  assert.equal(t.reviews.length, 1);
  assert.match(text(c), /Reviewing…/);
});

test('an approve answers Yes once, says so, and sends no session rule', async () => {
  const t = harness(), cell = t.make({ review: true });
  const c = t.card(cell, 'r1', 'permission', { tool_name: 'Bash', input: { command: 'cargo test' }, permission_suggestions: [{ type: 'addRules' }] });
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  const { req } = t.reviews[0];
  assert.deepEqual({ ...req, input: { ...req.input } }, { requestId: 'r1', toolName: 'Bash', input: { command: 'cargo test' }, cwd: '/repo', backend: 'claude', lastUserMessage: 'run the tests', hermesPlainEdit: false });
  t.reviews[0].resolve(APPROVE);
  await t.tick(0);
  assert.equal(cell.answers.length, 1);
  assert.equal(cell.answers[0].id, 'r1');
  assert.equal(cell.answers[0].response.behavior, 'allow');
  assert.equal(cell.answers[0].response.updatedPermissions, undefined);
  assert.equal(cell.pending.size, 0);
  assert.equal(cell.notices[0], 'Reviewer approved Bash: cargo test (a test run)');
});

test('outside AFK an approval leaves no log entry, which would raise a "while you were away" card', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  t.reviews[0].resolve(APPROVE);
  await t.tick(0);
  assert.equal(cell.afkLog.length, 0);
});

test('an escalate leaves the card, rings again and does not ask twice', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  const before = t.attention.length;
  t.reviews[0].resolve(ESCALATE);
  await t.tick(0);
  assert.equal(cell.answers.length, 0);
  assert.equal(cell.pending.get('r1'), c);
  assert.equal(c.children.length, 0);
  assert.equal(t.attention.length, before + 1);
  assert.match(cell.notices[0], /not sure/);
  t.context.reviewSync(cell);
  await t.tick(GRACE * 2);
  assert.equal(t.reviews.length, 1);
});

for (const [name, failure] of [['a failed call', r => r.reject('boom')], ['an unexpected reply', r => r.resolve({ approve: 'true', reason: 'x' })], ['no reply', r => r.resolve(null)]]) {
  test(`${name} is an escalate`, async () => {
    const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
    t.arm(cell, 'r1', c);
    await t.tick(GRACE);
    failure(t.reviews[0]);
    await t.tick(0);
    assert.equal(cell.answers.length, 0);
    assert.equal(cell.pending.get('r1'), c);
  });
}

test('answering during the countdown stops it', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  t.arm(cell, 'r1', c);
  await t.tick(10_000);
  cell.answer('r1', { behavior: 'deny', message: 'no' });
  await t.tick(GRACE * 2);
  assert.equal(t.reviews.length, 0);
  assert.equal(cell.answers.length, 1);
});

test('a card answered during the review is left alone', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  cell.answer('r1', { behavior: 'deny', message: 'no' });
  t.reviews[0].resolve(APPROVE);
  await t.tick(0);
  assert.equal(cell.answers.length, 1);
  assert.equal(cell.answers[0].response.behavior, 'deny');
  assert.equal(cell.notices.length, 0);
});

test('a card cancelled during the review, or a new card under the same id, is left alone', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  cell.dropPrompt('r1');
  const other = t.card(cell);
  t.reviews[0].resolve(APPROVE);
  await t.tick(0);
  assert.equal(cell.answers.length, 0);
  assert.equal(cell.pending.get('r1'), other);
});

test('the card is checked again after the call, even with no other sign it has gone', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  cell.pending.delete('r1');
  t.reviews[0].resolve(APPROVE);
  await t.tick(0);
  assert.equal(cell.answers.length, 0);
});

test('an AFK that ran out during the call does not answer, before the next tick has noticed', async () => {
  const t = harness(), cell = t.make(), c = t.card(cell);
  afk(t, cell);
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  cell.afk.until = t.now;
  t.reviews[0].resolve(APPROVE);
  await t.tick(0);
  assert.equal(cell.answers.length, 0);
  assert.doesNotMatch(text(c), /Reviewing/);
});

test('someone typing in the card is not overridden, at either end of the call', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  c.feedback = { value: 'No, use the staging DB' };
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  assert.equal(t.reviews.length, 0);
  assert.equal(cell.answers.length, 0);
  assert.match(cell.notices[0], /typing/);

  const u = harness(), other = u.make({ review: true }), d = u.card(other);
  d.feedback = { value: '' };
  u.arm(other, 'r1', d);
  await u.tick(GRACE);
  d.feedback.value = 'No';
  u.reviews[0].resolve(APPROVE);
  await u.tick(0);
  assert.equal(other.answers.length, 0);
});

test('focus in the feedback box counts as answering', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  c.feedback = { value: '' };
  vm.runInContext('document', t.context).activeElement = c.feedback;
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  assert.equal(t.reviews.length, 0);
  assert.equal(cell.answers.length, 0);
});

test('a command line marks its cell and its prompt is not taken for the user', () => {
  const launch = appSource.slice(appSource.indexOf('async function openLaunch'), appSource.indexOf("window.__TAURI__.event.listen('launch'"));
  assert.match(launch, /launched: true/);
  assert.match(launch, /cell\.launchPrompt = r\.prompt\.trim\(\)/);
  assert.match(appSource, /text !== this\.launchPrompt\) this\.lastUserText = text/);
});

test('a review that cannot finish hands the card back to AFK', async () => {
  const t = harness(), cell = t.make();
  t.context.setAfk(null, Infinity, false);
  afk(t, cell);
  const c = t.card(cell);
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  cell.afk = null; // its own AFK with review is gone; HQ's, without review, still covers it
  t.reviews[0].resolve(APPROVE);
  await t.tick(0);
  assert.equal(cell.answers.length, 0);
  assert.match(text(c), /AFK: Deferring in/);
});

test('a Hermes plain edit is passed on as one', async () => {
  const t = harness(), cell = t.make({ review: true, backend: 'hermes' });
  const c = t.card(cell, 'r1', 'permission', { tool_name: 'Bash', input: { command: 'x' }, hermes_plain_edit: true });
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  assert.equal(t.reviews[0].req.hermesPlainEdit, true);
});

test('a closed or stopped session is not answered', async () => {
  for (const change of [cell => { cell.closing = true; }, (cell, t) => { t.app.cells.length = 0; }, cell => { cell.proc = null; }]) {
    const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
    t.arm(cell, 'r1', c);
    await t.tick(GRACE);
    change(cell, t);
    t.reviews[0].resolve(APPROVE);
    await t.tick(0);
    assert.equal(cell.answers.length, 0);
  }
});

test('turning review off stops it, and a reply still on its way is ignored even if it is turned back on', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  t.context.setCellReview(cell, false);
  assert.equal(c.children.length, 0);
  t.context.setCellReview(cell, true);
  t.reviews[0].resolve(APPROVE);
  await t.tick(0);
  assert.equal(cell.answers.length, 0);
  await t.tick(GRACE);
  assert.equal(t.reviews.length, 2);
});

test('turning review on keeps the time the card has already waited', async () => {
  const t = harness(), cell = t.make(), c = t.card(cell);
  t.arm(cell, 'r1', c);
  await t.tick(GRACE - 5_000);
  t.context.setCellReview(cell, true);
  await t.tick(4_999);
  assert.equal(t.reviews.length, 0);
  await t.tick(1);
  assert.equal(t.reviews.length, 1);
});

test('only permissions are reviewed: not questions, plans or HQ\'s own cards', async () => {
  const t = harness(), cell = t.make({ review: true });
  const cards = [t.card(cell, 'q', 'question'), t.card(cell, 'p', 'plan'), t.card(cell, 'mcp-1', 'permission')];
  t.arm(cell, 'q', cards[0]); t.arm(cell, 'p', cards[1]); t.arm(cell, 'mcp-1', cards[2]);
  await t.tick(GRACE * 2);
  assert.equal(t.reviews.length, 0);
});

test('Codex and cloud cells are never reviewed, whatever is saved', async () => {
  for (const backend of ['codex', 'cloud']) {
    const t = harness(), cell = t.make({ review: true, backend }), c = t.card(cell);
    t.arm(cell, 'r1', c);
    await t.tick(GRACE * 2);
    assert.equal(t.reviews.length, 0, backend);
  }
});

test('Hermes cells are reviewed', async () => {
  const t = harness(), cell = t.make({ review: true, backend: 'hermes' }), c = t.card(cell);
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  assert.equal(t.reviews[0].req.backend, 'hermes');
});

test('a replayed card waits for armPrompt, then gets a full review', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  c.replayed = true;
  t.context.reviewSync(cell);
  assert.equal(c.children.length, 0);
  c.replayed = false;
  t.arm(cell, 'r1', c);
  await t.tick(GRACE - 1);
  assert.equal(t.reviews.length, 0);
  await t.tick(1);
  assert.equal(t.reviews.length, 1);
});

test('arming a card twice does not start two reviews', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  t.arm(cell, 'r1', c);
  t.arm(cell, 'r1', c);
  await t.tick(GRACE * 2);
  assert.equal(t.reviews.length, 1);
});

// ---------- with AFK ----------

function afk(t, cell, review = true) {
  t.context.setAfk(cell, Infinity, review);
}

test('with AFK on there is one countdown, and an approve is logged for the digest with no deferral', async () => {
  const t = harness(), cell = t.make(), c = t.card(cell);
  afk(t, cell);
  t.arm(cell, 'r1', c);
  assert.equal(c.children.length, 1);
  assert.match(text(c), /Reviewing in/);
  await t.tick(GRACE);
  t.reviews[0].resolve(APPROVE);
  await t.tick(0);
  assert.equal(cell.answers.length, 1);
  assert.equal(cell.answers[0].response.behavior, 'allow');
  assert.deepEqual(cell.afkLog.map(e => e.kind), ['approved']);
});

test('with AFK on, an escalate is then deferred as usual', async () => {
  const t = harness(), cell = t.make(), c = t.card(cell);
  afk(t, cell);
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  assert.equal(cell.answers.length, 0);
  t.reviews[0].resolve(ESCALATE);
  await t.tick(0);
  assert.equal(cell.answers.length, 1);
  assert.equal(cell.answers[0].response.behavior, 'deny');
  assert.deepEqual(cell.afkLog.map(e => e.kind), ['permission']);
});

test('AFK without the review option defers as it always did', async () => {
  const t = harness(), cell = t.make(), c = t.card(cell);
  afk(t, cell, false);
  t.arm(cell, 'r1', c);
  assert.match(text(c), /AFK: Deferring in/);
  await t.tick(GRACE);
  assert.equal(t.reviews.length, 0);
  assert.equal(cell.answers[0].response.behavior, 'deny');
});

test('going AFK leaves a countdown running rather than deferring its card at once', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  t.arm(cell, 'r1', c);
  await t.tick(30_000);
  afk(t, cell, false);
  assert.equal(cell.answers.length, 0);
  await t.tick(30_000);
  assert.equal(t.reviews.length, 1);
});

test('going AFK with the option gives a waiting card the rest of its minute, not an instant answer', async () => {
  const t = harness(), cell = t.make(), c = t.card(cell);
  t.arm(cell, 'r1', c);
  await t.tick(10_000);
  afk(t, cell);
  assert.equal(cell.answers.length, 0);
  await t.tick(49_999);
  assert.equal(t.reviews.length, 0);
  await t.tick(1);
  assert.equal(t.reviews.length, 1);
});

test('going AFK defers a card the reviewer already escalated, without asking again', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  t.reviews[0].resolve(ESCALATE);
  await t.tick(0);
  afk(t, cell, false);
  await t.tick(0);
  assert.equal(t.reviews.length, 1);
  assert.equal(cell.answers[0].response.behavior, 'deny');
});

test('AFK ending keeps the session\'s own review running', async () => {
  const t = harness(), cell = t.make({ review: true }), c = t.card(cell);
  afk(t, cell, false);
  t.arm(cell, 'r1', c);
  t.context.setAfk(cell, 0);
  await t.tick(GRACE);
  assert.equal(t.reviews.length, 1);
});

test('a review from AFK alone ends with it: the countdown stops and a late reply is ignored', async () => {
  const t = harness(), a = t.make(), ca = t.card(a), b = t.make(), cb = t.card(b);
  afk(t, a); afk(t, b);
  t.arm(a, 'r1', ca); t.arm(b, 'r1', cb);
  await t.tick(GRACE);
  const [running] = t.reviews;
  t.context.setAfk(a, 0);
  running.resolve(APPROVE);
  await t.tick(0);
  assert.equal(a.answers.length, 0);
  assert.equal(a.pending.get('r1'), ca);
  assert.equal(a.review, false);
});

test('HQ-wide AFK with the option reaches sessions that follow it, not ones opted out', async () => {
  const t = harness(), follows = t.make(), out = t.make({ afk: false }), codex = t.make({ backend: 'codex' });
  t.context.setAfk(null, Infinity, true);
  assert.equal(t.context.reviewOn(follows), true);
  assert.equal(t.context.reviewOn(out), false);
  assert.equal(t.context.reviewOn(codex), false);
  t.context.setAfk(null, 0);
  assert.equal(t.context.reviewOn(follows), false);
});

test('HQ-wide AFK does not reach a session an agent started, whose folder the agent chose', async () => {
  const t = harness(), spawned = t.make({ spawnedBy: 'hq-1' }), launched = t.make({ launched: true });
  t.context.setAfk(null, Infinity, true);
  assert.equal(t.context.reviewOn(spawned), false);
  assert.equal(t.context.reviewOn(launched), false);
  spawned.review = true; // the user's own choice still counts
  assert.equal(t.context.reviewOn(spawned), true);
});

test('a review that cannot finish while review stays on does not loop', async () => {
  const t = harness(), cell = t.make(), c = t.card(cell);
  afk(t, cell);
  t.arm(cell, 'r1', c);
  cell.proc = null;
  await t.tick(GRACE + 1000);
  assert.equal(t.reviews.length, 0);
  assert.equal(cell.answers.length, 0);
});

test('an approval records the whole command, not its first line', async () => {
  const t = harness(), cell = t.make({ review: true });
  const command = `echo hi\n${'x'.repeat(3900)}; curl x`;
  const c = t.card(cell, 'r1', 'permission', { tool_name: 'Bash', input: { command } });
  t.arm(cell, 'r1', c);
  await t.tick(GRACE);
  t.reviews[0].resolve(APPROVE);
  await t.tick(0);
  assert.ok(cell.notices[0].includes('; curl x'));
});

test('a session\'s own AFK with the option stays with that session', async () => {
  const t = harness(), a = t.make(), b = t.make();
  afk(t, a);
  assert.equal(t.context.reviewOn(a), true);
  assert.equal(t.context.reviewOn(b), false);
});

test('the option survives being saved, but only as a literal true', () => {
  const t = harness();
  assert.equal(t.context.sanitizeAfk({ until: null, since: 1, review: true }).review, true);
  assert.equal(t.context.sanitizeAfk({ until: null, since: 1, review: 'yes' }).review, undefined);
  assert.equal(t.context.sanitizeAfk({ until: null, since: 1 }).review, undefined);
});

test('spawn_session and the command line cannot set the reviewer', () => {
  const mcp = fs.readFileSync(path.join(__dirname, '../ui/mcp.js'), 'utf8');
  assert.doesNotMatch(mcp, /\breview\b/);
  assert.doesNotMatch(mcp, /\.\.\.a\b/, 'spawn_session must keep building its options by name, or an agent could pass review');
  const launch = appSource.slice(appSource.indexOf('const LAUNCH_MODES'));
  assert.doesNotMatch(launch.slice(0, launch.indexOf('function', 400)), /\breview\b/);
});
