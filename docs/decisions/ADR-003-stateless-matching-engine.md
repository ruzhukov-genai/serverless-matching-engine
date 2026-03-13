# ADR-003: Stateless Matching Engine Mode

**Status:** Accepted

## Context

To run the Matching Engine as a stateless worker (Lambda, container, or tokio task), it must not hold in-memory state between invocations. Each invocation should load what it needs, process, and finalize.

## Decision

Introduce a `STATELESS_MODE` flag (environment variable). When enabled:
1. Load Order Book from Dragonfly cache (PostgreSQL fallback) on invocation start
2. Process the order (match under lock)
3. Write results to cache + DB before returning

## Consequences

**Benefits:**
- Enables any deployment model (Lambda, containers, bare tokio tasks)
- Simplifies horizontal scaling — workers are interchangeable

**Risks:**
- Higher latency per invocation (load + process + write vs. in-memory)
- Order Book load must be optimized (lazy loading, price-filtered queries)

## Optimizations Planned

- Load only relevant side of book (buy/sell) filtered by price
- Lazy loading: 10 → 20 → 40 orders
- Dragonfly sorted sets for price-ordered retrieval

## Follow-ups

- [ ] Benchmark load latency for various order book sizes
- [ ] Define lazy-load batch sizes
- [ ] Test with `STATELESS_MODE=false` (backward-compatible, in-memory book)
