'use strict';
// Board, cells and the stream-json protocol. Every message is parsed tolerantly: unknown
// message types and fields are ignored, so Claude Code updates degrade to "not shown"
// instead of breaking the app.

const { invoke, Channel } = window.__TAURI__.core;

const $ = sel => document.querySelector(sel);
const grid = $('#grid');
const board = $('#board');
const MAX_RECENT = 20;

const app = {
  cells: [],
  focused: null,
  maximized: null,
  recent: [],
};

let reqSeq = 0;
const nextRequestId = () => `hq-${Date.now().toString(36)}-${++reqSeq}`;

// ======================================================================================
// Cell: one chat session bound to one folder
// ======================================================================================

class Cell {
  constructor({ cwd, sessionId = null, permissionMode = 'default', model = null }) {
    this.cwd = cwd;
    this.sessionId = sessionId;
    this.permissionMode = permissionMode || 'default';
    this.model = model;
    this.proc = null;          // backend process id while claude is running
    this.spawning = null;      // promise while a spawn is in flight
    this.status = 'idle';      // idle | running | exited
    this.unread = false;
    this.pending = new Map();  // control request_id -> prompt element
    this.controls = new Map(); // our control request_id -> resolver awaiting Claude's reply
    this.tools = new Map();    // tool_use_id -> { item, body }
    this.live = null;          // { msgId, blocks: Map<index, {item, md, text}> } for streamed text
    this.renderTimer = null;
    this.stickToBottom = true;
    this.closing = false;
    this.buildDom();
  }

  // ---------- DOM ----------

