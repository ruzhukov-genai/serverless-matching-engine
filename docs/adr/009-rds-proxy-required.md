# ADR-009: RDS Proxy is Required — Do Not Bypass

**Status:** Accepted  
**Date:** 2026-03-28  

## Context

On 2026-03-28, during performance optimization work, we attempted to bypass RDS Proxy and connect Lambda directly to the RDS PostgreSQL instance to reduce per-query latency (proxy adds ~2-3ms overhead per connection).

**What happened:**
- Direct RDS connection: per-query persist_ms dropped from ~8ms → ~3ms ✅
- Cold-start TCP setup for direct PG: 500ms+ per new Lambda container ❌
- Container warmup time: 2s (proxy) → 102s (direct) ❌
- Result: cascading 503s, throughput collapsed, reverted within 20 minutes

## Decision

**RDS Proxy is mandatory. Direct RDS connections from Lambda are prohibited.**

This constraint must not be changed without a new ADR superseding this one.

## Rationale

Lambda's ephemeral container model creates and destroys PG connections constantly. RDS Proxy solves this at the infrastructure level:

1. **Cold start TCP setup**: Direct PG connection from a new Lambda container requires a full TCP + TLS + auth handshake (~500ms). RDS Proxy maintains a persistent connection pool — new containers connect to the proxy in <10ms.

2. **Connection limits**: At 100 reserved Lambda slots, direct connections would exhaust RDS `max_connections` during burst. RDS Proxy multiplexes Lambda connections onto a smaller pool of backend DB connections.

3. **Stability**: Without proxy, warmup of 100 containers took 102s and produced cascading 503s. With proxy: 100 containers warm in ~2s.

4. **The per-query overhead is acceptable**: The ~2-3ms proxy overhead is negligible compared to the 500ms cold-start cost it eliminates. The real bottleneck is connection acquisition time (~100-110ms at 100 concurrent slots), which is a pool-size problem — not a proxy problem.

## Consequences

- Lambda functions (`serverless-matching-engine-gateway`) MUST use `DATABASE_URL` pointing to the RDS Proxy endpoint (`*.proxy-*.rds.amazonaws.com`)
- Never change `DATABASE_URL` to point directly at the RDS instance endpoint
- The RDS Proxy endpoint is: `serverless-matching-engine-proxy.proxy-cp3apgpybbhw.us-east-1.rds.amazonaws.com`
- Lambda Security Group egress must allow port 5432 to the RDS Proxy SG (not direct to RDS SG)
- If connection acquisition latency is the bottleneck, the fix is batching multiple orders per invocation — not removing the proxy

## Alternatives Rejected

- **Direct RDS**: Faster per-query, catastrophic cold-start behavior. Rejected permanently.
- **PgBouncer in Lambda layer**: Complex, adds cold-start overhead of its own. Not explored.
- **Aurora Serverless Data API**: HTTP-based, no connection management needed, but ~40ms per call. Not appropriate for our latency targets.
