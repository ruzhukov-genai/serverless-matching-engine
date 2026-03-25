# Brainstorm

Open questions, explorations, and half-formed ideas. Nothing here is decided.

## Resolved ✅

- **Language:** Rust (tokio async) — chosen for perf + safety
- **Cache/Queue:** Valkey (Redis-compatible, multi-threaded)
- **Database:** PostgreSQL (ACID, Aurora migration path)
- **Stateless workers:** Load-per-invocation from Valkey → ADR-001
- **Matching:** Atomic Lua EVAL in Valkey → ADR-004
- **Architecture:** Gateway/Worker split — gateway serves HTTP/WS, worker consumes queues
- **Queue mechanism:** Per-pair BRPOP queues (replaced Streams) → ADR-005
- **DB persistence:** Async, off hot path → ADR-003
- **Gateway scaling:** Shared cache broadcasts → ADR-002
- **Lambda cold start:** ~43ms first invoke, ~5ms warm; acceptable for async processing
- **Lambda concurrency:** Direct async invoke (no queues); Lambda handles concurrency scaling
- **SQS vs Valkey queues:** Direct Lambda invoke wins (5ms vs 50-100ms SQS)
- **Deployment:** AWS SAM with nested stacks; Docker ARM64 cross-compilation

## Open Questions

### Deployment
- **Production monitoring:** CloudWatch metrics, distributed tracing, error alerting
- **Auto-scaling:** Lambda concurrency limits, API Gateway throttling

### Reliability
- **Crash recovery:** Worker crashes after Lua match but before PG persist. Valkey has the trade, PG doesn't. Need WAL or idempotent replay for production.
- **Valkey persistence:** RDB snapshots only (no AOF). Acceptable since PG is source of truth.
- **Multi-gateway order events:** Currently `tokio::broadcast` (single-process). Need Valkey pub/sub for multi-gateway.

### Performance
- **Tokio runtime contention:** At 100+ clients, pure scheduling overhead dominates (~280ms for in-memory reads). Multi-threaded runtime? Separate gateway instances?
- **Batch matching:** Accumulate N orders per pair, match in one Lua call. Better throughput but more complex.
- **Valkey Lua serialization:** Single-threaded Lua executor causes p95 spikes under concurrent EVALs.

### Features
- **FOK orders:** Not implemented in worker (auto-cancelled). Need speculative matching in Lua or OCC/CAS path.
- **Event sourcing:** Reconstruct order book from audit trail for compliance.
- **Rate limiting:** Per-user order rate limits in gateway.

## References

- [Valkey](https://www.valkeydb.io/) — multi-threaded Redis replacement
- [redis-rs](https://docs.rs/redis/) — Rust Redis client
- [sqlx](https://docs.rs/sqlx/) — Rust async PostgreSQL
- [axum](https://docs.rs/axum/) — Rust web framework
