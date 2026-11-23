// node summarize.js results.jsonl -> markdown table (median across seeds)
const fs = require('fs');
const rows = fs.readFileSync(process.argv[2], 'utf8').trim().split('\n').map((l) => { try { return JSON.parse(l); } catch { return { error: l }; } });
const errs = rows.filter((r) => r.error);
const ok = rows.filter((r) => !r.error);
const med = (a) => { const s = a.filter((x) => x != null).sort((x, y) => x - y); return s.length ? s[Math.floor(s.length / 2)] : null; };
const groups = {};
for (const r of ok) (groups[`${r.server}|${r.lossPct}`] ||= []).push(r);
const order = ['rtc-gate-rack-pace', 'rtc-gate', 'rtc-gate-rack', 'rtc-gate-rack-v2', 'rtc-gate-rack-rto200', 'rtc-naive', 'ndc-gate', 'ndc-naive'];
let out = '| config | loss | ping p50 | ping p95 | state age p50 | state age p95 | reliable age p50 | reliable age p95 | goodput u/r KB/s | unrel dropped | seeds |\n|---|---|---|---|---|---|---|---|---|---|---|\n';
for (const loss of [0, 2, 5]) for (const s of order) {
  const g = groups[`${s}|${loss}`]; if (!g) continue;
  const m = (f) => med(g.map(f));
  const drop = m((r) => 1 - r.unreliable.accepted / r.unreliable.offered);
  out += `| ${s} | ${loss}% | ${m((r) => r.ping.p50)} | ${m((r) => r.ping.p95)} | ${m((r) => r.uAge.p50)} | ${m((r) => r.uAge.p95)} | ${m((r) => r.rAge.p50)} | ${m((r) => r.rAge.p95)} | ${m((r) => r.goodput.uKBps)}/${m((r) => r.goodput.rKBps)} | ${Math.round(drop * 100)}% | ${g.length} |\n`;
}
if (errs.length) out += `\nErrors: ${errs.length}\n` + errs.map((e) => '- ' + JSON.stringify(e).slice(0, 200)).join('\n') + '\n';
console.log(out);
