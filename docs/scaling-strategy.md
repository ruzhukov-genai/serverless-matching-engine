# Scaling Strategy — Serverless Matching Engine

> **Goal:** Define a clear, tested path from current throughput (~135/s) to 25,000 orders/sec
> with predictable cost at each tier.

## Current State (March 2026)

### Infrastructure
| Component | Spec | Cost/mo |
|-----------|------|---------|
| RDS PostgreSQL | `db.t4g.small` (2 vCPU, 2GB, ~52 conns) | $24 |
| ElastiCache Valkey | `cache.t4g.micro` (2 vCPU, 0.5GB), manually managed | $9 |
| RDS Proxy | `sme-proxy-v2` | $22 |
| Lambda Gateway | 512MB, unreserved | pay-per-use |
| Lambda Worker | 1024MB, 120s timeout, unreserved | pay-per-use |
| SQS FIFO | `serverless-matching-engine-orders.fifo` | pay-per-use (~$0) |
| API Gateway | HTTP API | pay-per-use |
| **Total fixed** | | **~$55/mo** |

### Measured Performance (March 26, 2026)

**Sync mode:**

| Metric | c=1 | c=10 | c=40 |
|--------|-----|------|------|
| Match p50 (Lua EVAL) | ~17ms | ~17ms | ~17ms |
| Persist p50 | 0.3ms | ~1ms | 2.7ms |
| Errors | 0 | 0 | 0 |

**Queue mode:**

| Metric | Value |
|--------|-------|
| Gateway ceiling | ~220 ord/s |
| Worker drain | ~100 ord/s (5 parallel × 20 ord/s) |

**Balance contention reduction:**
- 10 users: persist p99 = 29ms
- 100 users: persist p99 = 15ms (less row-level lock contention)

**Bottleneck chain (in order, sync mode):**
1. **PG persist latency** — p50=2.7ms at c=40; row-level lock contention eases with 100+ users
2. **Balance contention** — fewer distinct users = more lock wait; use `{"users": 100}` for benchmarks
3. **Valkey single-thread** — Lua EVAL is ~17ms (includes network RTT); theoretical max ~60 EVAL/s per shard at this latency
4. **Queue mode ceiling** — ~220 ord/s gateway dispatch; worker drain ~100 ord/s (SQS-direct bypasses this)

### Throughput Formula

```
Sustained throughput = concurrent_workers × (1000ms / avg_worker_duration_ms)

At c=10:  10 × (1000/125) = 80/s  (actual: 136/s due to pipelining)
At c=100: 100 × (1000/125) = 800/s (actual: 1374/s)
```

The actual numbers exceed the simple formula because Lua EVAL + PG persist partially overlap
with cache updates. But PG connections are the hard ceiling.

---

## Scaling Tiers

### Tier 0: Current — 135 orders/sec
**What we have now.** Sufficient for development and demo.

### Tier 1: 500 orders/sec (Target: POC demo, light production)

**Changes required:**

| Change | Why | Cost delta |
|--------|-----|------------|
| RDS → `db.t4g.medium` | 112 max_connections (supports c=20 workers × 5 conns) | +$36/mo |
| Worker reserved concurrency = 20 | Prevent stampede, allow 20 parallel workers | $0 |
| Fire-and-forget Lambda invoke | Reduce client latency 72ms → 22ms | $0 (code change) |

**Expected result:**
- 20 workers × (1000/125ms) ≈ 160/s baseline, ~500/s with pipelining
- PG: 20 × 5 = 100 connections (within 112 limit)
- Valkey: 20 concurrent Lua EVALs × 1ms = fine (single-thread handles ~1000/s)

**Estimated monthly cost:** ~$57/mo (+$36)

**What to test:**
- [ ] Benchmark at c=20 with `db.t4g.medium`
- [ ] Verify PG persist p99 improves with more headroom
- [ ] Implement fire-and-forget invoke, measure client latency drop

---

### Tier 2: 2,000 orders/sec (Target: Early production)

**SQS dispatch is now implemented** (as of 2026-03-26, ADR-008). `sqs-direct` mode eliminates Gateway Lambda from the order submission path entirely — API Gateway routes `POST /api/orders` directly to SQS FIFO, Worker Lambda picks up via event source mapping (batch size 10).

**Remaining changes for full Tier 2:**