  buildDom() {
    const el = $('#cell-template').content.firstElementChild.cloneNode(true);
    this.el = el;
    this.msgs = el.querySelector('.messages');
    this.prompts = el.querySelector('.prompts');
    this.input = el.querySelector('textarea');
    this.modeSel = el.querySelector('.mode');
    this.modelEl = el.querySelector('.model');
    this.metaEl = el.querySelector('.meta');
    this.stopBtn = el.querySelector('.act-stop');

    el.querySelector('.cell-title').textContent = baseName(this.cwd) || this.cwd;
    const pathEl = el.querySelector('.cell-path');
    pathEl.textContent = this.cwd;
    pathEl.title = this.cwd;
    this.modeSel.value = this.permissionMode;
    this.modelEl.textContent = this.model || '';

    el.addEventListener('mousedown', () => focusCell(this, false));
    el.querySelector('.cell-head').addEventListener('dblclick', e => {
      if (!e.target.closest('button')) toggleMaximize(this);
    });
    el.querySelector('.act-max').addEventListener('click', () => toggleMaximize(this));
    el.querySelector('.act-close').addEventListener('click', e => this.requestClose(e.currentTarget));
    el.querySelector('.act-reveal').addEventListener('click', () => invoke('reveal_folder', { path: this.cwd }).catch(err => this.notice(String(err), true)));
    el.querySelector('.act-code').addEventListener('click', () => invoke('open_in_vscode', { path: this.cwd }).catch(err => this.notice(String(err), true)));
    el.querySelector('.act-send').addEventListener('click', () => this.submit());
    this.stopBtn.addEventListener('click', () => this.interrupt());
    this.modeSel.addEventListener('change', () => this.setMode(this.modeSel.value));

    this.input.addEventListener('keydown', e => {
      if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
        e.preventDefault();
        this.submit();
      }
    });
    this.input.addEventListener('input', () => autoGrow(this.input));
    this.msgs.addEventListener('scroll', () => {
      const m = this.msgs;
      this.stickToBottom = m.scrollHeight - m.scrollTop - m.clientHeight < 40;
    });
    this.msgs.addEventListener('click', onLinkClick);
    this.prompts.addEventListener('click', onLinkClick);
  }

  append(node, parent) {
    (parent || this.msgs).append(node);
    this.scroll();
    return node;
  }

  scroll() {
    if (this.stickToBottom) this.msgs.scrollTop = this.msgs.scrollHeight;
  }

  notice(text, isError) {
    return this.append(h('div', { class: `item notice${isError ? ' error' : ''}`, text }));
  }

  // ---------- state ----------

  setStatus(status) {
    this.status = status;
    this.refreshState();
  }

  refreshState() {
    const waiting = this.pending.size > 0;
    const el = this.el;
    el.classList.toggle('state-running', this.status === 'running' && !waiting);
    el.classList.toggle('state-waiting', waiting);
    el.classList.toggle('state-exited', this.status === 'exited');
    el.classList.toggle('unread', this.unread && !waiting);
    this.stopBtn.classList.toggle('hidden', this.status !== 'running' && !waiting);
    updateSummary();
  }

  markUnread() {
    const seen = document.hasFocus() && app.focused === this && !document.hidden;
    if (!seen) {
      this.unread = true;
      invoke('request_attention').catch(() => {});
    }
  }

  // ---------- process ----------

  async ensureProcess() {
    if (this.proc != null) return true;
    if (this.spawning) return this.spawning;
    this.spawning = (async () => {
      const channel = new Channel();
      channel.onmessage = line => this.onLine(line);
      try {
        this.proc = await invoke('spawn_session', {
          cwd: this.cwd,
          resume: this.sessionId,
          permissionMode: this.permissionMode,
          model: null,
          onEvent: channel,
        });
      } catch (err) {
        this.notice(String(err), true);
        this.setStatus('exited');
        return false;
      } finally {
        this.spawning = null;
      }
      if (this.closing) { // cell was closed while claude was starting
        invoke('kill_session', { id: this.proc });
        this.proc = null;
        return false;
      }
      this.control({ subtype: 'initialize' });
      return true;
    })();
    return this.spawning;
  }

  write(obj) {
    if (this.proc == null) return Promise.resolve();
    return invoke('send_line', { id: this.proc, line: JSON.stringify(obj) }).catch(err => this.notice(String(err), true));
  }

  /** Sends a control request; resolves with Claude's control_response (or null if the process ends). */
  control(request) {
    const requestId = nextRequestId();
    const reply = new Promise(resolve => this.controls.set(requestId, resolve));
    this.write({ type: 'control_request', request_id: requestId, request });
    return reply;
  }

  respond(requestId, response) {
    return this.write({ type: 'control_response', response: { subtype: 'success', request_id: requestId, response } });
  }

  async submit() {
    const text = this.input.value.trim();
    if (!text) return;
    this.input.value = '';
    autoGrow(this.input);
    this.stickToBottom = true;
    this.append(h('div', { class: 'item user' }, h('div', { class: 'bubble', text })));
    this.unread = false;
    this.setStatus('running');
    if (!(await this.ensureProcess())) return;
    this.write({ type: 'user', message: { role: 'user', content: text }, parent_tool_use_id: null });
  }

  interrupt() {
    if (this.proc == null) return;
    this.control({ subtype: 'interrupt' });
  }

  async setMode(mode) {
    const previous = this.permissionMode;
    this.permissionMode = mode;
    this.modeSel.value = mode;
    saveState();
    if (this.proc == null) return; // applied with --permission-mode on the next spawn
    const reply = await this.control({ subtype: 'set_permission_mode', mode });
    if (reply && reply.subtype === 'error') {
      this.notice(`Could not switch mode: ${reply.error || 'rejected by Claude'}`, true);
      this.permissionMode = previous;
      this.modeSel.value = previous;
      saveState();
    }
  }

  requestClose(btn) {
    const busy = this.status === 'running' || this.pending.size > 0;
    if (busy && !btn.classList.contains('confirm')) {
      btn.classList.add('confirm');
      btn.title = 'Claude is working. Click again to close.';
      btn.style.color = 'var(--error)';
      setTimeout(() => { btn.classList.remove('confirm'); btn.style.color = ''; btn.title = 'Close session'; }, 3000);
      return;
    }
    closeCell(this);
  }

  dispose() {
    this.closing = true;
    if (this.proc != null) invoke('kill_session', { id: this.proc });
    this.proc = null;
    clearTimeout(this.renderTimer);
  }

  // ---------- protocol ----------

  onLine(line) {
    let m;
    try { m = JSON.parse(line); } catch { return; }
    if (this.closing) return;
    try {
      this.handle(m, false);
    } catch (err) {
      console.error('render failed', err, m);
    }
  }

  handle(m, replay) {
    switch (m.type) {
      case 'system': return this.onSystem(m);
      case 'stream_event': return this.onStreamEvent(m);
      case 'assistant': return this.onAssistant(m, replay);
      case 'user': return this.onUser(m, replay);
      case 'result': return this.onResult(m);
      case 'control_request': return this.onControlRequest(m);
      case 'control_cancel_request': return this.dropPrompt(m.request_id);
      case 'control_response': return this.onControlResponse(m);
      case 'hq_exit': return this.onExit(m);
      default: return; // rate_limit_event, keep_alive, future types
    }
  }

  onSystem(m) {
    if (m.subtype === 'init') {
      const changed = this.sessionId !== m.session_id;
      this.sessionId = m.session_id || this.sessionId;
      if (m.model) { this.model = m.model; this.modelEl.textContent = m.model; }
      if (m.permissionMode && m.permissionMode !== this.permissionMode) {
        this.permissionMode = m.permissionMode;
        this.modeSel.value = m.permissionMode;
      }
      if (changed) saveState();
      if (this.status !== 'running') this.setStatus('running');
    } else if (m.subtype === 'compact_boundary') {
      this.notice('Conversation compacted');
    }
  }

  onStreamEvent(m) {
    if (m.parent_tool_use_id) return; // subagent chatter is shown when its messages complete
    const ev = m.event || {};
    if (this.status !== 'running') this.setStatus('running');
    if (ev.type === 'message_start') {
      this.live = { msgId: ev.message && ev.message.id, blocks: new Map() };
    } else if (ev.type === 'content_block_start' && this.live) {
      if (ev.content_block && ev.content_block.type === 'text') {
        const md = h('div', { class: 'md streaming' });
        const item = this.append(h('div', { class: 'item text' }, md));
        this.live.blocks.set(ev.index, { item, md, text: ev.content_block.text || '' });
      }
    } else if (ev.type === 'content_block_delta' && this.live) {
      const block = this.live.blocks.get(ev.index);
      if (block && ev.delta && ev.delta.type === 'text_delta') {
        block.text += ev.delta.text || '';
        this.scheduleLiveRender();
      }
    }
  }

  scheduleLiveRender() {
    if (this.renderTimer) return;
    this.renderTimer = setTimeout(() => {
      this.renderTimer = null;
      if (!this.live) return;
      for (const b of this.live.blocks.values()) {
        if (b.md.classList.contains('streaming')) renderMarkdown(b.md, b.text);
      }
      this.scroll();
    }, 50);
  }

  /** Swaps the oldest still-streaming block of this message for its final text. */
  takeLiveText(msgId) {
    if (!this.live || this.live.msgId !== msgId) return null;
    for (const [idx, b] of this.live.blocks) {
      if (b.md.classList.contains('streaming')) {
        this.live.blocks.delete(idx);
        b.md.classList.remove('streaming');
        return b;
      }
    }
    return null;
  }

  finishLive() {
    if (!this.live) return;
    for (const b of this.live.blocks.values()) {
      b.md.classList.remove('streaming');
      renderMarkdown(b.md, b.text);
    }
    this.live = null;
  }

  containerFor(parentToolUseId) {
    if (!parentToolUseId) return null;
    const t = this.tools.get(parentToolUseId);
    if (!t) return null;
    if (!t.sub) {
      t.sub = h('div', { class: 'subagent' });
      t.item.append(t.sub);
    }
    return t.sub;
  }

  onAssistant(m, replay) {
    const msg = m.message || {};
    const parent = this.containerFor(m.parent_tool_use_id);
    for (const block of msg.content || []) {
      if (block.type === 'text') {
        if (!block.text) continue;
        const live = !replay && !parent ? this.takeLiveText(msg.id) : null;
        if (live) {
          renderMarkdown(live.md, block.text);
        } else {
          const md = h('div', { class: 'md' });
          renderMarkdown(md, block.text);
          this.append(h('div', { class: 'item text' }, md), parent);
        }
      } else if (block.type === 'thinking') {
        if (!block.thinking) continue;
        const md = h('div', { class: 'md' });
        renderMarkdown(md, block.thinking);
        this.append(h('details', { class: 'item thinking' }, h('summary', { text: 'Thinking' }), md), parent);
      } else if (block.type === 'tool_use' || block.type === 'server_tool_use') {
        this.addTool(block, parent);
      }
    }
  }

  addTool(block, parent) {
    const name = block.name || 'tool';
    const summary = h('summary', null,
      h('span', { class: 'tname', text: name }),
      h('span', { class: 'targ', text: toolSummary(name, block.input, this.cwd) }));
    const body = h('div', { class: 'tool-body' }, toolRow('in', toolInputView(name, block.input)));
    const item = h('details', { class: 'item tool pending' }, summary, body);
    if (name === 'TodoWrite') item.open = true;
    this.tools.set(block.id, { item, body, name });
    this.append(item, parent);
  }

  onUser(m, replay) {
    const msg = m.message || {};
    if (m.isMeta || m.isCompactSummary) {
      if (m.isCompactSummary) this.notice('Conversation compacted');
      return;
    }
    const parent = this.containerFor(m.parent_tool_use_id);
    const content = typeof msg.content === 'string' ? [{ type: 'text', text: msg.content }] : msg.content || [];
    for (const block of content) {
      if (block.type === 'tool_result') {
        const t = this.tools.get(block.tool_use_id);
        if (!t) continue;
        const text = toolResultText(block.content);
        t.item.classList.remove('pending');
        t.item.classList.add(block.is_error ? 'err' : 'ok');
        if (text) t.body.append(toolRow('out', h('pre', { text }), block.is_error ? 'err' : ''));
      } else if (block.type === 'text' && block.text) {
        const text = block.text;
        if (text.startsWith('[Request interrupted')) this.notice('Interrupted');
        else if (/^\s*<(command-|local-command-|system-reminder|task-notification)/.test(text)) continue;
        else if (replay && !parent) this.append(h('div', { class: 'item user' }, h('div', { class: 'bubble', text })));
      }
    }
  }

  onResult(m) {
    this.finishLive();
    for (const t of this.tools.values()) t.item.classList.remove('pending');
    for (const id of [...this.pending.keys()]) this.dropPrompt(id); // a finished turn has nothing left to approve
    if (m.is_error && m.subtype !== 'error_during_execution') {
      const detail = m.result || (m.errors && m.errors.join('\n')) || m.subtype;
      this.notice(`Error: ${detail}`, true);
    }
    if (typeof m.duration_ms === 'number') this.metaEl.textContent = `${(m.duration_ms / 1000).toFixed(1)}s`;
    this.setStatus('idle');
    this.markUnread();
    this.refreshState();
  }

  onControlResponse(m) {
    const response = m.response || {};
    const resolve = this.controls.get(response.request_id);
    if (!resolve) return;
    this.controls.delete(response.request_id);
    resolve(response);
  }

  onExit(m) {
    const wasRunning = this.status === 'running';
    this.proc = null;
    for (const resolve of this.controls.values()) resolve(null);
    this.controls.clear();
    this.finishLive();
    for (const id of [...this.pending.keys()]) this.dropPrompt(id);
    if (m.code !== 0 && m.code != null) {
      const tail = (m.stderr || []).slice(-8).join('\n');
      this.notice(`Claude exited with code ${m.code}${tail ? '\n' + tail : ''}`, true);
      this.setStatus('exited');
    } else {
      this.setStatus(wasRunning ? 'exited' : 'idle');
    }
  }

  // ---------- prompts: permission, questions, plan approval ----------

  onControlRequest(m) {
    const req = m.request || {};
    if (req.subtype !== 'can_use_tool') {
      // Hooks and SDK MCP servers are never registered by HQ, so refuse anything else.
      this.write({ type: 'control_response', response: { subtype: 'error', request_id: m.request_id, error: `HQ does not handle ${req.subtype}` } });
      return;
    }
    let card;
    if (req.tool_name === 'AskUserQuestion') card = this.questionCard(m.request_id, req);
    else if (req.tool_name === 'ExitPlanMode') card = this.planCard(m.request_id, req);
    else card = this.permissionCard(m.request_id, req);
    this.pending.set(m.request_id, card);
    this.prompts.append(card);
    this.unread = false;
    this.refreshState();
    invoke('request_attention').catch(() => {});
  }

  dropPrompt(requestId) {
    const card = this.pending.get(requestId);
    if (card) card.remove();
    this.pending.delete(requestId);
    this.refreshState();
  }

  answer(requestId, response) {
    this.respond(requestId, response);
    this.dropPrompt(requestId);
  }

  permissionCard(requestId, req) {
    const input = req.input || {};
    const name = req.display_name || req.tool_name;
    const title = h('div', { class: 'prompt-title' }, `Allow ${name}?`,
      req.description ? h('span', { class: 'targ', text: '  ' + req.description }) : null);
    let preview;
    if (req.tool_name === 'Edit' || req.tool_name === 'MultiEdit') {
      preview = h('div', { class: 'preview' }, h('div', { text: relPath(input.file_path, this.cwd) }), toolInputView(req.tool_name, input));
    } else if (req.tool_name === 'Bash' || req.tool_name === 'PowerShell') {
      preview = h('pre', { text: input.command || '' });
    } else if (req.tool_name === 'Write') {
      const lines = String(input.content || '').split('\n');
      const shown = lines.slice(0, 30).join('\n') + (lines.length > 30 ? `\n… ${lines.length - 30} more lines` : '');
      preview = h('div', { class: 'preview' }, h('div', { text: relPath(input.file_path, this.cwd) }), h('div', { class: 'diff' }, h('div', { class: 'add', text: shown })));
    } else {
      preview = h('pre', { text: JSON.stringify(input, null, 2) });
    }
    const reason = req.decision_reason ? h('div', { class: 'desc', text: String(req.decision_reason) }) : null;

    const feedback = h('input', { type: 'text', placeholder: 'No — tell Claude what to do instead (optional), then Enter' });
    const allow = () => this.answer(requestId, { behavior: 'allow', updatedInput: input });
    const allowAlways = () => this.answer(requestId, { behavior: 'allow', updatedInput: input, updatedPermissions: req.permission_suggestions });
    const deny = () => this.answer(requestId, { behavior: 'deny', message: feedback.value.trim() || 'The user denied this tool use.' });
    feedback.addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); deny(); } });

    const choices = [choice('1', 'Yes', null, allow)];
    const suggestions = Array.isArray(req.permission_suggestions) ? req.permission_suggestions : [];
    if (suggestions.length) choices.push(choice('2', suggestionLabel(suggestions), null, allowAlways));
    choices.push(choice(String(choices.length + 1), 'No', null, deny));

    const card = h('div', { class: 'prompt' }, title, reason, preview, h('div', { class: 'choices' }, choices), feedback);
    card.hotkeys = choices.map(c => c.run);
    return card;
  }

  questionCard(requestId, req) {
    const input = req.input || {};
    const questions = Array.isArray(input.questions) ? input.questions : [];
    const picks = questions.map(() => new Set());
    const others = [];
    const blocks = questions.map((q, qi) => {
      const buttons = (q.options || []).map((opt, oi) => {
        const b = choice(String(oi + 1), opt.label, opt.description, () => {
          if (q.multiSelect) {
            picks[qi].has(opt.label) ? picks[qi].delete(opt.label) : picks[qi].add(opt.label);
          } else {
            picks[qi].clear();
            picks[qi].add(opt.label);
            others[qi].value = '';
          }
          buttons.forEach(btn => btn.classList.toggle('selected', picks[qi].has(btn.dataset.label)));
          if (!q.multiSelect && questions.length === 1) submit();
        });
        b.dataset.label = opt.label;
        return b;
      });
      others[qi] = h('input', { type: 'text', placeholder: 'Other…' });
      others[qi].addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); submit(); } });
      return h('div', { class: 'qblock' },
        q.header ? h('div', { class: 'qhead', text: q.header }) : null,
        h('div', { class: 'prompt-title', text: q.question }),
        h('div', { class: 'choices' }, buttons),
        others[qi]);
    });
    const submit = () => {
      const answers = {};
      questions.forEach((q, qi) => {
        const other = others[qi].value.trim();
        const chosen = [...picks[qi]];
        if (other) chosen.push(other);
        answers[q.question] = chosen.join(', ');
      });
      this.answer(requestId, { behavior: 'allow', updatedInput: { ...input, answers } });
    };
    const skip = () => this.answer(requestId, { behavior: 'deny', message: 'The user declined to answer.' });
    const card = h('div', { class: 'prompt' }, blocks,
      h('div', { class: 'actions' },
        h('button', { class: 'btn primary', onclick: submit }, 'Submit'),
        h('button', { class: 'btn', onclick: skip }, 'Skip')));
    if (questions.length === 1) card.hotkeys = (questions[0].options || []).map((_, i) => () => blocks[0].querySelectorAll('.choice')[i].click());
    return card;
  }

  planCard(requestId, req) {
    const input = req.input || {};
    const plan = h('div', { class: 'preview md' });
    renderMarkdown(plan, input.plan || '');
    const feedback = h('textarea', { rows: '2', placeholder: 'Keep planning — tell Claude what to change (optional)' });
    const approve = mode => {
      this.answer(requestId, { behavior: 'allow', updatedInput: input });
      this.setMode(mode);
    };
    const keepPlanning = () => this.answer(requestId, { behavior: 'deny', message: feedback.value.trim() || 'The user wants to keep planning.' });
    const choices = [
      choice('1', 'Yes, and auto-accept edits', null, () => approve('acceptEdits')),
      choice('2', 'Yes, and manually approve edits', null, () => approve('default')),
      choice('3', 'No, keep planning', null, keepPlanning),
    ];
    const card = h('div', { class: 'prompt' }, h('div', { class: 'prompt-title', text: 'Ready to code? Here is Claude\'s plan:' }), plan, h('div', { class: 'choices' }, choices), feedback);
    card.hotkeys = choices.map(c => c.run);
    return card;
  }

  // ---------- history ----------

  async loadHistory() {
    if (!this.sessionId) return;
    let lines = [];
    try {
      lines = await invoke('load_transcript', { sessionId: this.sessionId });
    } catch (err) {
      this.notice(`Could not load history: ${err}`, true);
    }
    const prevStick = this.stickToBottom;
    this.stickToBottom = false;
    for (const line of lines) {
      try { this.handle(JSON.parse(line), true); } catch { /* skip malformed line */ }
    }
    for (const t of this.tools.values()) t.item.classList.remove('pending');
    this.stickToBottom = prevStick;
    this.notice(lines.length ? 'Session restored. Claude resumes when you send a message.' : 'Session restored (no history found).');
    this.msgs.scrollTop = this.msgs.scrollHeight;
  }
}

