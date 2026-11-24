// node summarize.mts results.jsonl -> markdown table (median across seeds)
// Rows group by scenario label (or loss% for unlabeled runs) and server config.
import fs from 'node:fs';

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type Row = any;
const rows = fs.readFileSync(process.argv[2], 'utf8').trim().split('\n').map((l): Row => { try { return JSON.parse(l); } catch { return { error: l }; } });
const errs = rows.filter((r) => r.error);
const ok = rows.filter((r) => !r.error);
const med = (a: (number | null | undefined)[]) => { const s = (a.filter((x) => x != null) as number[]).sort((x, y) => x - y); return s.length ? s[Math.floor(s.length / 2)] : null; };
const scen = (r: Row) => r.label ?? `${r.lossPct}%`;
const groups = new Map<string, Row[]>();
for (const r of ok) { const k = `${scen(r)}|${r.server}`; groups.set(k, [...(groups.get(k) ?? []), r]); }
const order = ['rtc-gate-rack-pace', 'rtc-gate', 'rtc-gate-rack', 'rtc-gate-rack-v2', 'rtc-gate-rack-rto200', 'rtc-naive', 'ndc-gate', 'ndc-naive'];
const scens = [...new Set(ok.map(scen))];
const multi = ok.some((r) => r.peers > 1);
let out = `| scenario | config | ping p50 | ping p95 | state age p50 | state age p95 | reliable age p50 | reliable age p95 | goodput u/r KB/s | unrel dropped |${multi ? ' worst peer ping/rel p95 |' : ''} seeds |\n`;
out += `|---|---|---|---|---|---|---|---|---|---|${multi ? '---|' : ''}---|\n`;
for (const sc of scens) for (const s of [...order, ...new Set(ok.map((r) => r.server))].filter((v, i, a) => a.indexOf(v) === i)) {
  const g = groups.get(`${sc}|${s}`); if (!g) continue;
  const m = (f: (r: Row) => number | null | undefined) => med(g.map(f));
  const drop = m((r) => 1 - r.unreliable.accepted / r.unreliable.offered);
  const worst = multi ? ` ${g[0].worstPeer ? `${m((r) => r.worstPeer?.pingP95)} / ${m((r) => r.worstPeer?.rAgeP95)}` : ''} |` : '';
  out += `| ${sc} | ${s} | ${m((r) => r.ping.p50)} | ${m((r) => r.ping.p95)} | ${m((r) => r.uAge.p50)} | ${m((r) => r.uAge.p95)} | ${m((r) => r.rAge.p50)} | ${m((r) => r.rAge.p95)} | ${m((r) => r.goodput.uKBps)}/${m((r) => r.goodput.rKBps)} | ${Math.round((drop ?? 0) * 100)}% |${worst} ${g.length} |\n`;
}
if (errs.length) out += `\nErrors: ${errs.length}\n` + errs.map((e) => '- ' + JSON.stringify(e).slice(0, 200)).join('\n') + '\n';
console.log(out);
