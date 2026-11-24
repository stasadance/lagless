#!/bin/bash
# Run the network scenario matrix and print a summary table.
# usage: bench/scenarios.sh [out.jsonl] [kind:gate...]   (default: rtc+rack+pace:1 ndc:0 ndc:1)
# env: SEEDS="1 2 3" DUR=30 ONLY="bloat bursty" (scenario names)
cd "$(dirname "$0")"
OUT=${1:-scenarios.jsonl}; shift || true
KINDS=${*:-"rtc+rack+pace:1 ndc:0 ndc:1"}
# name | lossPct | rttMs | flags
SCENARIOS=(
  "random5     |5|200|"
  "bloat       |0|200|--rate 32 --queue 1000"
  "bloat-loss  |2|200|--rate 48 --queue 500"
  "contention8 |1|200|--peers 8 --rate 240 --queue 300"
  "bursty      |0|200|--ge 0.01,0.25,0.5"
  "jitter      |1|200|--jitter 30"
  "rtt-swing   |1|200|--swing 200,20"
  "rtt400      |2|400|"
)
: > "$OUT"
for sc in "${SCENARIOS[@]}"; do
  IFS='|' read -r name loss rtt flags <<< "$sc"; name=${name// /}
  if [ -n "$ONLY" ] && [[ " $ONLY " != *" $name "* ]]; then continue; fi
  for seed in ${SEEDS:-1 2 3}; do for kg in $KINDS; do
    k=${kg%%:*}; g=${kg##*:}; [ "$g" = "$kg" ] && g=1
    # shellcheck disable=SC2086
    timeout 180 node run.mts "$k" "$g" "$loss" "$seed" "${DUR:-30}" "$rtt" 28 12 --label "$name" $flags 2>/dev/null | tail -1 >> "$OUT" \
      || echo "{\"error\":\"timeout\",\"label\":\"$name\",\"cfg\":\"$kg $seed\"}" >> "$OUT"
  done; done
done
node summarize.mts "$OUT"
