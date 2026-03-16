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
N=50

echo "=== Services up ==="

# --- TEST 1: Sequential (BTC-USDT) ---
echo ""
echo "=== TEST 1: Sequential ($N orders, BTC-USDT) ==="
for i in $(seq 1 $N); do
  P=$((80000 + i))
  curl -s -X POST "$API/api/orders" -H "Content-Type: application/json" \
    -d "{\"user_id\":\"user-1\",\"pair_id\":\"BTC-USDT\",\"side\":\"Sell\",\"order_type\":\"Limit\",\"price\":\"$P\",\"quantity\":\"0.001\"}" > /dev/null
done
sleep 3
> "$LOG"
for i in $(seq 1 $N); do
  P=$((80000 + i))
  curl -s -X POST "$API/api/orders" -H "Content-Type: application/json" \
    -d "{\"user_id\":\"user-2\",\"pair_id\":\"BTC-USDT\",\"side\":\"Buy\",\"order_type\":\"Limit\",\"price\":\"$P\",\"quantity\":\"0.001\"}" > /dev/null
done
sleep 5

SEQ_LOG=/tmp/sme-seq.log
cp "$LOG" "$SEQ_LOG"

# --- TEST 2: Burst (ETH-USDT) ---
echo "=== TEST 2: Burst ($N concurrent orders, ETH-USDT) ==="
for i in $(seq 1 $N); do
  P=$((5000 + i))
  curl -s -X POST "$API/api/orders" -H "Content-Type: application/json" \
    -d "{\"user_id\":\"user-3\",\"pair_id\":\"ETH-USDT\",\"side\":\"Sell\",\"order_type\":\"Limit\",\"price\":\"$P\",\"quantity\":\"0.01\"}" > /dev/null
done
sleep 3
> "$LOG"
for i in $(seq 1 $N); do
  P=$((5000 + i))
  curl -s -X POST "$API/api/orders" -H "Content-Type: application/json" \
    -d "{\"user_id\":\"user-4\",\"pair_id\":\"ETH-USDT\",\"side\":\"Buy\",\"order_type\":\"Limit\",\"price\":\"$P\",\"quantity\":\"0.01\"}" > /dev/null &
done
wait
sleep 5

BURST_LOG=/tmp/sme-burst.log
cp "$LOG" "$BURST_LOG"

# --- Analysis ---
python3 << 'PYEOF'
import json

def analyze(path, label):
    segments = {'queue_latency_us':[], 'parse_us':[], 'validate_us':[], 'db_pre_us':[], 'lua_us':[], 'cache_us':[], 'total_us':[]}
    errors = 0
    with open(path) as f:
        for line in f:
            if "segments" in line:
                try:
                    data = json.loads(line.strip())
                    fld = data.get("fields", {})
                    for key in segments:
                        if key in fld:
                            segments[key].append(int(fld[key]))
                except: pass
            if '"level":"ERROR"' in line:
                errors += 1

    n = len(segments["total_us"])
    print(f"\n{'='*70}")
    print(f"  {label}: {n} orders profiled, {errors} errors")
    print(f"{'='*70}")
    if n == 0:
        print("  No profiled orders found!")
        return

    print(f'\n  {"Segment":<20} {"Min":>8} {"Median":>8} {"p95":>8} {"Max":>8}  {"% med":>7}')
    print(f"  {'-'*68}")

    totals = sorted(segments["total_us"])
    totals_med = totals[n//2]

    for key in ["queue_latency_us","parse_us","validate_us","db_pre_us","lua_us","cache_us","total_us"]:
        vals = sorted(segments[key])
        if not vals: continue
        c = len(vals)
        pct = f"{vals[c//2]/totals_med*100:.1f}%" if key != "total_us" else "100%"
        label_s = key.replace("_us","").replace("_"," ")
        print(f"  {label_s:<20} {vals[0]:>8} {vals[c//2]:>8} {vals[int(c*0.95)]:>8} {vals[-1]:>8}  {pct:>7}")

analyze("/tmp/sme-seq.log", "Sequential (50 orders)")
analyze("/tmp/sme-burst.log", "Burst (50 concurrent)")

print("\n" + "="*70)
print("  BEFORE (reference):")
print("  Sequential: total=20,491  cache=13,577  db_pre=4,027  lua=2,394")
print("  Burst:      total=20,482  cache=11,025  db_pre=3,828  lua=4,921  queue=541,837")
print("="*70)
PYEOF

kill $WPID $GPID 2>/dev/null
wait $WPID $GPID 2>/dev/null
echo ""
echo "=== Done ==="
