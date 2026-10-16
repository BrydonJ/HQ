'use strict';
// Pure rendering helpers: markdown, tool rows, diffs. No session state lives here.

// ---------- platform ----------

const IS_MAC = /^Mac/i.test(navigator.platform || '') || /Mac OS X/.test(navigator.userAgent || '');
const FILE_MANAGER = IS_MAC ? 'Finder' : 'Explorer';

/** Shortcut text for the platform: `Ctrl+Shift+M` reads ⇧⌘M on a Mac, where Cmd takes Ctrl's place. */
function keyLabel(text) {
  return IS_MAC ? text.replace(/Ctrl\+Shift\+/g, '⇧⌘').replace(/Ctrl\+/g, '⌘') : text;
}

// The page's own shortcut labels and wording are written for Windows; fix them once at load.
if (IS_MAC) {
  const fix = root => {
    for (const el of root.querySelectorAll('[title]')) el.title = keyLabel(el.title).replace('Explorer', FILE_MANAGER);
    for (const el of root.querySelectorAll('.empty-keys')) el.textContent = keyLabel(el.textContent);
  };
  fix(document);
  for (const t of document.querySelectorAll('template')) fix(t.content);
}

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
    code({ text, lang }) {
      return codeBlock(text || '', (lang || '').trim().split(/\s+/)[0]);
    },
    link({ href, tokens }) {
      const inner = this.parser.parseInline(tokens);
      if (!/^https?:\/\//i.test(href || '')) return inner;
      return `<a href="${escapeHtml(href)}" data-external="1" title="${escapeHtml(href)}">${inner}</a>`;
    },
  },
});

// Fence languages that get a Preview button (Markdown) or a Run button (a command for a terminal).
const MARKDOWN_LANGS = new Set(['md', 'markdown', 'mdx']);
const SHELL_LANGS = new Set(['bash', 'sh', 'shell', 'zsh', 'console', 'terminal', 'powershell', 'ps1', 'pwsh', 'ps', 'cmd', 'bat', 'batch']);

/**
 * A fenced code block with its language and a copy button (handled in app.js). Code tagged with
 * a language highlight.js knows is coloured; untagged code is not guessed at, since a block that
 * is still streaming would change colours as it grows. Markdown gets a Preview button, and
 * shell languages a Run button that opens a terminal running the code (also in app.js).
 */
function codeBlock(code, lang) {
  const known = lang && window.hljs && hljs.getLanguage(lang) ? lang : '';
  let body = escapeHtml(code);
  if (known) {
    try {
      body = hljs.highlight(code, { language: known, ignoreIllegals: true }).value;
    } catch { /* shown plain */ }
  }
  const cls = known ? ` class="hljs language-${escapeHtml(known.replace(/[^\w+#-]/g, ''))}"` : '';
  const key = (lang || '').toLowerCase();
  const preview = MARKDOWN_LANGS.has(key)
    ? '<button class="code-act code-preview" title="Preview the Markdown" tabindex="-1"><span class="codicon codicon-open-preview"></span></button>' : '';
  const shell = IS_MAC ? 'Terminal' : /^(cmd|bat|batch)$/.test(key) ? 'cmd' : 'PowerShell';
  // A Mac has no cmd; PowerShell runs only if pwsh is installed (the app says so when it is not).
  const run = SHELL_LANGS.has(key) && code.trim() && !(IS_MAC && /^(cmd|bat|batch)$/.test(key))
    ? `<button class="code-act code-run" title="Run in ${shell}, in a new terminal in this session's folder" tabindex="-1"><span class="codicon codicon-play"></span></button>` : '';
  return `<div class="code-block" data-lang="${escapeHtml(key)}"><div class="code-head"><span class="code-lang">${escapeHtml(lang || 'text')}</span>`
    + `${preview}${run}<button class="code-copy" title="Copy code" tabindex="-1"><span class="codicon codicon-copy"></span></button></div>`
    + `<pre><code${cls}>${body}</code></pre></div>`;
}

function renderMarkdown(el, text) {
  el._src = text || ''; // kept so a reply can be copied or exported as the Markdown it was written in
  try {
    el.innerHTML = markdown.parse(text || '');
  } catch {
    el.textContent = text || '';
  }
}

/**
 * Renders text that is still growing without redoing the finished part. The text is lexed into
 * top-level blocks (cheap); every block but the last is treated as finished and keeps its DOM
 * nodes while its source text is unchanged, so only the last block is rendered each time, and
 * only the text from the last finished block on is lexed again.
 * `state` belongs to one element and starts as `{}`.
 */
