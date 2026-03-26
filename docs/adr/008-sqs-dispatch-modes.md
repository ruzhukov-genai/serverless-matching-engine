# ADR-008: SQS-Based Dispatch Modes

**Status:** Accepted  
**Date:** 2026-03-26  
**Decision makers:** Roman Zhukov

## Context

ADR-007 introduced queue-based dispatch using Valkey LPUSH + EventBridge drain (`queue` mode). This works but has two limitations:

1. **Latency** — EventBridge schedule is `rate(1 minute)`. Orders sit in the Valkey queue for up to 1 minute before the Worker Lambda drains them.
2. **Gateway Lambda overhead** — even in queue mode, every API request goes through the Gateway Lambda. For `/api/orders` specifically, the gateway only validates and pushes to Valkey — it doesn't do reads or serve a response body. This is wasted Lambda time.

SQS FIFO provides native Lambda event source mapping (ESM), meaning Worker Lambda is invoked automatically when messages arrive — no polling delay, no EventBridge schedule. Additionally, API Gateway has a native SQS integration that can route directly to SQS without going through any Lambda.

## Decision

**Add two SQS-based dispatch modes, controlled by the `ORDER_DISPATCH_MODE` environment variable:**

### Mode: `sqs`
Gateway Lambda validates the order, then sends it to an SQS FIFO queue instead of pushing to Valkey. Worker Lambda is triggered via SQS event source mapping (batch size 1–10).

```
Client → API GW → Gateway Lambda (validate) → SQS FIFO → Worker Lambda (ESM)
```

### Mode: `sqs-direct`
API Gateway routes `POST /api/orders` directly to SQS FIFO via a native AWS integration. Gateway Lambda is bypassed entirely for the order submission path. All other routes (`GET /api/pairs`, `/api/orderbook`, etc.) still go through Gateway Lambda via the `$default` route.

```
Client → API GW ──POST /api/orders──► SQS Standard → Worker Lambda (ESM, MaxConcurrency=50)
                └──all other routes─► Gateway Lambda
```

**`sqs-direct` is the currently deployed mode.**

### Summary of all three modes

| Mode | `ORDER_DISPATCH_MODE` | Gateway Lambda in order path? | Latency |
|------|----------------------|-------------------------------|---------|
| Queue | `queue` | Yes | Up to 1 min (EventBridge poll) |
| SQS | `sqs` | Yes (validate only) | Near-real-time (ESM) |
| SQS-direct | `sqs-direct` | No | Near-real-time (ESM) |

## Implementation Details

### SQS Queue
- **Name:** `serverless-matching-engine-orders` (Standard queue — switched from FIFO on 2026-03-26)
- **CF-managed:** Yes, conditional on `sqs-direct` mode (parameter `OrderDispatchMode`)
- **VisibilityTimeout:** Must be ≥ Worker Lambda timeout (180s)
- **Why Standard over FIFO:** FIFO serializes processing per MessageGroupId (~20 ord/s drain with single group). Atomicity is already guaranteed by Valkey Lua EVAL, not message ordering. Standard SQS enables parallel Lambda consumption → ~170-230 ord/s drain with MaxConcurrency=50.

### Worker Lambda (SQS event source mapping)
- Batch size: 10, batching window: 1s
- MaximumConcurrency: 50 (parallel Lambda invocations)
- Worker processes each record in the batch sequentially
- On partial batch failure, only failed records return to the queue

### Worker validation in `sqs-direct` mode
Since Gateway Lambda is bypassed, the Worker must validate incoming raw client orders:
- Generate UUID if `id` is missing
- Accept `time_in_force` as alias for `tif`
- Validate `quantity > 0`
- Validate `limit` order has `price`
- Validate `market` order has no `price`

This is the **accept-then-reject** model: API Gateway always returns 200 OK (SQS enqueue success), and validation happens asynchronously in the Worker. Invalid orders are rejected in the Worker and logged.

### Trade-offs of `sqs-direct`

**Pros:**
- Zero Gateway Lambda invocations for order submission (reduces cost and latency)
- Native AWS integration — no Rust code in the order submission path
- SQS provides durability + dead-letter queue support

**Cons:**
- Client receives a raw SQS XML response body on order submission (not a clean JSON 202)
- No per-order synchronous validation feedback (client can't know if order was malformed until it's rejected in Worker)
- Requires SQS VPC endpoint in the network stack (for Lambda ESM in VPC)

### Timestamp handling
- Worker uses SQS `attributes.SentTimestamp` as `received_at` — captures when the message entered SQS, not when the worker picks it up
- `persisted_at` is set by Postgres `NOW()` in the INSERT statement
- This gives true E2E measurement including queue wait time

### Infrastructure
- SQS Standard queue: CF-managed in `backend.yaml`, conditional on `OrderDispatchMode=sqs-direct`
- SQS VPC endpoint: added to `network.yaml` (required for Lambda ESM in VPC)
- Worker Lambda timeout: 120s (increased from 30s to accommodate batch SQS processing)

## Consequences

- `ORDER_DISPATCH_MODE` must be set in Gateway Lambda env vars (and Worker, for validation logic)
- Worker Lambda timeout raised to 120s to safely process SQS batches within VisibilityTimeout
- SQS VisibilityTimeout (120s) must always be ≥ Worker Lambda timeout — if Lambda timeout is increased again, update SQS VisibilityTimeout too
- `queue` mode (ADR-007) remains the default for local development and as fallback

## Alternatives Considered

- **Valkey keyspace notifications:** Could trigger Worker on LPUSH without 1-min wait, but adds Valkey config complexity and is not a managed trigger
- **Keep EventBridge `rate(1 second)`:** Reduces queue latency to ~1s but increases invocation count 60× vs `rate(1 minute)` — cost trade-off
- **Direct Lambda invoke from API GW:** Native integration exists but reintroduces the cold-start-in-hot-path problem (ADR-007 context)

---

_SQS-direct is the current production mode as of 2026-03-26. Uses Standard SQS (not FIFO) with MaxConcurrency=50. `queue` mode remains in code for local dev and testing._
