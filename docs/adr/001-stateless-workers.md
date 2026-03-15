# ADR-001: Stateless Workers, Dragonfly as Primary Cache

**Status:** Accepted  
**Date:** 2026-03-15  
**Decision makers:** Roman Zhukov

## Context

Workers (sme-api) consume order queues, run matching, and persist results. The target deployment is AWS Lambda, where each invocation is ephemeral with no shared memory between invocations.

## Decision

**Workers MUST be stateless.** All mutable state lives in Dragonfly (order book, cache keys) and PostgreSQL (persistence). Workers may cache only:

- **Dictionary-style data that rarely changes** (e.g., pairs config loaded at startup)
- No in-memory order book replicas
- No in-memory trade caches
- No session/connection affinity

**Dragonfly is the primary cache layer.** Workers read/write order book state via Dragonfly sorted sets and hashes. Pre-computed cache keys (`cache:orderbook:*`, `cache:trades:*`, `cache:ticker:*`) are written by workers and read by gateways.

**PostgreSQL is the persistence layer.** DB writes are deferred off the hot path (see ADR-003). The DB is the source of truth for audit, balances, and historical data.

## Consequences

- Workers can scale horizontally (N Lambda invocations per pair)
- Cold start cost: must load pairs config from PG on each invocation (~5ms)
- No local caching of order book state — every match reads from Dragonfly
- Gateway (long-lived process) MAY cache Dragonfly reads in-memory (see ADR-002)
- Dragonfly must handle all concurrent worker connections (pool size matters)

## Alternatives Considered

- **In-memory order book per worker:** Faster matching but breaks stateless constraint. Would need sticky routing or book synchronization protocol.
- **Redis Streams instead of BRPOP:** Better at-least-once semantics but more complex. BRPOP sufficient for PoC.
