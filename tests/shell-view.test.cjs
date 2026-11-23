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

test('an assignment before a command keeps the command slot', () => {
  assert.deepEqual(run('A=1 ls'), ['sh-var:A=1', 'sh-cmd:ls']);
});

test('the text is kept exactly (nothing is lost or reordered)', () => {
  const cmd = 'cd /x && echo "a b" ; cp $A/f "$E"/ 2>&1 & cat <<< hi # "#" x\necho "#" # x';
  const out = [];
  const context = vm.createContext({
    document: { createDocumentFragment: () => ({ append: n => out.push(n) }), createTextNode: text => ({ text }) },
    h: (_t, a) => ({ text: a.text }),
  });
  vm.runInContext(`${code}; globalThis.shellView = shellView;`, context);
  context.shellView(cmd);
  assert.equal(out.map(n => n.text).join(''), cmd);
});

test('# never hides a command: nothing after it is dimmed', () => {
  assert.ok(run('echo hi # note\nrm x').includes('sh-risk:rm'));
  assert.ok(run('echo "a"#;rm -rf x').includes('sh-risk:rm'));
  assert.ok(run("sh -c 'a\n# b'; rm -rf /").includes('sh-risk:rm'));
  assert.ok(run('echo a\\ #; rm x').includes('sh-risk:rm'));
});

test('heredoc bodies are never dimmed, so a command fed to bash stays visible', () => {
  const got = run("bash <<'EOF'\nrm -rf /\nEOF");
  assert.ok(got.includes('sh-risk:rm'));
  assert.ok(!got.some(x => x.startsWith('sh-doc')));
});