function choice(key, label, desc, run) {
  const b = h('button', { class: 'choice', onclick: run },
    h('span', { class: 'key', text: key }),
    h('span', null, h('span', { text: label }), desc ? h('div', { class: 'desc', text: desc }) : null));
  b.run = run;
  return b;
}

function suggestionLabel(suggestions) {
  for (const s of suggestions) {
    if (s.type === 'setMode' && s.mode === 'acceptEdits') return 'Yes, allow all edits this session';
    if (s.type === 'addDirectories') return `Yes, and allow access to ${(s.directories || []).join(', ')}`;
    if (s.type === 'addRules' && Array.isArray(s.rules)) {
      const rules = s.rules.map(r => (r.ruleContent ? `${r.toolName}(${r.ruleContent})` : r.toolName)).join(', ');
      const where = s.destination === 'session' ? ' this session' : s.destination === 'localSettings' ? ' in this project' : '';
      return `Yes, and don't ask again for ${rules}${where}`;
    }
  }
  return "Yes, and don't ask again";
}

function autoGrow(ta) {
  ta.style.height = 'auto';
  ta.style.height = Math.min(ta.scrollHeight, 200) + 'px';
}

function onLinkClick(e) {
  const a = e.target.closest('a');
  if (!a) return;
  e.preventDefault();
  if (a.dataset.external) invoke('open_url', { url: a.getAttribute('href') }).catch(() => {});
}

