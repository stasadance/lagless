#!/bin/bash
# Run a loss matrix and print a summary table.
# usage: bench/matrix.sh [out.jsonl] [kinds...]   (default kinds: rtc+rack+pace rtc ndc)
# env: LOSSES="0 2 5" SEEDS="1 2 3" DUR=45 RTT=200 GATE=1
cd "$(dirname "$0")"
OUT=${1:-results.jsonl}; shift || true
KINDS=${*:-"rtc+rack+pace rtc ndc"}
: > "$OUT"
for loss in ${LOSSES:-0 2 5}; do for seed in ${SEEDS:-1 2 3}; do for k in $KINDS; do
  timeout 90 node run.js "$k" "${GATE:-1}" "$loss" "$seed" "${DUR:-45}" "${RTT:-200}" 2>/dev/null | tail -1 >> "$OUT" \
    || echo "{\"error\":\"timeout\",\"cfg\":\"$k $loss $seed\"}" >> "$OUT"
done; done; done
node summarize.js "$OUT"
