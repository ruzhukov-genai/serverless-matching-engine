# Architecture Decision Records

Lightweight ADRs for serverless-matching-engine.
Format: `NNN-title.md`. Status: proposed → accepted → superseded/deprecated.

| # | Title | Status | Date |
|---|-------|--------|------|
| 001 | Stateless workers, Valkey as primary cache | Accepted | 2026-03-15 |
| 002 | Gateway shared cache broadcasts | Accepted | 2026-03-15 |
| 003 | Async DB persistence off hot path | Accepted | 2026-03-15 |
| 004 | Atomic Lua matching in Valkey | Accepted | 2026-03-15 |
| 005 | Per-pair queue consumers with bounded concurrency | Accepted | 2026-03-15 |
| 006 | API Gateway WebSocket API for real-time feeds | Accepted | 2026-03-23 |
| 007 | Queue-based Lambda dispatch (replace direct invoke) | **Superseded** | 2026-03-25 |
| 008 | SQS-based dispatch modes | **Superseded** | 2026-03-26 |

**Production architecture (as of 2026-03-28):** Gateway Lambda handles orders inline (`ORDER_DISPATCH_MODE=inline`). No Worker Lambda, no SQS, no queue drain. ADRs 007 and 008 are historical — kept for context.