// ======================================================================================
// Board: add / remove / focus / layout
// ======================================================================================

function addCell(opts, { restore = false } = {}) {
  const cell = new Cell(opts);
  app.cells.push(cell);
  layout();
  focusCell(cell, true);
  // Claude starts on the first message, so an empty cell costs no claude process.
  if (restore) cell.loadHistory();
  else rememberFolder(cell.cwd);
  saveState();
  return cell;
}

function closeCell(cell) {
  cell.dispose();
  const i = app.cells.indexOf(cell);
  if (i >= 0) app.cells.splice(i, 1);
  cell.el.remove();
  if (app.maximized === cell) app.maximized = null;
  if (app.focused === cell) {
    app.focused = null;
    const next = app.cells[Math.min(i, app.cells.length - 1)];
    if (next) focusCell(next, true);
  }
  layout();
  updateSummary();
  saveState();
}

function focusCell(cell, focusInput) {
  if (app.focused && app.focused !== cell) app.focused.el.classList.remove('focused');
  app.focused = cell;
  cell.el.classList.add('focused');
  if (cell.unread) { cell.unread = false; cell.refreshState(); }
  if (focusInput) {
    const firstInput = cell.prompts.querySelector('input, textarea');
    (firstInput || cell.input).focus();
  }
}

function toggleMaximize(cell) {
  app.maximized = app.maximized === cell ? null : cell;
  layout();
  focusCell(cell, true);
}