function renderMarkdownLive(el, text, state) {
  el._src = text || '';
  text = text || '';
  const items = state.items || (state.items = []);
  state.done = state.done || ''; // the source text of the finished blocks
  // Lex again from the last finished block, so one that grows into the next (a list) is redrawn.
  let keep = Math.max(0, items.length - 1);
  let offset = keep ? items[keep - 1].end : 0;
  if (offset && text.slice(0, offset) !== state.done.slice(0, offset)) { keep = 0; offset = 0; } // earlier text changed
  let tokens;
  try {
    tokens = markdown.lexer(text.slice(offset));
  } catch {
    state.items = null;
    state.done = '';
    renderMarkdown(el, text);
    return;
  }
  let k = 0;
  while (keep + k < items.length && k < tokens.length - 1 && items[keep + k].raw === tokens[k].raw) k++;
  const idx = keep + k;
  for (const item of items.splice(idx)) for (const node of item.nodes) node.remove();
  state.done = state.done.slice(0, idx ? items[idx - 1].end : 0);
  const render = token => {
    const box = document.createElement('template');
    box.innerHTML = markdown.parser([token]);
    return [...box.content.childNodes];
  };
  for (let i = k; i < tokens.length - 1; i++) {
    const nodes = render(tokens[i]);
    el.append(...nodes);
    items.push({ raw: tokens[i].raw, nodes, end: (items.length ? items[items.length - 1].end : 0) + tokens[i].raw.length });
    state.done += tokens[i].raw;
  }
  // The last block is replaced every time.
  for (const node of state.tail || []) node.remove();
  const last = tokens[tokens.length - 1];
  state.tail = last ? render(last) : [];
  // Marked as still being written (a code block's Run button waits for the final render).
  for (const node of state.tail) if (node.nodeType === 1) node.classList.add('streaming');
  el.append(...state.tail);
}

// ---------- long text ----------

const LONG_LINES = 400;
const LONG_CHARS = 40000;

/** A `<pre>` that shows the start of very long text, with a button that shows the rest. */
function longPre(text, cls) {
  const full = String(text ?? '');
  const pre = h('pre', { class: cls || null });
  const lines = full.split('\n');
  if (lines.length <= LONG_LINES && full.length <= LONG_CHARS) {
    pre.textContent = full;
    return pre;
  }
  const head = lines.slice(0, LONG_LINES).join('\n').slice(0, LONG_CHARS);
  pre.textContent = head;
  const more = lines.length > LONG_LINES ? `${lines.length - LONG_LINES} more lines` : `${full.length - head.length} more characters`;
  const box = h('div', { class: 'long' }, pre);
  const button = h('button', { class: 'btn long-more', text: `Show all (${more})` });
  button.addEventListener('click', () => { pre.textContent = full; button.remove(); });
  box.append(button);
  return box;
}

// ---------- paths ----------

function normPath(p) {
  return String(p || '').replace(/\//g, '\\').replace(/\\+$/, '');
}

/** The interpreter a Mac runs a shell block with: its own language when it names one, else zsh. */
function macShell(lang) {
  if (/^(ps|ps1|pwsh|powershell)$/.test(lang)) return 'powershell';
  return /^(bash|sh|zsh)$/.test(lang) ? lang : 'zsh';
}

/** A path in the form `like` is written in: slashes for a Unix path, backslashes otherwise. */
function nativeSep(p, like) {
  return String(like || '').startsWith('/') ? p.replace(/\\/g, '/') : p;
}

function relPath(p, cwd) {
  if (!p) return '';
  const np = normPath(p);
  const nc = normPath(cwd);
  if (nc && np.toLowerCase().startsWith(nc.toLowerCase() + '\\')) return nativeSep(np.slice(nc.length + 1), p);
  return nativeSep(np, p);
}

function baseName(p) {
  const np = normPath(p);
  const i = np.lastIndexOf('\\');
  return i >= 0 ? np.slice(i + 1) || np : np;
}

/**
 * A stable color per project folder, so cells and list entries for the same folder match.
 * Soft pastels that avoid the amber, green and red that the status cues use. A folder's colour
 * can be shuffled (click the project name); the picks live in `projectColorPicks`, keyed by
 * lower-cased folder, and are saved with the board.
 */
const PROJECT_COLORS = ['#8ab4f8', '#8fd3c7', '#c3a6e8', '#f2a7c3', '#f5b78e', '#e6d38f',
  '#a5c8e4', '#b6a8f0', '#93d0d8', '#f0a6a0', '#d7b8e3', '#c9b79c'];
let projectColorPicks = {};

function projectColor(path) {
  const key = normPath(path).toLowerCase();
  if (Object.hasOwn(projectColorPicks, key)) return projectColorPicks[key];
  let hash = 0x811c9dc5; // FNV-1a
  for (const ch of normPath(path).toLowerCase()) {
    hash ^= ch.charCodeAt(0);
    hash = Math.imul(hash, 0x01000193);
  }
  return PROJECT_COLORS[(hash >>> 0) % PROJECT_COLORS.length];
}

// ---------- images ----------

const IMAGE_TYPES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp'];
// 2000px is the API's per-image limit once a request holds many images; 3.75 MB of raw bytes
// stays under the 5 MB base64 cap.
const MAX_IMAGE_EDGE = 2000;
const MAX_IMAGE_BYTES = 3.75 * 1024 * 1024;

function readAsDataUrl(blob) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(blob);
  });
}

