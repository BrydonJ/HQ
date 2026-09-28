'use strict';
// Pure rendering helpers: markdown, tool rows, diffs. No session state lives here.

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      if (v == null || v === false) continue;
      if (k === 'class') el.className = v;
      else if (k === 'text') el.textContent = v;
      else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
      else el.setAttribute(k, v === true ? '' : v);
    }
  }
  for (const c of children.flat()) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

function icon(name) {
  return h('span', { class: `codicon codicon-${name}` });
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

// Model output can quote file contents, so raw HTML is always shown as text, images are
// never fetched, and only http(s) links are kept. The CSP in tauri.conf.json is the backstop.
const markdown = new marked.Marked({ gfm: true, breaks: false });
markdown.use({
  renderer: {
    html({ text }) {
      return escapeHtml(text);
    },
    image({ href, text }) {
      return escapeHtml(`![${text || ''}](${href || ''})`);
    },
    link({ href, tokens }) {
      const inner = this.parser.parseInline(tokens);
      if (!/^https?:\/\//i.test(href || '')) return inner;
      return `<a href="${escapeHtml(href)}" data-external="1" title="${escapeHtml(href)}">${inner}</a>`;
    },
  },
});

function renderMarkdown(el, text) {
  try {
    el.innerHTML = markdown.parse(text || '');
  } catch {
    el.textContent = text || '';
  }
}

// ---------- paths ----------

function normPath(p) {
  return String(p || '').replace(/\//g, '\\').replace(/\\+$/, '');
}

function relPath(p, cwd) {
  if (!p) return '';
  const np = normPath(p);
  const nc = normPath(cwd);
  if (nc && np.toLowerCase().startsWith(nc.toLowerCase() + '\\')) return np.slice(nc.length + 1);
  return np;
}

function baseName(p) {
  const np = normPath(p);
  const i = np.lastIndexOf('\\');
  return i >= 0 ? np.slice(i + 1) || np : np;
}

// ---------- tools ----------

function firstLine(s, max = 160) {
  const line = String(s ?? '').split(/\r?\n/)[0];
  return line.length > max ? line.slice(0, max) + '…' : line;
}

function toolSummary(name, input, cwd) {
  const i = input || {};
  switch (name) {
    case 'Bash':
    case 'PowerShell':
      return firstLine(i.command);
    case 'Read':
    case 'Write':
    case 'Edit':
    case 'MultiEdit':
      return relPath(i.file_path, cwd);
    case 'NotebookEdit':
      return relPath(i.notebook_path, cwd);
    case 'Grep':
      return `${i.pattern}${i.path ? '  in ' + relPath(i.path, cwd) : ''}`;
    case 'Glob':
      return `${i.pattern}${i.path ? '  in ' + relPath(i.path, cwd) : ''}`;
    case 'WebFetch':
      return i.url;
    case 'WebSearch':
      return i.query;
    case 'Task':
    case 'Agent':
      return i.description || i.subagent_type || '';
    case 'TodoWrite':
      return `${(i.todos || []).length} items`;
    case 'AskUserQuestion':
      return (i.questions || []).map(q => q.question).join(' · ');
    case 'Skill':
      return i.skill || i.command || '';
    default: {
      const s = Object.values(i).find(v => typeof v === 'string');
      return s ? firstLine(s) : '';
    }
  }
}

function diffView(oldText, newText) {
  const box = h('div', { class: 'diff' });
  for (const line of String(oldText ?? '').split('\n')) box.append(h('div', { class: 'del', text: '- ' + line }));
  for (const line of String(newText ?? '').split('\n')) box.append(h('div', { class: 'add', text: '+ ' + line }));
  return box;
}

function todoList(todos) {
  const iconFor = { completed: 'pass-filled', in_progress: 'circle-large-filled', pending: 'circle-large-outline' };
  return h('ul', { class: 'todos' },
    (todos || []).map(t => h('li', { class: t.status }, icon(iconFor[t.status] || 'circle-large-outline'), t.content || t.activeForm || '')));
}

/** The "IN" part of a tool row: the most readable view of what the tool was asked to do. */
function toolInputView(name, input) {
  const i = input || {};
  if (name === 'Edit') return diffView(i.old_string, i.new_string);
  if (name === 'MultiEdit') return h('div', null, (i.edits || []).map(e => diffView(e.old_string, e.new_string)));
  if (name === 'Write') return h('pre', { text: i.content ?? '' });
  if (name === 'Bash' || name === 'PowerShell') return h('pre', { text: i.command ?? '' });
  if (name === 'TodoWrite') return todoList(i.todos);
  return h('pre', { text: JSON.stringify(i, null, 2) });
}

function toolResultText(content) {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) {
    return content.map(c => (c.type === 'text' ? c.text : c.type === 'image' ? '[image]' : `[${c.type}]`)).join('\n');
  }
  return content == null ? '' : JSON.stringify(content, null, 2);
}

function toolRow(label, body, cls) {
  return h('div', { class: `tool-row ${cls || ''}` }, h('div', { class: 'label', text: label }), body);
}
