# Brainstorm

Open questions, explorations, and half-formed ideas. Nothing here is decided.

## Resolved ✅

- **Language:** Rust (tokio async) → [ADR-005](../decisions/ADR-005-rust-implementation.md)
- **Cache/Queue:** Dragonfly (Redis-compatible, 10-25x throughput) → [ADR-004](../decisions/ADR-004-dragonfly-postgresql.md)
- **Database:** PostgreSQL (ACID, indexing, Aurora migration path) → [ADR-004](../decisions/ADR-004-dragonfly-postgresql.md)
- **Queue mechanism:** Dragonfly Streams (XADD/XREADGROUP) — no separate queue service needed
- **Stateless mode:** Load-per-invocation with cache → [ADR-003](../decisions/ADR-003-stateless-matching-engine.md)

## Open Questions

### Matching Engine
- **Order book size limits?**
  - Lazy loading breaks down when a single order fills across 100s of price levels
  - May need a "full load" fallback threshold

### Dragonfly-specific
- **Streams consumer group behavior under high contention?**
  - Need to benchmark XREADGROUP with many concurrent consumers per group
  - Pending Entry List (PEL) management and XACK patterns

- **Persistence strategy?**
  - Dragonfly uses RDB snapshots only (no AOF)
  - Acceptable since Dragonfly is cache/queue layer; PostgreSQL is source of truth
  - Define snapshot interval for local dev

### Observability
- How do we trace a single order across 3 services + API?
  - Correlation IDs (order_id / trace_id) propagated through all streams
  - `tracing` crate with OpenTelemetry exporter for local dev

### Schema Design
- PostgreSQL schema for orders: partitioning by pair_id?
- Order versioning: integer version column vs. event sourcing?
- Trade event schema for stream messages

## Ideas to Explore

- [ ] Event sourcing for order book reconstruction from audit trail
- [ ] WebSocket push for real-time order status updates (implemented in `crates/api`)
- [ ] Chaos testing: lock expiry under load
- [ ] Dragonfly cluster mode for horizontal scaling
- [ ] Benchmarking Dragonfly Streams vs. dedicated message broker at scale

## References

- [Dragonfly](https://www.dragonflydb.io/) — multi-threaded Redis replacement
- [Dragonfly Streams docs](https://www.dragonflydb.io/docs/category/streams)
- [RedLock](https://redis.io/docs/latest/develop/use/patterns/distributed-locks/) — distributed locking algorithm
- [redis-rs](https://docs.rs/redis/) — Rust Redis client
- [sqlx](https://docs.rs/sqlx/) — Rust async PostgreSQL with compile-time checked queries
- [axum](https://docs.rs/axum/) — Rust web framework