function loadImage(url) {
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error('not a readable image'));
    img.src = url;
  });
}

/** Reads an image file into an API image block, re-encoding it when it is too big or an unsupported type. */
async function prepareImage(file) {
  let url = await readAsDataUrl(file);
  const img = await loadImage(url);
  let type = file.type;
  const edge = Math.max(img.naturalWidth, img.naturalHeight);
  if (!IMAGE_TYPES.includes(type) || edge > MAX_IMAGE_EDGE || file.size > MAX_IMAGE_BYTES) {
    const scale = Math.min(1, MAX_IMAGE_EDGE / edge);
    const canvas = document.createElement('canvas');
    canvas.width = Math.max(1, Math.round(img.naturalWidth * scale));
    canvas.height = Math.max(1, Math.round(img.naturalHeight * scale));
    canvas.getContext('2d').drawImage(img, 0, 0, canvas.width, canvas.height);
    type = type === 'image/jpeg' ? 'image/jpeg' : 'image/png';
    url = canvas.toDataURL(type, 0.9);
    if (url.length * 0.75 > MAX_IMAGE_BYTES) {
      type = 'image/jpeg';
      url = canvas.toDataURL(type, 0.85);
    }
  }
  return { mediaType: type, data: url.slice(url.indexOf(',') + 1), url };
}

/** A data: URL for an image block from a transcript, or null when the block is not a plain base64 image. */
function imageBlockUrl(source) {
  if (!source || source.type !== 'base64' || !IMAGE_TYPES.includes(source.media_type)) return null;
  // The backend blanks the data of transcript images, so they cost nothing to load; see imageNote().
  if (typeof source.data !== 'string' || !source.data || !/^[A-Za-z0-9+/=]*$/.test(source.data)) return null;
  return `data:${source.media_type};base64,${source.data}`;
}

/** Stands in for an image of a restored transcript, which is not loaded. */
function imageNote() {
  return h('span', { class: 'image-note', text: '[image omitted]' });
}

function userBubble(text, imageUrls, ms, omitted = 0) {
  return h('div', { class: 'item user' },
    h('div', { class: 'bubble' },
      stamp(ms),
      (imageUrls && imageUrls.length) || omitted
        ? h('div', { class: 'bubble-images' }, (imageUrls || []).map(src => h('img', { class: 'thumb', src, alt: 'image' })), Array.from({ length: omitted }, imageNote))
        : null,
      text ? h('div', { text }) : null));
}

