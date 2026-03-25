# ADR-007: Queue-Based Lambda Dispatch (Replace Direct Invoke)

**Status:** Accepted  
**Date:** 2026-03-25  
**Decision makers:** Roman Zhukov

## Context

The gateway Lambda currently invokes the worker Lambda directly (synchronous SDK call) for each order. This has several problems:

1. **Cold start tax per order** — each invoke can hit a cold start (1.9s observed), blocking the gateway response
2. **No batching** — 1 invocation = 1 order. The batch persist code (`process_persist_batch`) can't batch across invocations
3. **VPC endpoint fragility** — the gateway needs egress to the Lambda service endpoint (port 443). VPC endpoint SG misconfiguration caused a full outage (2026-03-25)
4. **Cost** — N orders = N Lambda invocations, each billed for full duration including cold start
5. **55ms LWA overhead** — the Lambda invoke SDK roundtrip adds ~55ms to the gateway's hot path (ADR noted in MEMORY.md 2026-03-25)

The local `sme-api` worker already uses Valkey queues with BRPOP + batch drain (ADR-005). The Lambda worker should follow the same pattern.

## Decision

**Switch Lambda worker to queue-based dispatch using Valkey queues + scheduled invocation.**

### Architecture:
1. Gateway does `LPUSH queue:orders:{pair_id}` (same as local mode) — returns 202 immediately
2. Worker Lambda is invoked on a **1-second EventBridge schedule** (rate(1 minute) initially, can tune down)
3. Each invocation: BRPOP from all pair queues, drain up to 50 orders, batch process + batch persist
4. If queue is empty, return immediately (minimal cost — ~100ms billed)
5. Gateway's `ORDER_DISPATCH_MODE` set to `queue` (same as local dev)

### Benefits:
- **Batch processing** — 1 invocation handles N orders in one PG transaction
- **No cold start in hot path** — gateway returns instantly after LPUSH
- **Simpler networking** — gateway only needs Valkey access, not Lambda service endpoint
- **Lower cost** — fewer invocations, batch amortizes overhead
- **Consistent architecture** — same queue pattern as local `sme-api` worker

### Trade-offs:
- **Added latency** — orders wait up to polling interval (1s) before processing starts
- **Polling cost** — Lambda invoked every 1s even when idle (~2.6M invocations/month at 1s rate)
- **Queue visibility** — harder to trace individual order → invocation mapping (mitigated by client_order_id tagging)

### Tuning:
- Start with EventBridge `rate(1 minute)` + SQS dead-letter for the queue
- Can reduce to `rate(1 second)` or use Valkey keyspace notifications for lower latency
- Batch size: up to 50 per invocation (matches existing `process_persist_batch` limit)

## Consequences

- Gateway no longer needs Lambda VPC endpoint access (removes SG fragility)
- Worker Lambda needs VPC endpoint for Valkey only (already has it)
- EventBridge rule replaces direct invocation — add to CloudFormation
- `ORDER_DISPATCH_MODE=queue` in gateway Lambda env vars
- Remove Lambda invoke permissions from gateway role (simplifies IAM)

## Alternatives Considered

- **Keep direct invoke, fix SG:** Works but doesn't solve cold start, batching, or cost issues
- **SQS trigger:** SQS → Lambda is native AWS but adds another service + double serialization. Valkey queue is already there and faster
- **Valkey Streams + consumer groups:** More sophisticated but overkill for current scale. Good future option.
- **Step Functions:** Too complex for simple queue drain

---

_This ADR supersedes the direct invoke pattern. The gateway's `lambda` dispatch mode is deprecated._