/**
 * Picks how many rows to use so cells stay as close to square as possible, then fills the
 * rows left to right. Rows can hold different counts, so there are never empty slots.
 */
function chooseRows(n, width, height) {
  let best = 1;
  let bestScore = Infinity;
  for (let rows = 1; rows <= n; rows++) {
    const perRow = distribute(n, rows);
    const cellH = height / rows;
    let score = 0;
    for (const count of perRow) {
      const cellW = width / count;
      score += count * Math.abs(Math.log(cellW / cellH));
      if (cellW < 380) score += count * 2; // chat becomes unreadable when too narrow
      if (cellH < 260) score += count * 2;
    }
    score /= n;
    if (score < bestScore - 1e-9) { bestScore = score; best = rows; }
  }
  return best;
}

function distribute(n, rows) {
  const base = Math.floor(n / rows);
  const extra = n % rows;
  return Array.from({ length: rows }, (_, i) => base + (i < extra ? 1 : 0));
}

function layout() {
  const n = app.cells.length;
  $('#empty').classList.toggle('hidden', n > 0);
  grid.classList.toggle('maximized', !!app.maximized);

  // Preserve scroll positions: re-parenting a scroller can reset it.
  const scrolls = app.cells.map(c => [c, c.msgs.scrollTop, c.stickToBottom]);

  const rect = board.getBoundingClientRect();
  const rowsWanted = n ? chooseRows(n, rect.width, rect.height) : 0;
  const counts = distribute(n, rowsWanted);
  while (grid.children.length > rowsWanted) grid.lastElementChild.remove();
  while (grid.children.length < rowsWanted) grid.append(h('div', { class: 'row' }));

  let k = 0;
  counts.forEach((count, r) => {
    const row = grid.children[r];
    const want = app.cells.slice(k, k + count).map(c => c.el);
    k += count;
    const same = row.children.length === want.length && want.every((el, i) => row.children[i] === el);
    if (!same) row.replaceChildren(...want);
    row.classList.toggle('has-max', !!app.maximized && want.includes(app.maximized.el));
    for (const el of want) el.classList.toggle('max', !!app.maximized && el === app.maximized.el);
  });

  for (const [c, top, stick] of scrolls) {
    c.msgs.scrollTop = stick ? c.msgs.scrollHeight : top;
  }
}