/** A small time in the corner of a prompt or reply; the date is added once it is not today. Null for a bad time. */
function stamp(ms) {
  if (!Number.isFinite(ms)) return null;
  const d = new Date(ms);
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  const text = d.toDateString() === new Date().toDateString() ? time : `${d.toLocaleDateString([], { month: 'short', day: 'numeric' })} ${time}`;
  return h('span', { class: 'stamp', text, title: d.toLocaleString() });
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

const DIFF_CONTEXT = 2;
const DIFF_MAX_CELLS = 1_500_000; // above this the line diff costs too much; the whole edit shows as removed and added

/** A line diff as [' ' | '-' | '+', line] pairs: common ends are peeled off, the middle is an LCS table. */
function diffLines(oldText, newText) {
  const a = String(oldText ?? '').split('\n');
  const b = String(newText ?? '').split('\n');
  let head = 0;
  while (head < a.length && head < b.length && a[head] === b[head]) head++;
  let tail = 0;
  while (tail < a.length - head && tail < b.length - head && a[a.length - 1 - tail] === b[b.length - 1 - tail]) tail++;
  const x = a.slice(head, a.length - tail);
  const y = b.slice(head, b.length - tail);
  const ops = a.slice(0, head).map(s => [' ', s]);
  const n = x.length;
  const m = y.length;
  if (!n || !m || n * m > DIFF_MAX_CELLS) {
    for (const s of x) ops.push(['-', s]);
    for (const s of y) ops.push(['+', s]);
  } else {
    // lcs[i][j]: the longest common run of x[i..] and y[j..]
    const w = m + 1;
    const lcs = new Int32Array((n + 1) * w);
    for (let i = n - 1; i >= 0; i--) {
      for (let j = m - 1; j >= 0; j--) {
        lcs[i * w + j] = x[i] === y[j] ? lcs[(i + 1) * w + j + 1] + 1 : Math.max(lcs[(i + 1) * w + j], lcs[i * w + j + 1]);
      }
    }
    let i = 0;
    let j = 0;
    while (i < n && j < m) {
      if (x[i] === y[j]) { ops.push([' ', x[i]]); i++; j++; }
      else if (lcs[(i + 1) * w + j] >= lcs[i * w + j + 1]) ops.push(['-', x[i++]]);
      else ops.push(['+', y[j++]]);
    }
    while (i < n) ops.push(['-', x[i++]]);
    while (j < m) ops.push(['+', y[j++]]);
  }
  for (const s of a.slice(a.length - tail)) ops.push([' ', s]);
  return ops;
}

/** Rows for a diff: changed lines, with a few unchanged lines around them and a note for the rest. */
function diffRows(ops) {
  const near = new Array(ops.length).fill(false);
  ops.forEach(([t], i) => {
    if (t === ' ') return;
    for (let k = Math.max(0, i - DIFF_CONTEXT); k <= Math.min(ops.length - 1, i + DIFF_CONTEXT); k++) near[k] = true;
  });
  const rows = [];
  for (let i = 0; i < ops.length; i++) {
    const [t, s] = ops[i];
    if (!near[i] && t === ' ') {
      let j = i;
      while (j < ops.length && !near[j] && ops[j][0] === ' ') j++;
      rows.push(h('div', { class: 'skip', text: `⋯ ${j - i} unchanged line${j - i === 1 ? '' : 's'}` }));
      i = j - 1;
      continue;
    }
    const cls = t === '-' ? 'del' : t === '+' ? 'add' : 'ctx';
    rows.push(h('div', { class: cls, text: `${t === ' ' ? '  ' : t + ' '}${s}` }));
  }
  return rows;
}

function diffView(oldText, newText) {
  const rows = diffRows(diffLines(oldText, newText));
  const box = h('div', { class: 'diff' }, rows.slice(0, 600));
  if (rows.length > 600) box.append(h('div', { class: 'skip', text: `⋯ ${rows.length - 600} more lines` }));
  return box;
}

/** The structured patch Claude Code reports for an Edit: hunks of ' ', '-' and '+' lines with real line numbers. */
function patchView(patch) {
  const hunks = (Array.isArray(patch) ? patch : []).filter(p => p && Array.isArray(p.lines));
  if (!hunks.length) return null;
  const box = h('div', { class: 'diff' });
  hunks.forEach((hunk, k) => {
    if (k) box.append(h('div', { class: 'skip', text: '⋯' }));
    let oldNo = Number(hunk.oldStart) || 0;
    let newNo = Number(hunk.newStart) || 0;
    for (const raw of hunk.lines.slice(0, 600)) {
      if (raw[0] === '\\') { box.append(h('div', { class: 'skip', text: raw.slice(1).trim() })); continue; } // "\ No newline at end of file"
      const t = raw[0] === '-' || raw[0] === '+' ? raw[0] : ' ';
      const no = t === '+' ? newNo : oldNo;
      if (t !== '+') oldNo++;
      if (t !== '-') newNo++;
      box.append(h('div', { class: t === '-' ? 'del' : t === '+' ? 'add' : 'ctx' },
        h('span', { class: 'ln', text: no ? String(no) : '' }), `${t === ' ' ? '  ' : t + ' '}${raw.slice(1)}`));
    }
  });
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
  if (name === 'Write') return longPre(i.content);
  if (name === 'Bash' || name === 'PowerShell') return h('pre', { text: i.command ?? '' });
  if (name === 'TodoWrite') return todoList(i.todos);
  return longPre(JSON.stringify(i, null, 2));
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