| Change | Why | Cost delta |
|--------|-----|------------|
| RDS → `db.t4g.large` | 236 max_connections (supports c=40 × 5) | +$48/mo vs Tier 1 |
| Worker reserved concurrency = 40 | 40 parallel workers | $0 |
| Batch PG persist (multi-row INSERT) | Reduce per-order persist overhead | $0 (code change) |
| Valkey → `cache.t4g.small` | More memory for larger order books | +$14/mo |

**Already done:**
- ✅ RDS Proxy (`sme-proxy-v2`) — connection pooling for burst scaling ($22/mo, already in prod)
- ✅ SQS FIFO dispatch (ADR-008) — `sqs-direct` deployed, Worker ESM batch=10

**Expected result:**
- 40 workers × (1000/80ms batched) ≈ 500/s baseline, ~2000/s with SQS batching
- SQS allows workers to process batches of 10 orders → amortize PG round-trip
- RDS Proxy handles connection multiplexing for burst to 100+ Lambdas

**Estimated monthly cost:** ~$142/mo (RDS Proxy already included in current ~$55/mo base)

**What to test:**
- [ ] Benchmark `sqs-direct` mode at c=40 sustained
- [ ] Measure batch persist improvement (expect p50 ≤ 5ms per order with batching)
- [ ] Verify Worker ESM backpressure (SQS VisibilityTimeout ≥ Lambda timeout=120s ✅)
- [ ] Load test at c=40 sustained for 5 minutes

**Remaining code changes:**
- Worker persist: multi-row INSERT for orders + trades in single statement

---

### Tier 3: 5,000 orders/sec (Target: Production)

**Changes required:**

| Change | Why | Cost delta |
|--------|-----|------------|
| RDS → `db.r7g.large` | 683 max_connections, dedicated memory | +$101/mo vs Tier 2 |
| RDS Multi-AZ | Production reliability | +$197/mo |
| Worker reserved concurrency = 100 | 100 parallel workers | $0 |
| Valkey → `cache.t4g.medium` | 3.1GB for production order books | +$23/mo |
| Provisioned Gateway concurrency = 10 | Eliminate cold starts | +$11/mo |
| PG: partition orders table by date | Keep active table small | $0 (migration) |

**Expected result:**
- 100 workers × batch processing ≈ 5000/s sustained
- PG has ample connection headroom (100 workers × 5 = 500 < 683)
- Order table partitioning prevents bloat from degrading INSERT performance

**Estimated monthly cost:** ~$474/mo (+$332 vs Tier 2)

**What to test:**
- [ ] Sustained 5min load at 5000/s — verify no queue growth
- [ ] PG IOPS consumption (gp3 baseline is 3000 IOPS, may need provisioned)
- [ ] Valkey memory usage under 1M+ order book entries
- [ ] Table partitioning migration (zero-downtime via partition swaps)

---

### Tier 4: 10,000 orders/sec (Target: Scale production)

**Changes required:**

| Change | Why | Cost delta |
|--------|-----|------------|
| RDS → `db.r7g.xlarge` | 4 vCPU, 1365 connections | +$197/mo vs Tier 3 |
| RDS provisioned IOPS (6000) | Sustained write throughput | +$60/mo |
| Worker reserved concurrency = 200 | 200 parallel workers | $0 |
| Valkey → `cache.r7g.large` (+ read replica) | Separate read/write paths | +$320/mo |
| Multi-pair partitioning | Separate SQS queues per pair | $0 (code change) |

**Expected result:**
- 200 workers × batch processing ≈ 10,000/s
- Per-pair SQS queues prevent hot-pair blocking cold pairs
- Valkey read replica offloads gateway snapshot reads from matching writes

**Estimated monthly cost:** ~$1,051/mo (+$577 vs Tier 3)

**Architecture change: Per-pair worker scaling**
- Each trading pair gets its own SQS queue
- Worker concurrency scales per pair based on volume
- Hot pairs (BTC-USDT) get more workers, cold pairs share
- This matches ADR-005 (per-pair queue consumers with bounded concurrency)

---

### Tier 5: 25,000 orders/sec (Target: High-scale production)

**Changes required:**

| Change | Why | Cost delta |
|--------|-----|------------|
| RDS → `db.r7g.2xlarge` | 8 vCPU, 2730 connections | +$394/mo vs Tier 4 |
| RDS provisioned IOPS (12000) | Handle 25K write ops/sec | +$60/mo |
| PG: async persist via Kinesis/Firehose | Decouple matching from persistence entirely | ~$50/mo |
| Worker reserved concurrency = 500 | 500 parallel workers | $0 |
| Valkey cluster mode (3 shards) | Shard Lua EVAL across pairs | +$500/mo |
| Dedicated VPC endpoints | Remove NAT Gateway bottleneck | audit existing |

