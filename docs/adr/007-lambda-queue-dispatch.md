# ADR-007: Queue-Based Lambda Dispatch (Replace Direct Invoke)

**Status:** Superseded — inline mode is production as of 2026-03-28.
**Date:** 2026-03-25
**Decision makers:** Roman Zhukov

> **Superseded by inline matching (2026-03-28).** Gateway Lambda processes orders inline (Lua EVAL → PG persist in the same invocation). No Worker Lambda, no SQS, no queue drain. `queue` mode retained for local dev only. See ADR-008 for history of SQS experiments that led to this conclusion.

## Context

(Historical — kept for reference)

Direct Lambda invoke had: cold start tax, no batching, VPC endpoint fragility, LWA 55ms overhead.
Queue-based dispatch (this ADR) addressed those but introduced up-to-1-minute processing latency.
Both approaches were superseded when benchmarks showed inline matching at ~200 orders/s sustained
with p50=5ms E2E was simpler and faster than any queue/invoke scheme.

## Decision (original, 2026-03-25)

Switch Lambda worker to queue-based dispatch using Valkey queues + EventBridge scheduled drain.

**Reverted 2026-03-28:** Worker Lambda deleted. Gateway runs matching inline. `ORDER_DISPATCH_MODE=inline` in production.

## Consequences

- No Worker Lambda deployed
- No EventBridge rule
- Gateway handles full order lifecycle: validate → Lua EVAL match → PG persist → respond 201
- `queue` mode still works for local dev (sme-api BRPOP worker)
