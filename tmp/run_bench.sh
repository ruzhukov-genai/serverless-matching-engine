#!/bin/bash
set -e
cd ~/projects/serverless-matching-engine

LOG=/tmp/sme-worker.log
rm -f "$LOG" /tmp/sme-gateway.log

# Start worker + gateway (detached)
DATABASE_URL=postgres://sme:sme_dev@localhost:5432/matching_engine \
REDIS_URL=redis://localhost:6379 \
RUST_LOG=info \
setsid ./target/release/sme-api >>"$LOG" 2>&1 &
WPID=$!
sleep 3

PORT=3001 REDIS_URL=redis://localhost:6379 RUST_LOG=info \
setsid ./target/release/sme-gateway >>/tmp/sme-gateway.log 2>&1 &
GPID=$!
sleep 2

API="http://localhost:3001"
PAIR="BTC-USDT"
N=50

echo "=== Services up (worker=$WPID gateway=$GPID) ==="

# Phase 1: place resting sells (user-1 has BTC)
echo "Placing $N resting sells..."
for i in $(seq 1 $N); do
  P=$((70000 + i))
  curl -s -X POST "$API/api/orders" -H "Content-Type: application/json" \
    -d "{\"user_id\":\"user-1\",\"pair_id\":\"$PAIR\",\"side\":\"Sell\",\"order_type\":\"Limit\",\"price\":\"$P\",\"quantity\":\"0.001\"}" > /dev/null
done
sleep 3
echo "Sells placed."

# Phase 2: truncate log, then matching buys (user-2 has USDT)
> "$LOG"
echo "Placing $N matching buys (profiled)..."
for i in $(seq 1 $N); do
  P=$((70000 + i))
  curl -s -X POST "$API/api/orders" -H "Content-Type: application/json" \
    -d "{\"user_id\":\"user-2\",\"pair_id\":\"$PAIR\",\"side\":\"Buy\",\"order_type\":\"Limit\",\"price\":\"$P\",\"quantity\":\"0.001\"}" > /dev/null
done
sleep 5

echo ""
echo "=== Segment Analysis ($N matching orders) ==="
echo ""

python3 << 'PYEOF'
import json, sys

segments = {'queue_latency_us':[], 'parse_us':[], 'validate_us':[], 'db_pre_us':[], 'lua_us':[], 'cache_us':[], 'total_us':[]}

with open("/tmp/sme-worker.log") as f:
    for line in f:
        if "segments" not in line:
            continue
        try:
            data = json.loads(line.strip())
        except:
            continue
        fld = data.get("fields", {})
        for key in segments:
            if key in fld:
                segments[key].append(int(fld[key]))

n = len(segments["total_us"])
print(f"Orders profiled: {n}")
if n == 0:
    # dump last 5 log lines for debugging
    with open("/tmp/sme-worker.log") as f:
        lines = f.readlines()
    print(f"Log lines: {len(lines)}")
    for l in lines[-5:]:
        print(l.strip()[:200])
    sys.exit(0)

print()
print(f'{"Segment":<20} {"Min":>8} {"Median":>8} {"p95":>8} {"p99":>8} {"Max":>8}  {"% of med":>9}')
print("-" * 82)

totals = sorted(segments["total_us"])
totals_med = totals[n//2]

for key in ["queue_latency_us","parse_us","validate_us","db_pre_us","lua_us","cache_us","total_us"]:
    vals = sorted(segments[key])
    if not vals:
        continue
    c = len(vals)
    mn = vals[0]
    med = vals[c//2]
    p95 = vals[int(c*0.95)]
    p99 = vals[int(c*0.99)]
    mx = vals[-1]
    pct = f"{med/totals_med*100:.1f}%" if key != "total_us" else "100%"
    label = key.replace("_us","").replace("_"," ")
    print(f"{label:<20} {mn:>8} {med:>8} {p95:>8} {p99:>8} {mx:>8}  {pct:>9}")
PYEOF

# Cleanup
kill $WPID $GPID 2>/dev/null
wait $WPID $GPID 2>/dev/null
echo ""
echo "=== Done ==="
