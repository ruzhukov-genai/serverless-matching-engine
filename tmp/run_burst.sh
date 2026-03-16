#!/bin/bash
set -e
cd ~/projects/serverless-matching-engine

LOG=/tmp/sme-worker.log
rm -f "$LOG" /tmp/sme-gateway.log

DATABASE_URL=postgres://sme:sme_dev@localhost:5432/matching_engine \
DRAGONFLY_URL=redis://localhost:6379 \
RUST_LOG=info \
setsid ./target/release/sme-api >>"$LOG" 2>&1 &
WPID=$!
sleep 3

PORT=3001 DRAGONFLY_URL=redis://localhost:6379 RUST_LOG=info \
setsid ./target/release/sme-gateway >>/tmp/sme-gateway.log 2>&1 &
GPID=$!
sleep 2

API="http://localhost:3001"
PAIR="ETH-USDT"
N=50

echo "=== Burst test: $N concurrent order pairs on $PAIR ==="

# Resting sells (sequential to avoid overwhelming)
for i in $(seq 1 $N); do
  P=$((4000 + i))
  curl -s -X POST "$API/api/orders" -H "Content-Type: application/json" \
    -d "{\"user_id\":\"user-3\",\"pair_id\":\"$PAIR\",\"side\":\"Sell\",\"order_type\":\"Limit\",\"price\":\"$P\",\"quantity\":\"0.01\"}" > /dev/null
done
sleep 3
> "$LOG"

# Burst: all buys in parallel
echo "Firing $N buys concurrently..."
for i in $(seq 1 $N); do
  P=$((4000 + i))
  curl -s -X POST "$API/api/orders" -H "Content-Type: application/json" \
    -d "{\"user_id\":\"user-4\",\"pair_id\":\"$PAIR\",\"side\":\"Buy\",\"order_type\":\"Limit\",\"price\":\"$P\",\"quantity\":\"0.01\"}" > /dev/null &
done
wait
sleep 5

echo ""
python3 << 'PYEOF'
import json

segments = {'queue_latency_us':[], 'parse_us':[], 'validate_us':[], 'db_pre_us':[], 'lua_us':[], 'cache_us':[], 'total_us':[]}

with open("/tmp/sme-worker.log") as f:
    for line in f:
        if "segments" not in line: continue
        try:
            data = json.loads(line.strip())
        except: continue
        fld = data.get("fields", {})
        for key in segments:
            if key in fld:
                segments[key].append(int(fld[key]))

n = len(segments["total_us"])
print(f"=== Burst Segment Analysis ({n} orders) ===\n")
if n == 0:
    print("No profiled orders found"); exit()

print(f'{"Segment":<20} {"Min":>8} {"Median":>8} {"p95":>8} {"p99":>8} {"Max":>8}  {"% of med":>9}')
print("-" * 82)

totals = sorted(segments["total_us"])
totals_med = totals[n//2]

for key in ["queue_latency_us","parse_us","validate_us","db_pre_us","lua_us","cache_us","total_us"]:
    vals = sorted(segments[key])
    if not vals: continue
    c = len(vals)
    print(f"{key.replace('_us','').replace('_',' '):<20} {vals[0]:>8} {vals[c//2]:>8} {vals[int(c*0.95)]:>8} {vals[int(c*0.99)]:>8} {vals[-1]:>8}  {vals[c//2]/totals_med*100 if key!='total_us' else 100:.1f}%")
PYEOF

kill $WPID $GPID 2>/dev/null
wait $WPID $GPID 2>/dev/null
echo -e "\n=== Done ==="
