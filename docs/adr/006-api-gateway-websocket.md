# ADR-006: API Gateway WebSocket API for Real-Time Feeds

**Status:** Accepted  
**Date:** 2026-03-23  
**Decision makers:** Roman Zhukov

## Context

The Gateway Lambda serves REST via API Gateway HTTP API, but HTTP API doesn't support WebSocket upgrades. The trading UI requires live orderbook, trade, and ticker feeds via WebSocket. Locally, the Gateway runs as a long-lived process with native WS support. On AWS, we need an equivalent.

## Decision

**Add an API Gateway WebSocket API alongside the existing HTTP API.**

### Architecture

```
Browser ──WS──▶ API Gateway WebSocket API ──▶ Gateway Lambda ($connect/$disconnect/$default)
                        ▲                              │
                        │                              ▼
                   @connections API              Dragonfly (connection store)
                        ▲                              ▲
                        │                              │
                   Worker Lambda ─────────────────────▶│ (writes cache + publishes)
                        │
                        └── after match: push trade/orderbook updates to subscribers
```

### Connection Management
- `$connect` → store `connectionId` in Dragonfly SET `ws:connections`
- `$disconnect` → remove `connectionId` from Dragonfly
- `$default` → parse `{"subscribe": ["trades:BTC-USDT", ...]}` commands
  - Store subscriptions: `ws:subs:{channel}` → SET of connectionIds
  - Store reverse map: `ws:conn:{connectionId}` → SET of channels
  - Send current cached value immediately via `@connections` POST

### Push Mechanism
- Worker Lambda already writes `cache:trades:{pair}`, `cache:orderbook:{pair}` to Dragonfly
- After writing cache, Worker calls `push_ws_updates()`:
  1. For each updated cache key, read `ws:subs:{channel}` from Dragonfly
  2. POST to `https://{api-id}.execute-api.{region}.amazonaws.com/{stage}/@connections/{connectionId}`
  3. Wrap in same format: `{"ch": "trades:BTC-USDT", "data": {...}}`
- Stale connections (410 Gone) → remove from Dragonfly

### CloudFormation Resources
- `AWS::ApiGatewayV2::Api` (ProtocolType: WEBSOCKET)
- `AWS::ApiGatewayV2::Route` ($connect, $disconnect, $default)
- `AWS::ApiGatewayV2::Integration` (Lambda proxy for each route)
- `AWS::ApiGatewayV2::Stage` (auto-deploy)
- `AWS::ApiGatewayV2::Deployment`
- Lambda permission for API GW to invoke

### Gateway Lambda Changes
- Add route handler: detect API GW WebSocket event format
- Parse `requestContext.routeKey` ($connect / $disconnect / $default)
- Use `requestContext.connectionId` for connection management
- Use API GW Management API for pushing messages

### Worker Lambda Changes
- After `persist_and_update_cache()`, call `push_to_ws_subscribers()`
- Read subscriber list from Dragonfly
- POST updates to API GW @connections endpoint
- Handle 410 Gone (stale connections) gracefully

## Consequences

- Trading UI works on AWS with live feeds
- Adds ~10-50ms latency to trade push (Worker → API GW @connections)
- Worker Lambda needs IAM permission: `execute-api:ManageConnections`
- Connection state in Dragonfly (ephemeral, acceptable)
- No additional always-on compute (Lambda-only, pay-per-invocation)
- Gateway Lambda handles both HTTP (via LWA) and WS (via direct event parsing)

## Alternatives Considered

- **Lambda Function URL with streaming:** Doesn't support WebSocket protocol
- **ALB + Lambda:** Supports WS but adds $16/mo minimum + complexity
- **AppSync:** Managed WS but adds vendor lock-in and doesn't fit our event model
- **EC2 + Gateway process:** Defeats the serverless goal
- **Polling from frontend:** Works but poor UX (500ms+ stale data, high request volume)
