#!/bin/bash
# End-to-end latency profiling: submit orders and capture segment timings from worker logs
set -e

API="http://localhost:3001"
PAIR="BTC-USDT"
N=20  # number of order pairs to submit

echo "=== Latency Profiling: $N order pairs on $PAIR ==="
echo ""

# Clear old worker logs
> /tmp/worker_segments.log

# Seed some sell orders first (resting), then buy orders to match
for i in $(seq 1 $N); do
  PRICE=$((50000 + i))
  # Sell (resting)
  curl -s -X POST "$API/api/orders" \
    -H "Content-Type: application/json" \
    -d "{\"user_id\":\"latency-seller\",\"pair_id\":\"$PAIR\",\"side\":\"Sell\",\"order_type\":\"Limit\",\"price\":\"$PRICE\",\"quantity\":\"0.001\"}" > /dev/null &
done
wait
sleep 1

echo "Resting orders placed. Now submitting matching buys..."

for i in $(seq 1 $N); do
  PRICE=$((50000 + i))
  # Buy (will match the sell)
  curl -s -X POST "$API/api/orders" \
    -H "Content-Type: application/json" \
    -d "{\"user_id\":\"latency-buyer\",\"pair_id\":\"$PAIR\",\"side\":\"Buy\",\"order_type\":\"Limit\",\"price\":\"$PRICE\",\"quantity\":\"0.001\"}" > /dev/null &
done
wait
sleep 2

echo ""
echo "=== Segment Timings (from worker logs) ==="
echo ""