function updateSummary() {
  let waiting = 0, running = 0, done = 0;
  for (const c of app.cells) {
    if (c.pending.size) waiting++;
    else if (c.status === 'running') running++;
    else if (c.unread) done++;
  }
  const pills = [];
  if (waiting) pills.push(h('span', { class: 'pill waiting', text: `${waiting} waiting on you` }));
  if (done) pills.push(h('span', { class: 'pill done', text: `${done} finished` }));
  if (running) pills.push(h('span', { class: 'pill running', text: `${running} working` }));
  $('#summary').replaceChildren(...pills);
  document.title = waiting ? `(${waiting}) HQ` : 'HQ';
}

// ======================================================================================
// New-session picker
// ======================================================================================

const picker = $('#picker');
let pickerIndex = -1;

async function openPicker() {
  const list = $('#picker-recent');
  list.replaceChildren();
  const exists = await Promise.all(app.recent.map(p => invoke('folder_exists', { path: p }).catch(() => false)));
  app.recent.forEach((path, i) => {
    if (!exists[i]) return;
    const forget = h('span', { class: 'icon-btn forget', title: 'Remove from list' }, icon('close'));
    forget.addEventListener('click', e => {
      e.stopPropagation();
      app.recent = app.recent.filter(p => p !== path);
      saveState();
      openPicker();
    });
    list.append(h('button', { class: 'picker-item', 'data-path': path, onclick: () => { closePicker(); addCell({ cwd: path }); } },
      icon('folder'), h('span', { class: 'name', text: baseName(path) || path }), h('span', { class: 'path', text: path }), forget));
  });
  pickerIndex = -1;
  picker.classList.remove('hidden');
  if (!list.children.length) browseFolder();
  else picker.querySelector('.picker-item').focus();
}

