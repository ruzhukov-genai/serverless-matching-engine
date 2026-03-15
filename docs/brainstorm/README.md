# Brainstorm

Open questions, explorations, and half-formed ideas. Nothing here is decided.

## Resolved ✅

- **Language:** Rust (tokio async) — chosen for perf + safety
- **Cache/Queue:** Dragonfly (Redis-compatible, multi-threaded)
- **Database:** PostgreSQL (ACID, Aurora migration path)
- **Stateless workers:** Load-per-invocation from Dragonfly → ADR-001
- **Matching:** Atomic Lua EVAL in Dragonfly → ADR-004
- **Architecture:** Gateway/Worker split — gateway serves HTTP/WS, worker consumes queues
- **Queue mechanism:** Per-pair BRPOP queues (replaced Streams) → ADR-005
- **DB persistence:** Async, off hot path → ADR-003
- **Gateway scaling:** Shared cache broadcasts → ADR-002

## Open Questions

### Deployment
- **Lambda cold start:** How does ~5ms pairs config load + Dragonfly pool creation affect Lambda latency?
- **Lambda concurrency:** Per-pair invocations or shared workers? Semaphore model (ADR-005) may not apply.
- **SQS vs Dragonfly queues:** Lambda trigger from SQS vs BRPOP? SQS has built-in retry/DLQ.

### Reliability
- **Crash recovery:** Worker crashes after Lua match but before PG persist. Dragonfly has the trade, PG doesn't. Need WAL or idempotent replay for production.
- **Dragonfly persistence:** RDB snapshots only (no AOF). Acceptable since PG is source of truth.
- **Multi-gateway order events:** Currently `tokio::broadcast` (single-process). Need Dragonfly pub/sub for multi-gateway.

### Performance
- **Tokio runtime contention:** At 100+ clients, pure scheduling overhead dominates (~280ms for in-memory reads). Multi-threaded runtime? Separate gateway instances?
- **Batch matching:** Accumulate N orders per pair, match in one Lua call. Better throughput but more complex.
- **Dragonfly Lua serialization:** Single-threaded Lua executor causes p95 spikes under concurrent EVALs.

### Features
- **FOK orders:** Not implemented in worker (auto-cancelled). Need speculative matching in Lua or OCC/CAS path.
- **Event sourcing:** Reconstruct order book from audit trail for compliance.
- **Rate limiting:** Per-user order rate limits in gateway.

## References

- [Dragonfly](https://www.dragonflydb.io/) — multi-threaded Redis replacement
- [redis-rs](https://docs.rs/redis/) — Rust Redis client
- [sqlx](https://docs.rs/sqlx/) — Rust async PostgreSQL
- [axum](https://docs.rs/axum/) — Rust web framework
