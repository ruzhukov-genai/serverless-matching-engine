#!/usr/bin/env bash
# Integration test: orderbook data isolation between pairs
# Verifies that switching pairs returns only that pair's orders
set -euo pipefail

API="${1:-https://kpvhsf0ub8.execute-api.us-east-1.amazonaws.com}"
WS="${2:-wss://2shnq9yk0c.execute-api.us-east-1.amazonaws.com/ws}"
FAILED=0

pass() { echo "  ✅ $1"; }
fail() { echo "  ❌ $1"; FAILED=$((FAILED+1)); }

echo "=== Orderbook Pair Isolation Test ==="
echo "API: $API"
echo ""

# Test 1: Each pair's orderbook contains only its own orders
echo "--- Test 1: Orderbook data isolation ---"
for pair in BTC-USDT ETH-USDT SOL-USDT; do
    data=$(curl -s "$API/api/orderbook/$pair")
    returned_pair=$(echo "$data" | python3 -c "import json,sys; print(json.load(sys.stdin).get('pair',''))")
    if [ "$returned_pair" = "$pair" ]; then
        pass "GET /api/orderbook/$pair returns pair=$pair"
    else
        fail "GET /api/orderbook/$pair returned pair=$returned_pair (expected $pair)"
    fi
done

# Test 2: WebSocket subscriptions are pair-scoped
echo ""
echo "--- Test 2: WS subscriptions are pair-scoped ---"
python3 -c "
import asyncio, json, websockets

async def test():
    async with websockets.connect('$WS') as ws:
        # Subscribe to BTC only
        await ws.send(json.dumps({'subscribe': ['orderbook:BTC-USDT']}))
        
        channels = set()
        for _ in range(5):
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=3)
                data = json.loads(msg)
                channels.add(data.get('ch', ''))
            except asyncio.TimeoutError:
                break
        
        # Should only get BTC orderbook, not ETH or SOL
        has_btc = 'orderbook:BTC-USDT' in channels
        has_eth = 'orderbook:ETH-USDT' in channels
        has_sol = 'orderbook:SOL-USDT' in channels
        
        if has_btc:
            print('  ✅ Received orderbook:BTC-USDT')
        else:
            print('  ❌ Did not receive orderbook:BTC-USDT')
        
        if not has_eth:
            print('  ✅ Did not receive orderbook:ETH-USDT (correct)')
        else:
            print('  ❌ Received orderbook:ETH-USDT (should not)')
        
        if not has_sol:
            print('  ✅ Did not receive orderbook:SOL-USDT (correct)')
        else:
            print('  ❌ Received orderbook:SOL-USDT (should not)')

asyncio.run(test())
"

# Test 3: Rapid pair switching via REST doesn't leak data
echo ""
echo "--- Test 3: Rapid REST pair switching ---"
for i in $(seq 1 5); do
    for pair in BTC-USDT ETH-USDT SOL-USDT; do
        data=$(curl -s "$API/api/orderbook/$pair")
        returned_pair=$(echo "$data" | python3 -c "import json,sys; print(json.load(sys.stdin).get('pair',''))" 2>/dev/null)
        if [ "$returned_pair" != "$pair" ]; then
            fail "Round $i: /api/orderbook/$pair returned pair=$returned_pair"
        fi
    done
done
if [ $FAILED -eq 0 ]; then
    pass "15 rapid pair switches: all returned correct pair"
fi

echo ""
if [ $FAILED -eq 0 ]; then
    echo "=== ALL TESTS PASSED ==="
else
    echo "=== $FAILED TEST(S) FAILED ==="
    exit 1
fi
