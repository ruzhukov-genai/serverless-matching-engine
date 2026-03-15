# ADR-002: Gateway Shared Cache Broadcasts

**Status:** Accepted  
**Date:** 2026-03-15  
**Decision makers:** Roman Zhukov

## Context

The gateway serves REST + WebSocket to N browser clients. Each REST request and each WS connection was individually calling `pool.get() + conn.get(key)` on Dragonfly. At 100 clients (300 WS + REST polling), this produced ~1000 Dragonfly ops/sec, saturating the connection pool and degrading all endpoints from 2ms to 500-750ms.

## Decision

**Gateway uses a shared CacheBroadcasts system.** A single background poller per cache key fetches from Dragonfly at a fixed interval and broadcasts to all subscribers.

- **REST handlers** read the latest cached value from memory (zero Dragonfly ops)
- **WS handlers** subscribe to a broadcast channel (zero Dragonfly ops)
- **Write operations** (POST orders, DELETE cancel) still use Dragonfly directly

### Polling intervals:
| Key pattern | Interval | Rationale |
|-------------|----------|-----------|
| `cache:orderbook:{pair}` | 500ms | Real-time book display |
| `cache:trades:{pair}` | 500ms | Trade feed |
| `cache:ticker:{pair}` | 1000ms | Summary stats |
| `cache:metrics`, `cache:throughput`, etc. | 2000ms | Dashboard, not latency-sensitive |
| `cache:pairs` | 5000ms | Rarely changes |

### What's NOT cached in gateway:
- `cache:portfolio:{user_id}` — per-user, unbounded keyspace
- `cache:orders:{user_id}` — per-user, unbounded keyspace
- Order/cancel queue writes — must hit Dragonfly immediately

## Consequences

- Dragonfly ops: ~1000/sec → ~15/sec at any client count
- REST responses are at most {interval}ms stale (acceptable for cached market data)
- Gateway memory usage increases by ~1MB per pair (cached JSON strings)
- Adding a new pair requires adding pollers (or making it dynamic)
- This is a gateway-only pattern — workers remain stateless (ADR-001)

## Alternatives Considered

- **Dragonfly Pub/Sub:** Workers publish updates, gateway subscribes. More real-time but adds complexity and couples gateway to worker event format. Good future optimization.
- **HTTP caching headers + reverse proxy:** Would help but doesn't solve WS scaling.
- **Client-side polling reduction:** Helps but doesn't fix the server-side architecture.
