const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
function functionSource(name, next) {
  return source.slice(source.indexOf(`function ${name}(`), source.indexOf(`\n${next}`, source.indexOf(`function ${name}(`)));
}
const code = [functionSource('tokensSince', 'async function loadTokens'),
  functionSource('tokenTotals', '/** The sessions shown'),
  functionSource('tokenChart', 'function renderTokens')].join('\n');

for (const zone of ['Asia/Kolkata', 'Asia/Kathmandu', 'Africa/Johannesburg', 'America/New_York']) {
  for (const range of ['today', '7', '30', 'all']) {
    test(`${zone}: ${range} chart accounts for every returned request`, () => {
      const previous = process.env.TZ;
      process.env.TZ = zone;
      try {
        const now = new Date('2026-10-28T12:00:00Z').getTime();
        class Clock extends Date {
          constructor(...args) { super(...(args.length ? args : [now])); }
          static now() { return now; }
        }
        const midnight = new Clock();
        midnight.setHours(0, 0, 0, 0);
        const rows = [midnight.getTime(), midnight.getTime() + 600_000, now].map(at => ({
          at, requests: 1, input: 1, output: 2, cacheWrite: 3, cacheRead: 4, cost: 0.01,
        }));
        const sessions = [{ rows }];
        const context = vm.createContext({ Date: Clock, tokens: { range, sessions }, HOUR_MS: 3_600_000,
          formatCost: String, formatTokens: String,
          h: (tag, attrs, children) => ({ tag, attrs, children }), sessions });
        vm.runInContext(code, context);
        const chart = vm.runInContext('tokenChart(sessions, false)', context);
        const chartTokens = chart.children.reduce((sum, b) => sum + Number(b.attrs.title.match(/· (\d+) tokens$/)[1]), 0);
        assert.equal(chartTokens, vm.runInContext('tokenTotals(sessions[0].rows).tokens', context));
        assert.equal(chartTokens, 30);
        if (range === 'today') assert.equal(vm.runInContext('tokensSince(tokens.range)', context), midnight.getTime());
      } finally {
        if (previous === undefined) delete process.env.TZ;
        else process.env.TZ = previous;
      }
    });
  }
}
