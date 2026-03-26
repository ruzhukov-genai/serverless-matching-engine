# BENCHMARK.md — SME Benchmark Rules

Project-specific benchmarking configuration for serverless-matching-engine.
Read by the `benchmarking` skill. Do not duplicate general methodology here.

## Environment

### Local (Docker Compose)
- **Gateway:** `http://localhost:3001` (sme-gateway, port 3001)
- **Worker:** sme-api (BRPOP queue consumer)
- **Valkey:** `localhost:6379` (Docker: sme-valkey)
- **PostgreSQL:** `localhost:5432` (Docker: sme-postgres, user=sme, pass=sme_dev, db=matching_engine)
- **Build:** `cargo build --release` before benchmarking

### AWS
- **API:** `https://kpvhsf0ub8.execute-api.us-east-1.amazonaws.com`
- **WebSocket:** `wss://2shnq9yk0c.execute-api.us-east-1.amazonaws.com/ws`
- **Worker Lambda manage commands:** `aws lambda invoke --function-name serverless-matching-engine-worker`

## Benchmark Tool

```bash
# Local — full lifecycle
python3 tools/benchmark.py --ws --duration 30 --clients "1,10,25,40,100" \
  --db-url "postgresql://sme:sme_dev@localhost:5432/matching_engine"

# AWS
python3 tools/benchmark.py --ws --duration 30 --clients "1,10,40,100" \
  --api https://kpvhsf0ub8.execute-api.us-east-1.amazonaws.com \
  --ws-url wss://2shnq9yk0c.execute-api.us-east-1.amazonaws.com/ws \
  --lambda
```

## Pre-Run Checklist

1. **Reset data** — truncate orders/trades/audit_log, reset balances to 1,000,000
   - Local: `docker exec sme-postgres psql -U sme -d matching_engine -c "TRUNCATE orders CASCADE; TRUNCATE trades CASCADE; TRUNCATE audit_log CASCADE; UPDATE balances SET available = 1000000, locked = 0;"`
   - AWS: invoke worker with `{"manage":{"command":"reset_all"}}`
2. **Restart worker** after reset (so cache re-seeds from clean PG)
3. **Verify services** — `curl -s http://localhost:3001/api/pairs` returns JSON
4. **Use `--run-id`** for tagging (auto-generates `bench-YYYYMMDD-HHMM` if omitted)

## Pair & Order Config

- **Pair:** BTC-USDT
- **Order pattern:** Alternating buy/sell GTC limit orders
- **Prices:** Bids ~70600, asks ~70800 (crossable with seeded liquidity)
- **Size:** 0.001 BTC per order
- **Seeded liquidity:** 140 resting orders (10 price levels × 5 orders/side passive + 40 active at crossing prices)
- **Users:** 10 test users (`user-1` through `user-10`)

## DB Lifecycle Timestamps

Three columns in `orders` table:
- `received_at` — gateway receives order from client
- `matched_at` — Lua EVAL completes (matched or placed in book)
- `persisted_at` — PG transaction commits (`NOW()` inside INSERT)

### Key Queries (filter by run)
```sql
-- Summary by status
SELECT status, count(*) FROM orders
WHERE client_order_id LIKE 'bench-{run_id}%' AND client_order_id NOT LIKE '%seed%'
GROUP BY status;

-- Lifecycle percentiles
SELECT
  count(*) as orders,
  percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at)*1000)::int as match_p50,
  percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from matched_at - received_at)*1000)::int as match_p99,
  percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - matched_at)*1000)::int as persist_p50,
  percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - matched_at)*1000)::int as persist_p99,
  percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - received_at)*1000)::int as e2e_p50,
  percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - received_at)*1000)::int as e2e_p99
FROM orders WHERE client_order_id LIKE 'bench-{run_id}%' AND client_order_id NOT LIKE '%seed%'
  AND matched_at IS NOT NULL AND persisted_at IS NOT NULL;

-- Trade count
SELECT count(*) FROM trades t
WHERE EXISTS (SELECT 1 FROM orders o WHERE o.id = t.buy_order_id AND o.client_order_id LIKE 'bench-{run_id}%' AND o.client_order_id NOT LIKE '%seed%')
   OR EXISTS (SELECT 1 FROM orders o WHERE o.id = t.sell_order_id AND o.client_order_id LIKE 'bench-{run_id}%' AND o.client_order_id NOT LIKE '%seed%');
```

## Known Bottlenecks

1. **Balance row contention** — 10 test users means high `UPDATE balances` lock contention at c≥40. Real workload (1000+ users) would be much faster.
2. **AWS cache rebuild** — ~300ms per order (orderbook snapshot + 3 PG queries + Valkey writes). Dominates gateway latency.
3. **ElastiCache network latency** — Lua EVAL ~15ms on AWS vs ~1-8ms local (~10ms VPC network hop).
4. **Lambda Web Adapter** — `tokio::spawn` for background tasks doesn't work (LWA freezes runtime after response). All async work must complete inline.

## Baselines

Historical baselines in `docs/benchmarks/`. Save new baselines after significant changes:
```
docs/benchmarks/YYYY-MM-DD-description.md
```

### Current Local Baseline (2026-03-26)
```
         Match p50  Persist p50  E2E p50    Throughput
c=1      1.4ms      0.3ms        1.7ms      2.0/s
c=40     6.9ms      9.6ms        19.2ms     78.7/s
c=100    8.1ms      20.8ms       32.8ms     193.3/s
```
