'use strict';

// ======================================================================================
// Board layout: the grid, the spotlight, splitters, groups, moving cells, and the motion
// ======================================================================================
//
// The open cells fill rows that keep them near square (chooseRows). Cells in the spotlight take a
// main pane and the rest stack in a strip beside it. Splitters in the gaps resize cells, rows and
// the strip; the sizes are flex-grow weights kept on each cell (width in a row, height in the
// strip), per row count, and for the strip. A header can be dragged to move its cell. Cells in a
// group always sit together, so dragging one past another cell moves the whole group.
//
// Every change the user makes glides into place (FLIP: measure each cell, lay out, then animate it
// from where it was). Window resizes and the startup restore lay out at once.
//
// Loaded before app.js; its functions use app.js's globals (app, grid, dock, board, menuState,
// focusCell, saveState, renderSidebar, refreshMenu, closeMenu) only once called.

const LAYOUT_MS = 240;
const LAYOUT_EASE = 'cubic-bezier(0.2, 0.8, 0.2, 1)';
const MIN_W = 220;            // the narrowest a splitter drag leaves a cell or the strip
const MIN_H = 110;            // and the shortest
const DRAG_START_PX = 5;      // a header press moves this far before it becomes a drag (clicks still rename, expand, maximize)
const DEFAULT_STRIP = 0.28;   // the strip's share of the board's width in the spotlight
const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)');

// Sizes dragged with the splitters that belong to no cell: row heights per arrangement ("g3" is the
// grid with 3 rows, "s2" the spotlight pane with 2), and the strip's share of the width.
const boardSizes = { rows: {}, strip: DEFAULT_STRIP };
const rowPool = { g: [], s: [] };      // row elements, reused so a layout moves as little as it can
const splitPool = new Map();           // key -> splitter element, kept while the layout uses it
const spotMain = h('div', { class: 'spot-main' });
const spotStrip = h('div', { class: 'spot-strip' });
const flips = new Map();               // element -> its running layout animation
let cellDrag = null;                   // a cell being dragged to a new place
let dropSlot = null;                   // the outline of where the dragged cell lands

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

/** Replaces an element's children only when they differ: re-parenting a chat resets its scroll and focus. */
function setKids(parent, kids) {
  if (parent.children.length === kids.length && kids.every((el, i) => parent.children[i] === el)) return;
  parent.replaceChildren(...kids);
}

/**
 * Lays the board out. `animate` (the default, for anything the user did) glides every cell from
 * where it was; the window's resize and the startup restore pass false.
 */
function layout(animate = true) {
  $('#empty').classList.toggle('hidden', app.cells.length > 0);
  const motion = animate && !reducedMotion.matches && !document.hidden;
  const before = motion ? measureCells() : null;
  if (!motion) stopFlips();
  const active = document.activeElement;

  // Collapsed cells sit in the dock as bare headers; the grid lays out the rest.
  const shown = app.cells.filter(c => !c.collapsed);
  const docked = app.cells.filter(c => c.collapsed);
  for (const c of docked) c.el.style.flexGrow = ''; // the dock sizes its headers itself
  setKids(dock, docked.map(c => c.el));
  grid.classList.toggle('all-collapsed', !shown.length && app.cells.length > 0);
  grid.classList.toggle('maximized', !!app.maximized);

  // Preserve scroll positions: re-parenting a scroller can reset it.
  const scrolls = shown.map(c => [c, c.msgs.scrollTop, c.stickToBottom]);
  const used = new Set();
  const rect = grid.getBoundingClientRect(); // after the dock is filled, which takes its height
  const spot = shown.filter(c => c.spot);
  const rest = shown.filter(c => !c.spot);
  const spotlight = spot.length > 0 && rest.length > 0;
  grid.classList.toggle('spotlight', spotlight);
  if (spotlight) {
    const share = boardSizes.strip;
    setKids(spotMain, rowsOf(spot, rect.width * (1 - share), rect.height, 's', used));
    setKids(spotStrip, chain(rest, 'y', used));
    spotMain.style.flexGrow = 2 * (1 - share); // averaging 1, as asGrows does
    spotStrip.style.flexGrow = 2 * share;
    setKids(grid, [spotMain, splitter('strip', 'x', used), spotStrip]);
  } else {
    setKids(grid, rowsOf(shown, rect.width, rect.height, 'g', used));
  }
  for (const key of splitPool.keys()) if (!used.has(key)) splitPool.delete(key);

  // Maximized: only the maximized cell and the boxes around it stay on screen (style.css).
  const max = app.maximized && app.maximized.el;
  for (const box of [spotMain, spotStrip, ...rowPool.g, ...rowPool.s]) box.classList.toggle('has-max', !!max && box.contains(max));
  for (const c of shown) c.el.classList.toggle('max', c.el === max);
  // A move or a closed cell can change which session leads a group, and so its colour.
  for (const c of app.cells) if (c.group) c.applyGroup();

  // Writes only: a scrollTop past the end is clamped, so scrollHeight is not read once per cell.
  for (const [c, top, stick] of scrolls) c.msgs.scrollTop = stick ? 1e9 : top;
  // Moving an element blurs whatever had focus inside it.
  if (active && active !== document.activeElement && active.isConnected) active.focus({ preventScroll: true });
  // Rings are measured at the cells' new places, so running moves stop first; glide() replays
  // every move from the places measured at the start.
  stopFlips();
  if (cellDrag) placeDragged();
  placeRings(motion);
  if (before) glide(before);
  if (menuState.anchor) refreshMenu();
}

