# Spec: Matching Engine

## What It Does

Takes an incoming order, loads the relevant side of the order book from cache, matches at price-time priority, and writes results back.

## Stateless Cycle

```
1. XREADGROUP stream:orders:match (receive order)
2. SET book:{pair_id}:lock {workerId} NX EX 1 (acquire lock)
3. ZRANGEBYSCORE book:{pair_id}:{opposite_side} (load matching orders)
4. Match: walk the book, fill at each price level
5. Update cache: ZADD/ZREM modified orders
6. INCR book:{pair_id}:version
7. Write trades: XADD stream:transactions
8. Async: persist to PostgreSQL (version-guarded)
9. DEL book:{pair_id}:lock (release)
10. XACK stream:orders:match (acknowledge)
```

## Matching Logic (Limit Orders)

- **BUY at $P**: match against asks where `price <= P`, lowest price first, then oldest first
- **SELL at $P**: match against bids where `price >= P`, highest price first, then oldest first
- Partial fills: reduce quantity, keep resting remainder in book
- Full fill: remove from book

## Lock Contention

If lock unavailable:
- Exponential backoff: `[10, 20, 50, 100, 200, 500]` ms
- Jitter: `delay *= (1 + 0.2 * random())`
- Max retries: 10 (then fail + requeue)

## Order Book Cache Structure

```
book:{pair_id}:bids  → ZSet  (score = price, member = order_id)
book:{pair_id}:asks  → ZSet  (score = price, member = order_id)
order:{order_id}     → Hash  {price, quantity, remaining, side, pair_id, timestamp, version}
```

## What We Need to Prove

- [ ] Correct matching under concurrent load (same pair)
- [ ] No fills beyond order quantity
- [ ] Version conflicts handled (stale write rejected)
- [ ] Lock expiry recovery (worker crash mid-match)
- [ ] Throughput: matches/sec single pair, scaling across pairs
