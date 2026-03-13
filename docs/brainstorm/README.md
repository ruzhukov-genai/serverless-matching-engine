# Brainstorm

Open questions, explorations, and half-formed ideas. Nothing here is decided.

## Resolved ✅

- **Cache/Queue:** Dragonfly (Redis-compatible, 10-25x throughput) → [ADR-004](../decisions/ADR-004-dragonfly-postgresql.md)
- **Database:** PostgreSQL (ACID, indexing, Aurora migration path) → [ADR-004](../decisions/ADR-004-dragonfly-postgresql.md)
- **Queue mechanism:** Dragonfly Streams (XADD/XREADGROUP) — no separate queue service needed

## Open Questions

### Matching Engine
- **Cold start impact on P99 latency?**
  - Provisioned concurrency: eliminates cold starts, adds cost
  - Keep-warm pings: hack but works for low-traffic pairs

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
- How do we trace a single order across 3 services?
  - Correlation IDs propagated through all streams
  - OpenTelemetry for local dev, X-Ray for AWS

### Schema Design
- PostgreSQL schema for orders: partitioning by pair_id?
- Order versioning: integer version column vs. event sourcing?
- Trade event schema for stream messages

## Ideas to Explore

- [ ] Event sourcing for order book reconstruction
- [ ] WebSocket push for order status updates
- [ ] Canary deployments per trading pair
- [ ] Chaos testing: lock expiry under load
- [ ] Dragonfly cluster mode for horizontal scaling

## References

- [Dragonfly](https://www.dragonflydb.io/) — multi-threaded Redis replacement
- [Dragonfly Streams docs](https://www.dragonflydb.io/docs/category/streams)
- RedLock: https://redis.io/docs/latest/develop/use/patterns/distributed-locks/
- [@es-node/redis-manager](https://www.npmjs.com/package/@es-node/redis-manager)