function closePicker() {
  picker.classList.add('hidden');
}

async function browseFolder() {
  closePicker();
  const path = await invoke('pick_folder').catch(() => null);
  if (path) addCell({ cwd: path });
}

function rememberFolder(path) {
  app.recent = [path, ...app.recent.filter(p => p.toLowerCase() !== path.toLowerCase())].slice(0, MAX_RECENT);
}

picker.addEventListener('keydown', e => {
  const items = [...picker.querySelectorAll('.picker-item')];
  if (e.key === 'Escape') { e.stopPropagation(); closePicker(); return; }
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    const cur = items.indexOf(document.activeElement);
    const next = e.key === 'ArrowDown' ? Math.min(cur + 1, items.length - 1) : Math.max(cur - 1, 0);
    items[next].focus();
  }
});
document.addEventListener('mousedown', e => {
  if (!picker.classList.contains('hidden') && !picker.contains(e.target) && !e.target.closest('#new-session, #empty-new')) closePicker();
});
$('#picker-browse').addEventListener('click', browseFolder);
$('#new-session').addEventListener('click', () => (picker.classList.contains('hidden') ? openPicker() : closePicker()));
$('#empty-new').addEventListener('click', openPicker);

// ======================================================================================
// Keyboard
// ======================================================================================

document.addEventListener('keydown', e => {
  const cell = app.focused;
  const typing = e.target.matches('input, textarea, select');
  if (e.ctrlKey && !e.shiftKey && !e.altKey) {
    if (e.key === 'n' || e.key === 'N') { e.preventDefault(); openPicker(); return; }
    if ((e.key === 'm' || e.key === 'M') && cell) { e.preventDefault(); toggleMaximize(cell); return; }
    if ((e.key === 'w' || e.key === 'W') && cell) { e.preventDefault(); cell.requestClose(cell.el.querySelector('.act-close')); return; }
    if (/^[1-9]$/.test(e.key)) {
      const target = app.cells[Number(e.key) - 1];
      if (target) {
        e.preventDefault();
        if (app.maximized) { app.maximized = target; layout(); }
        focusCell(target, true);
      }
      return;
    }
  }
  if (!cell) return;
  if (e.key === 'Escape' && picker.classList.contains('hidden') && (cell.status === 'running' || cell.pending.size)) {
    e.preventDefault();
    cell.interrupt();
    return;
  }
  // 1/2/3 answer the top prompt when the focus is not in a text field.
  if (!typing && !e.ctrlKey && !e.altKey && /^[1-9]$/.test(e.key)) {
    const card = cell.prompts.firstElementChild;
    const run = card && card.hotkeys && card.hotkeys[Number(e.key) - 1];
    if (run) { e.preventDefault(); run(); }
  }
});

new ResizeObserver(() => layout()).observe(board);

// ======================================================================================
// Persistence
// ======================================================================================

let saveTimer = null;
function saveState() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    const state = {
      version: 1,
      cells: app.cells.map(c => ({ cwd: c.cwd, sessionId: c.sessionId, permissionMode: c.permissionMode, model: c.model })),
      recent: app.recent,
    };
    invoke('save_state', { json: JSON.stringify(state) }).catch(err => console.error('save failed', err));
  }, 300);
}

async function restore() {
  let state = null;
  try {
    const raw = await invoke('load_state');
    state = raw ? JSON.parse(raw) : null;
  } catch (err) {
    console.error('load failed', err);
  }
  app.recent = Array.isArray(state && state.recent) ? state.recent : [];
  for (const c of (state && state.cells) || []) {
    if (!c || !c.cwd) continue;
    const ok = await invoke('folder_exists', { path: c.cwd }).catch(() => false);
    if (!ok) continue;
    addCell(c, { restore: true });
  }
  layout();
  updateSummary();
}

restore();