// ---------- group rings ----------

const ringPool = [];

/**
 * Draws one ring around each group's cells, in the gaps outside them, so the cells inside a group
 * have no line between them. A group that wraps onto the next row (or sits in both the spotlight
 * pane and the strip) gets a ring per run of neighbours. `smooth`: the rings glide with the cells.
 */
function placeRings(smooth) {
  const b = board.getBoundingClientRect();
  const runs = [];
  let run = null;
  for (const c of app.maximized ? [] : app.cells) { // a maximized cell stands alone
    const r = cellDrag && c === cellDrag.cell ? cellDrag.slot : c.el.getBoundingClientRect();
    if (!c.group || !r.width || !r.height) { run = null; continue; }
    const p = run && run.last;
    const nextTo = p && run.group === c.group && p.parent === c.el.parentElement
      && ((Math.abs(p.r.top - r.top) < 1 && Math.abs(p.r.height - r.height) < 1) || (Math.abs(p.r.left - r.left) < 1 && Math.abs(p.r.width - r.width) < 1));
    if (nextTo) {
      Object.assign(run, { left: Math.min(run.left, r.left), top: Math.min(run.top, r.top), right: Math.max(run.right, r.right), bottom: Math.max(run.bottom, r.bottom) });
    } else {
      run = { group: c.group, color: groupColor(c.group), left: r.left, top: r.top, right: r.right, bottom: r.bottom };
      runs.push(run);
    }
    run.last = { r, parent: c.el.parentElement };
  }
  const OUT = 3; // half the 6px gap: the ring's line sits between neighbouring cells
  runs.forEach((g, i) => {
    let ring = ringPool[i];
    const fresh = !ring;
    if (fresh) ring = ringPool[i] = h('div', { class: 'group-ring' });
    if (!ring.isConnected) board.append(ring);
    ring.classList.toggle('smooth', smooth && !fresh && ring.classList.contains('on'));
    ring.style.setProperty('--group', g.color);
    Object.assign(ring.style, {
      transform: `translate(${g.left - b.left - OUT}px, ${g.top - b.top - OUT}px)`,
      width: `${g.right - g.left + 2 * OUT}px`, height: `${g.bottom - g.top + 2 * OUT}px`,
    });
    ring.classList.add('on');
  });
  for (const ring of ringPool.slice(runs.length)) { ring.classList.remove('on', 'smooth'); ring.remove(); }
}

