'use strict';

// ======================================================================================
// Cloud cells
// ======================================================================================
//
// A cloud cell is an ordinary Cell with `backend === 'cloud'`: src/cloud.rs runs the session on
// Anthropic's Managed Agents, billed to the API key saved in Settings, and translates its events
// to the stream-json lines Claude speaks, so the chat itself needs nothing new. What differs is
// kept here: the model list and the header. The container starts empty (it has none of the
// folder the cell was opened on) and runs its tools without asking. app.js calls these
// functions at the few places the agents differ; removing cloud cells means removing this file
// and those calls.

const CLOUD_MODELS = [
  { value: 'default', displayName: 'Default', description: 'Sonnet 5.5: fast and capable for everyday work' },
  { value: 'claude-sonnet-5-5', displayName: 'Sonnet 5.5', description: '$2 / $10 per MTok' },
  { value: 'claude-opus-5-5', displayName: 'Opus 5.5', description: '$4 / $20 per MTok' },
  { value: 'claude-haiku-5-5', displayName: 'Haiku 5.5', description: '$0.10 / $0.50 per MTok' },
  { value: 'claude-fable-5-1', displayName: 'Fable 5.1', description: '$10 / $50 per MTok' },
];

function cloudModels() {
  return CLOUD_MODELS;
}

/** Fits a new cloud cell's header and composer to Managed Agents. */
function cloudDecorate(cell) {
  const el = cell.el;
  el.classList.add('agent-cloud');
  el.querySelector('.cell-state').before(h('span', {
    class: 'agent-tag', text: 'Cloud',
    title: "Runs on Anthropic's Managed Agents in a cloud container, billed to your API key. The container starts empty: it has none of this folder.",
  }));
  cell.modeSel.replaceChildren(h('option', { value: 'bypassPermissions', text: 'Runs without asking (cloud sandbox)' }));
  cell.permissionMode = 'bypassPermissions';
  cell.modeSel.disabled = true;
  cell.mcpBtn.classList.add('hidden');
  cell.taskBtn.classList.add('hidden'); // the cloud agent has no task-list tools
  cell.input.placeholder = 'Ask the cloud agent…  (Enter to send, Shift+Enter for new line)';
}
