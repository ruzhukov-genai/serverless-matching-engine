# ADR-010: Native Lambda Runtime for Gateway (Remove LWA)

**Status:** Accepted
**Date:** 2026-03-30

## Context

Gateway Lambda was deployed using Lambda Web Adapter (LWA), which translates API Gateway events
into HTTP requests forwarded to the axum server running on port 3001 inside the container.
This zero-code-change approach was convenient initially.

CloudWatch REPORT logs show Lambda execution takes 80–110ms. However, clients observe ~800ms
response times. Investigation revealed LWA's internal HTTP proxy adds ~700ms overhead per request
(buffered mode).

## Decision

Migrate to native `lambda_http` crate, which integrates axum directly with the Lambda runtime
via tower Service trait — no proxy layer, no TCP listener in Lambda.

The binary is split into two entrypoints:
- `bootstrap` (Lambda): `lambda_http::run(router)` — no TCP listener, no LWA
- `sme-gateway` (local dev): TCP listener + hyper serve loop (unchanged)

All shared types (`AppState`, `CacheBroadcasts`, `DispatchMode`, `CacheEntry`) and
`build_router()` are extracted into `crates/gateway/src/lib.rs` so both entrypoints share
the same logic without duplication.

## Consequences

- ~700ms LWA overhead eliminated
- No LWA container layer (~10MB saved)
- No readiness check needed (no HTTP server to warm up)
- local dev unchanged (`sme-gateway` binary still uses TCP listener)
- API Gateway integration unchanged (HTTP API v2 proxy, PayloadFormatVersion 2.0)
- Cold start slightly faster (no LWA extension init)
- Note: `tokio::spawn` background tasks now work correctly in Lambda — LWA buffered mode
  previously froze the runtime after HTTP response, preventing background work. Native
  runtime does not have this restriction.