/** Rows of cells for the grid ('g') or the spotlight pane ('s'), with splitters between them. */
function rowsOf(cells, width, height, mode, used) {
  if (!cells.length) return [];
  const counts = distribute(cells.length, chooseRows(cells.length, width, height));
  const key = mode + counts.length;
  const weights = asGrows(rowWeights(key, counts.length));
  const out = [];
  let k = 0;
  counts.forEach((count, r) => {
    const row = rowPool[mode][r] || (rowPool[mode][r] = h('div', { class: 'row' }));
    setKids(row, chain(cells.slice(k, k += count), 'x', used));
    row.style.flexGrow = weights[r];
    row.dataset.key = key;
    row.dataset.index = r;
    if (r) out.push(splitter(`${key}-${r}`, 'y', used));
    out.push(row);
  });
  return out;
}

/** Cells in a line ('x' across a row, 'y' down the strip), each sized by its own weight, with splitters between them. */
function chain(cells, axis, used) {
  const out = [];
  const grow = asGrows(cells.map(c => (axis === 'x' ? c.flexW : c.flexH)));
  cells.forEach((c, i) => {
    c.el.style.flexGrow = grow[i];
    if (i) out.push(splitter(`${axis}-${c.hqId}`, axis, used));
    out.push(c.el);
  });
  return out;
}

/**
 * Weights scaled to average 1. Flexbox hands out only a share of the free space when the grows
 * add up to less than 1, so a lone cell weighted 0.7 (dragged smaller, then moved to a row of its
 * own) would leave a gap beside it.
 */
function asGrows(weights) {
  const sum = weights.reduce((a, w) => a + w, 0);
  return sum > 0 ? weights.map(w => w * weights.length / sum) : weights.map(() => 1);
}

function rowWeights(key, count) {
  const saved = boardSizes.rows[key];
  return Array.isArray(saved) && saved.length === count ? saved : Array(count).fill(1);
}

// ---------- motion ----------

/** Where each cell is on screen now, mid-animation included, so an interrupted move carries on from there. */
function measureCells() {
  const at = new Map();
  for (const c of app.cells) {
    if (cellDrag && c === cellDrag.cell) continue;
    const r = c.el.getBoundingClientRect();
    if (r.width && r.height) at.set(c.el, { r, docked: c.collapsed });
  }
  return at;
}

function stopFlips() {
  for (const anim of flips.values()) anim.cancel();
  flips.clear();
}

/** Plays every cell from where it was (`before`) to where the layout put it. */
function glide(before) {
  stopFlips(); // the new places are read without the old animations' offsets
  for (const c of app.cells) {
    if (cellDrag && c === cellDrag.cell) continue;
    const el = c.el;
    const r = el.getBoundingClientRect();
    if (!r.width || !r.height) continue;
    const was = before.get(el);
    let frames;
    if (!was) {
      // Newly on screen (added, or back from behind a maximized cell): it settles in.
      frames = [{ opacity: 0, transform: 'scale(0.97)' }, { opacity: 1, transform: 'none' }];
    } else {
      const dx = was.r.left - r.left, dy = was.r.top - r.top;
      const sx = was.r.width / r.width, sy = was.r.height / r.height;
      if (Math.abs(dx) < 0.5 && Math.abs(dy) < 0.5 && Math.abs(sx - 1) < 0.005 && Math.abs(sy - 1) < 0.005) continue;
      // Into or out of the dock the cell changes shape entirely: stretching a bare header would look
      // wrong, so it travels and fades instead.
      frames = was.docked !== c.collapsed
        ? [{ transformOrigin: '0 0', transform: `translate(${dx}px, ${dy}px)`, opacity: 0.25 }, { transformOrigin: '0 0', transform: 'none', opacity: 1 }]
        : [{ transformOrigin: '0 0', transform: `translate(${dx}px, ${dy}px) scale(${sx}, ${sy})` }, { transformOrigin: '0 0', transform: 'none' }];
    }
    play(el, frames);
  }
}

function play(el, frames, done) {
  const anim = el.animate(frames, { duration: LAYOUT_MS, easing: LAYOUT_EASE });
  flips.set(el, anim);
  const end = () => { if (flips.get(el) === anim) flips.delete(el); if (done) done(); };
  anim.addEventListener('finish', end);
  anim.addEventListener('cancel', end);
}

// ---------- moving a cell by its header ----------

