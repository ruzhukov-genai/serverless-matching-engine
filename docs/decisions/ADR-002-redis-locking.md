# ADR-002: Redis-based Order Book Locking

**Status:** Accepted

## Context

Multiple Lambda invocations may process messages for the same trading pair concurrently. Without coordination, race conditions will corrupt the order book.

## Decision

Use Redis `SET NX EX` for distributed per-pair locking. Messages are enqueued in Redis (`RPUSH book:{pair_id}:queue`) and processed under the lock. Exponential backoff with jitter is used on lock contention.

## Consequences

**Benefits:**
- Simple, well-understood mechanism
- Per-pair granularity (pairs do not block each other)
- Redis already in stack for cache

**Risks:**
- Single-node Redis: lock lost on node failure → potential duplicate processing
- Mitigation: implement RedLock post-migration (multi-node Redis)

## Follow-ups

- [ ] Implement RedLock via `@es-node/redis-manager` after serverless migration
- [ ] Define max retry attempts and backoff ceiling
- [ ] Add lock acquisition metrics (contention rate)
