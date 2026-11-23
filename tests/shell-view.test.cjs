const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

const source = fs.readFileSync(path.join(__dirname, '../ui/render.js'), 'utf8');
const start = source.indexOf('const SHELL_RISKY');
const code = source.slice(start, source.indexOf('function relPath', start));

// A minimal DOM: spans record their class and text, text nodes are plain strings.
function run(cmd) {
  const out = [];
  const context = vm.createContext({
    document: {
      createDocumentFragment: () => ({ append: n => out.push(n) }),
      createTextNode: text => ({ cls: '', text }),
    },
    h: (_tag, attrs) => ({ cls: attrs.class, text: attrs.text }),
  });
  vm.runInContext(`${code}; globalThis.shellView = shellView;`, context);
  context.shellView(cmd);
  return out.filter(n => n.cls).map(n => `${n.cls}:${n.text}`);
}

test('commands, flags, strings, variables and separators get their own class', () => {
  assert.deepEqual(run('ls -la "$E" | wc -l'), ['sh-cmd:ls', 'sh-flag:-la', 'sh-str:"$E"', 'sh-sep:|', 'sh-cmd:wc', 'sh-flag:-l']);
});

test('risky commands and git force/push are flagged', () => {
  assert.deepEqual(run('rm -rf x'), ['sh-risk:rm', 'sh-flag:-rf']);
  assert.deepEqual(run('git push --force'), ['sh-cmd:git', 'sh-risk:push', 'sh-risk:--force']);
  assert.deepEqual(run('ls -f'), ['sh-cmd:ls', 'sh-flag:-f']);
});

test('a heredoc body is dimmed and the next command is recognised again', () => {
  const got = run("python - <<'EOF'\nprint('x')\nEOF\ncp a b");
  assert.ok(got.includes('sh-doc:print(\'x\')'));
  assert.ok(got.includes('sh-cmd:cp'));
});

test('an assignment before a command keeps the command slot', () => {
  assert.deepEqual(run('A=1 ls'), ['sh-var:A=1', 'sh-cmd:ls']);
});

test('the text is kept exactly (nothing is lost or reordered)', () => {
  const cmd = 'cd /x && echo "a b" ; cp $A/f "$E"/ 2>&1 & cat <<< hi';
  const out = [];
  const context = vm.createContext({
    document: { createDocumentFragment: () => ({ append: n => out.push(n) }), createTextNode: text => ({ text }) },
    h: (_t, a) => ({ text: a.text }),
  });
  vm.runInContext(`${code}; globalThis.shellView = shellView;`, context);
  context.shellView(cmd);
  assert.equal(out.map(n => n.text).join(''), cmd);
});