/** A header press that moves past DRAG_START_PX picks the cell up; Esc puts it back. */
function wireCellDrag(cell) {
  const head = cell.el.querySelector('.cell-head');
  head.addEventListener('pointerdown', e => {
    if (e.button !== 0 || cellDrag || app.maximized || e.target.closest('button, input, select, textarea, .cell-rename')) return;
    const id = e.pointerId;
    const start = { x: e.clientX, y: e.clientY };
    let frame = 0, last = null, lifted = false;
    const move = ev => {
      if (ev.pointerId !== id) return;
      if (!lifted) {
        if (Math.hypot(ev.clientX - start.x, ev.clientY - start.y) < DRAG_START_PX) return;
        if (!liftCell(cell, start)) { stopMoving(); off(); return; }
        lifted = true;
      }
      last = ev;
      if (!frame) frame = requestAnimationFrame(() => { frame = 0; if (cellDrag && cellDrag.cell === cell && last) dragCellTo(last.clientX, last.clientY); });
    };
    const up = ev => {
      if (ev.pointerId !== id) return;
      stopMoving();
      off();
      if (cellDrag && cellDrag.cell === cell) dropCell(ev.type === 'pointercancel');
      // The release that ends a drag (or follows an Esc) must not also click: rename, expand, open the colour menu.
      if (lifted) swallowNextClick();
    };
    const key = ev => {
      if (ev.key !== 'Escape' || !cellDrag || cellDrag.cell !== cell) return;
      ev.preventDefault();
      ev.stopPropagation(); // Esc would otherwise interrupt the focused session
      stopMoving(); // the button is still down: `up` stays to swallow its click
      dropCell(true);
    };
    const stopMoving = () => {
      cancelAnimationFrame(frame);
      window.removeEventListener('pointermove', move);
      window.removeEventListener('keydown', key, true);
    };
    const off = () => {
      window.removeEventListener('pointerup', up);
      window.removeEventListener('pointercancel', up);
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
    window.addEventListener('pointercancel', up);
    window.addEventListener('keydown', key, true);
  });
}

function liftCell(cell, start) {
  if (cell.renaming || app.cells.length < 2) return false;
  const running = flips.get(cell.el);
  if (running) running.cancel();
  const r = cell.el.getBoundingClientRect();
  cellDrag = {
    cell, x: start.x, y: start.y, skip: null, slot: r,
    fx: (start.x - r.left) / r.width, fy: (start.y - r.top) / r.height, // where the header was grabbed, as a share of the cell
    order: app.cells.slice(), spots: new Map(app.cells.map(c => [c, c.spot])),
  };
  if (menuState.anchor) closeMenu();
  focusCell(cell, false);
  cell.el.classList.add('lifted');
  document.body.classList.add('cell-dragging');
  if (!dropSlot) dropSlot = h('div', { class: 'drop-slot' });
  board.append(dropSlot);
  placeSlot(r, false);
  requestAnimationFrame(() => dropSlot && dropSlot.classList.add('on'));
  return true;
}

/** The cell follows the pointer; over another cell it takes that cell's place and the rest make room. */
function dragCellTo(x, y) {
  const d = cellDrag;
  d.x = x;
  d.y = y;
  if (!app.cells.includes(d.cell)) { dropCell(false); return; } // closed while held (an agent can close sessions)
  const target = app.cells.find(c => c !== d.cell && c.collapsed === d.cell.collapsed && inside(c.el.getBoundingClientRect(), x, y));
  // Cells that just moved slide under the pointer as they make room: they must not swap straight
  // back. They count again once the pointer is over something else.
  if (d.skip && !(target && d.skip.has(target))) d.skip = null;
  if (target && !d.skip) {
    const was = app.cells.slice();
    moveCell(d.cell, target);
    d.skip = new Set(app.cells.filter((c, i) => c !== was[i] && c !== d.cell));
    if (d.skip.size) layout();
    else { d.skip = null; placeDragged(); }
  } else {
    placeDragged();
  }
}

const inside = (r, x, y) => x >= r.left && x < r.right && y >= r.top && y < r.bottom;

/**
 * Puts `cell` where `target` is. Across the spotlight's edge the two trade places, so the pane
 * keeps its size. A grouped cell brings its group along when it leaves it, and the groups stay whole.
 */
function moveCell(cell, target) {
  const cells = app.cells;
  const from = cells.indexOf(cell), to = cells.indexOf(target);
  if (cell.spot !== target.spot) {
    [cell.spot, target.spot] = [target.spot, cell.spot];
    cells[from] = target;
    cells[to] = cell;
    cell.applySpot();
    target.applySpot();
  } else {
    // Out of its group, a cell takes the whole group along in its order; onto another group, it
    // goes past that whole group rather than into it.
    const outside = target.group !== cell.group;
    const mine = outside && cell.group ? cells.filter(c => c.group === cell.group) : [cell];
    const theirs = outside && target.group ? cells.filter(c => c.group === target.group) : [target];
    for (const c of mine) cells.splice(cells.indexOf(c), 1);
    cells.splice(from < to ? cells.indexOf(theirs.at(-1)) + 1 : cells.indexOf(theirs[0]), 0, ...mine);
  }
  keepGroupsTogether(cell);
}

/** Keeps the dragged cell under the pointer, measured against its slot in the layout. */
function placeDragged() {
  const d = cellDrag;
  const el = d.cell.el;
  el.style.transform = '';
  d.slot = el.getBoundingClientRect();
  el.style.transform = `translate(${d.x - d.slot.left - d.fx * d.slot.width}px, ${d.y - d.slot.top - d.fy * d.slot.height}px)`;
  placeSlot(d.slot, true);
}

function placeSlot(r, smooth) {
  if (!dropSlot) return;
  const b = board.getBoundingClientRect();
  dropSlot.classList.toggle('smooth', smooth);
  Object.assign(dropSlot.style, { transform: `translate(${r.left - b.left}px, ${r.top - b.top}px)`, width: `${r.width}px`, height: `${r.height}px` });
}

/** Lands the cell in its slot (or back where it started, on Esc). */
function dropCell(cancel) {
  const d = cellDrag;
  const el = d.cell.el;
  const from = el.getBoundingClientRect();
  if (cancel) {
    // Back to the order it started from, leaving out cells closed meanwhile and keeping any added.
    const kept = d.order.filter(c => app.cells.includes(c));
    app.cells.splice(0, app.cells.length, ...kept, ...app.cells.filter(c => !d.order.includes(c)));
    for (const c of kept) if (c.spot !== d.spots.get(c)) { c.spot = d.spots.get(c); c.applySpot(); }
  }
  cellDrag = null;
  el.style.transform = '';
  document.body.classList.remove('cell-dragging');
  if (cancel) layout();
  const slot = dropSlot;
  dropSlot = null;
  if (slot) { slot.classList.remove('on'); setTimeout(() => slot.remove(), LAYOUT_MS); }
  const land = () => el.classList.remove('lifted');
  const r = el.getBoundingClientRect();
  const running = flips.get(el);
  if (running) running.cancel();
  if (reducedMotion.matches || !r.width) land();
  else play(el, [{ transformOrigin: '0 0', transform: `translate(${from.left - r.left}px, ${from.top - r.top}px) scale(${from.width / r.width}, ${from.height / r.height})` },
    { transformOrigin: '0 0', transform: 'none' }], land);
  if (!cancel && d.order.some((c, i) => app.cells[i] !== c)) {
    renderSidebar();
    saveState();
  }
}

/** Called from a pointerup: the click the browser fires right after it, in the same task, is dropped. */
function swallowNextClick() {
  const swallow = e => { e.stopPropagation(); e.preventDefault(); };
  window.addEventListener('click', swallow, { capture: true, once: true });
  setTimeout(() => window.removeEventListener('click', swallow, { capture: true }), 0);
}

// ---------- splitters ----------

function splitter(key, axis, used) {
  used.add(key);
  let el = splitPool.get(key);
  if (!el) {
    el = h('div', { class: `split split-${axis}` });
    el.addEventListener('pointerdown', e => resizeFrom(e, el, axis));
    el.addEventListener('dblclick', () => evenOut(el));
    splitPool.set(key, el);
  }
  return el;
}

/**
 * Drags the gap between two neighbours: one grows by what the other gives up, the rest stay put.
 * A layout() during the drag (a session collapses, say) re-applies the saved weights, so the drag
 * snaps back for a frame; cosmetic, and the next pointer move puts it right.
 */
function resizeFrom(e, el, axis) {
  const a = el.previousElementSibling, b = el.nextElementSibling;
  if (e.button !== 0 || !a || !b) return;
  e.preventDefault();
  stopFlips();
  el.setPointerCapture(e.pointerId);
  const size = x => { const r = x.getBoundingClientRect(); return axis === 'x' ? r.width : r.height; };
  const pa = size(a), both = pa + size(b);
  const total = (Number(a.style.flexGrow) || 1) + (Number(b.style.flexGrow) || 1);
  const least = Math.min(axis === 'x' ? MIN_W : MIN_H, both / 2);
  const start = axis === 'x' ? e.clientX : e.clientY;
  el.classList.add('active');
  document.body.classList.add(`resizing-${axis}`);
  let frame = 0, at = start;
  const apply = () => {
    frame = 0;
    const na = Math.max(least, Math.min(both - least, pa + at - start));
    a.style.flexGrow = total * na / both;
    b.style.flexGrow = total * (both - na) / both;
    placeRings(false);
  };
  const move = ev => {
    at = axis === 'x' ? ev.clientX : ev.clientY;
    if (!frame) frame = requestAnimationFrame(apply);
  };
  const box = el.parentElement;
  const up = () => {
    if (frame) { cancelAnimationFrame(frame); apply(); }
    for (const type of ['pointermove', 'pointerup', 'pointercancel', 'lostpointercapture']) el.removeEventListener(type, type === 'pointermove' ? move : up);
    el.classList.remove('active');
    document.body.classList.remove(`resizing-${axis}`);
    if (box && box.isConnected) keepWeights(box, axis);
    saveState();
  };
  el.addEventListener('pointermove', move);
  el.addEventListener('pointerup', up);
  el.addEventListener('pointercancel', up);
  el.addEventListener('lostpointercapture', up); // a layout took the splitter away mid-drag
}

/**
 * Reads the dragged flex-grows back into the cells, rows or strip of one line. The whole line is
 * read, not just the two neighbours: layout() scales a line's weights (asGrows), so mixing scaled
 * and saved values would change the others' sizes.
 */
function keepWeights(box, axis) {
  if (box === grid && grid.classList.contains('spotlight')) {
    const strip = Number(spotStrip.style.flexGrow) || DEFAULT_STRIP, main = Number(spotMain.style.flexGrow) || 1 - DEFAULT_STRIP;
    boardSizes.strip = strip / (strip + main);
    return;
  }
  const rows = [];
  for (const kid of box.children) {
    const g = Number(kid.style.flexGrow) || 1;
    const cell = app.cells.find(c => c.el === kid);
    if (cell) {
      if (axis === 'x') cell.flexW = g;
      else cell.flexH = g;
    } else if (kid.classList.contains('row')) {
      rows.push(kid);
    }
  }
  if (rows.length) boardSizes.rows[rows[0].dataset.key] = rows.map(r => Number(r.style.flexGrow) || 1);
}

/** Double-clicking a splitter gives everything beside it an equal share again. */
function evenOut(el) {
  const box = el.parentElement;
  if (!box) return;
  if (box === grid && grid.classList.contains('spotlight')) {
    boardSizes.strip = DEFAULT_STRIP;
  } else {
    for (const kid of box.children) {
      const cell = app.cells.find(c => c.el === kid);
      if (cell) {
        if (el.classList.contains('split-x')) cell.flexW = 1;
        else cell.flexH = 1;
      } else if (kid.classList.contains('row')) {
        delete boardSizes.rows[kid.dataset.key];
      }
    }
  }
  layout();
  saveState();
}

const sanitizeWeight = w => (Number.isFinite(w) && w >= 0.05 && w <= 20 ? w : 1);

function sanitizeBoardSizes(v) {
  const rows = {};
  const saved = v && typeof v.rows === 'object' && v.rows ? v.rows : {};
  for (const [key, weights] of Object.entries(saved)) {
    if (/^[gs]\d{1,2}$/.test(key) && Array.isArray(weights) && weights.length === Number(key.slice(1))) rows[key] = weights.map(sanitizeWeight);
  }
  const strip = v && Number.isFinite(v.strip) && v.strip >= 0.1 && v.strip <= 0.6 ? v.strip : DEFAULT_STRIP;
  return { rows, strip };
}

// ---------- the spotlight ----------

/** Puts a session in the spotlight's main pane, or back among the rest. */
function toggleSpot(cell) {
  if (cell.collapsed) cell.setCollapsed(false);
  cell.spot = !cell.spot;
  cell.applySpot();
  if (app.maximized && app.maximized !== cell) app.maximized = null;
  layout();
  focusCell(cell, true);
  saveState();
}

// ---------- groups ----------

/**
 * A group's colour: its first session's project colour, so a group started from Framework wears
 * Framework's colour. With no session left in it, one picked from the palette by its id.
 */
function groupColor(id) {
  const first = app.cells.find(c => c.group === id);
  if (first) return first.color;
  let n = 0;
  for (const ch of id) n = (n * 31 + ch.charCodeAt(0)) >>> 0;
  return PROJECT_COLORS[n % PROJECT_COLORS.length];
}

function newGroupId() {
  let id;
  do id = `g${Math.random().toString(36).slice(2, 7)}`; while (app.cells.some(c => c.group === id));
  return id;
}

/**
 * Reorders the board so each group's cells sit together, in their own order. A group goes where
 * `anchor` is when the anchor belongs to it (the cell being dragged), else where its first cell is.
 */
function keepGroupsTogether(anchor = null) {
  const out = [], done = new Set();
  for (const c of app.cells) {
    if (!c.group) { out.push(c); continue; }
    if (done.has(c.group) || (anchor && anchor.group === c.group && c !== anchor)) continue;
    done.add(c.group);
    out.push(...app.cells.filter(m => m.group === c.group));
  }
  app.cells.splice(0, app.cells.length, ...out);
}

/** A group of one is no group: its ring goes. */
function tidyGroups() {
  for (const c of app.cells) {
    if (c.group && !app.cells.some(m => m !== c && m.group === c.group)) { c.group = null; c.applyGroup(); }
  }
}

function joinGroup(cell, other) {
  if (!other.group) { other.group = newGroupId(); other.applyGroup(); }
  cell.group = other.group;
  cell.applyGroup();
  tidyGroups();
  keepGroupsTogether(other);
  afterGroupChange();
}

function leaveGroup(cell) {
  cell.group = null;
  cell.applyGroup();
  tidyGroups();
  afterGroupChange();
}

function afterGroupChange() {
  layout();
  renderSidebar();
  saveState();
}

/** The Group button's menu: join any other session's group, or leave this one. */
function buildGroupMenu(menuEl, cell) {
  const item = (name, desc, ico, color, run) => h('button', { class: 'menu-item', onclick: () => { closeMenu(); run(); } },
    h('span', { class: 'group-swatch', style: color ? `--group:${color}` : null }, icon(ico)),
    h('span', { class: 'menu-main' }, h('span', { class: 'name', text: name }), desc ? h('span', { class: 'desc', text: desc }) : null));
  const others = app.cells.filter(c => c !== cell && (!cell.group || c.group !== cell.group));
  // append() writes a null as the text "null": the optional rows are filtered out first.
  menuEl.append(...[h('div', { class: 'menu-title', text: cell.group ? 'Group · move to another' : 'Group with…' }),
    ...others.map(c => item(c.displayTitle() || 'New session', baseName(c.cwd) || c.cwd, c.group ? 'layers-active' : 'layers', c.group && groupColor(c.group), () => joinGroup(cell, c))),
    others.length ? null : h('div', { class: 'menu-empty', text: 'No other sessions are open.' }),
    cell.group ? item('Leave the group', null, 'close', null, () => leaveGroup(cell)) : null].filter(Boolean));
}