**Expected result:**
- 500 workers with async persistence ≈ 25,000/s
- Matching (Lua EVAL) is ~1ms; with 3 Valkey shards, theoretical max = 3000 EVAL/s per shard
- PG persistence fully async — worker returns after Lua EVAL + cache update, PG write goes to Kinesis

**Estimated monthly cost:** ~$2,055/mo (+$1,004 vs Tier 4)

**Architecture changes:**
- **Async persistence pipeline:** Worker → Kinesis Data Stream → Lambda consumer → PG batch INSERT
- **Valkey cluster mode:** Hash-slot routing by pair_id, each shard handles ~8K EVAL/s
- **Gateway reads from Valkey only** — no PG reads in hot path at all
- This is a significant architecture shift — requires new ADR

**Risk: Lua EVAL serialization**
At 25K/s across 10 pairs, the hottest pair might see 5K EVAL/s. Single Valkey node handles
~1000 complex EVAL/s. Solutions:
- Cluster mode with pair-based sharding (3+ shards)
- Simplify Lua script (pre-compute outside Lua, minimize EVAL scope)
- Consider moving matching to Worker RAM (breaks stateless ADR-001 — needs new ADR)

---

## Summary Table

| Tier | Target | Workers | RDS | Valkey | Monthly Cost | Key Change |
|------|--------|---------|-----|--------|-------------|------------|
| 0 | ~220/s (queue) | — | t4g.small + Proxy | t4g.micro | $55 | **Current** (sqs-direct deployed) |
| 1 | 500/s | 20 | t4g.medium | t4g.micro | $81 | Bigger RDS + fire-and-forget |
| 2 | 2,000/s | 40 | t4g.large | t4g.small | $142 | Batch persist (SQS already done) |
| 3 | 5,000/s | 100 | r7g.large (Multi-AZ) | t4g.medium | $474 | Production hardening |
| 4 | 10,000/s | 200 | r7g.xlarge + PIOPS | r7g.large + replica | $1,051 | Per-pair scaling |
| 5 | 25,000/s | 500 | r7g.2xlarge + PIOPS | 3-shard cluster | $2,055 | Async persist + cluster |

## Testing Plan

Each tier should be validated before committing to the next:

1. **Benchmark** at target throughput for 5 minutes sustained
2. **Verify queue depth stays at 0** (no growing backlog)
3. **Check PG connection count** stays below 80% of max_connections
4. **Check Valkey memory** and EVAL latency under load
5. **Record lifecycle timestamps** (received_at → matched_at → persisted_at)
6. **Document results** in `docs/benchmarks/tier-N-results.md`

### Benchmark command template
```bash
./target/release/sme-loadtest \
  --url https://kpvhsf0ub8.execute-api.us-east-1.amazonaws.com \
  --concurrency {WORKERS} \
  --duration 300 \
  --label "Tier {N} validation (c={WORKERS})"
```

### Post-benchmark analysis
```sql
-- Lifecycle latency breakdown
SELECT
  count(*) as total_orders,
  avg(extract(epoch from matched_at - received_at) * 1000)::int as avg_dispatch_ms,
  avg(extract(epoch from persisted_at - matched_at) * 1000)::int as avg_persist_ms,
  avg(extract(epoch from persisted_at - received_at) * 1000)::int as avg_e2e_ms,
  percentile_cont(0.99) WITHIN GROUP (ORDER BY extract(epoch from persisted_at - received_at) * 1000)::int as p99_e2e_ms
FROM orders WHERE received_at IS NOT NULL;
```

## Immediate Next Steps (Tier 0 → Tier 1)

1. **Set worker reserved concurrency = 20** (prevent stampede)
2. **Upgrade RDS to `db.t4g.medium`** ($36/mo increase)
3. **Implement fire-and-forget invoke** (code change, 0 cost)
4. **Benchmark at c=20** for 5 minutes → validate 500/s sustained
5. **Document results** → decide if Tier 2 work is needed

## Decision Log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2026-03-24 | Created scaling strategy | Need clear path from POC to 25K/s |
| 2026-03-26 | Implemented SQS dispatch (ADR-008) | Eliminate gateway Lambda from order path, near-zero dispatch latency |
| 2026-03-26 | Made cache update non-fatal | 13% error rate at high concurrency traced to cache update; stale cache acceptable |
| 2026-03-26 | RDS upgraded to db.t4g.small + Proxy | Persistent pool + proxy already in prod; updated cost baseline |
