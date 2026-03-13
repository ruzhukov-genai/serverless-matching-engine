# ADR-003: Stateless Matching Engine Mode

**Status:** Proposed

## Context

To run the Matching Engine as a Lambda, it must not hold in-memory state between invocations. Each invocation should load what it needs, process, and finalize.

## Decision

Introduce a `StatelessMode` flag (environment variable / CDK stack parameter). When enabled:
1. Load Order Book from Redis cache (DB fallback) on invocation start
2. Process the order
3. Write results before returning

## Consequences

**Benefits:**
- Enables Lambda deployment
- Simplifies horizontal scaling

**Risks:**
- Higher latency per invocation (load + process + write vs. in-memory)
- Order Book load must be optimized (lazy loading, price-filtered queries)

## Optimizations Planned

- Load only relevant side of book (buy/sell) filtered by price
- Lazy loading: 10 → 20 → 40 orders
- Redis sorted sets for price-ordered retrieval

## Follow-ups

- [ ] Benchmark load latency for various order book sizes
- [ ] Define lazy-load batch sizes
- [ ] Test with `StatelessMode=false` (backward-compatible)
