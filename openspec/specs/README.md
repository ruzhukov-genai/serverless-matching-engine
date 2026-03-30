# OpenSpec — Serverless Matching Engine

Spec-driven development for the SME project. These specs are the **source of truth**
for system behavior. Code, tests, and ADRs must align with them.

## Domain Structure

| Domain | Path | What it covers |
|--------|------|----------------|
| Orders | `specs/orders/` | Order submission, lifecycle, cancellation, TIF, STP |
| Matching | `specs/matching/` | Price-time priority, Lua atomicity, fills, balance locking |
| Market Data | `specs/market-data/` | Orderbook, trades, ticker, pairs, snapshot endpoint |
| Real-Time | `specs/realtime/` | WebSocket feeds, multiplexed WS, SSE stream |
| Portfolio | `specs/portfolio/` | User balances, available vs locked, WS updates |
| Admin | `specs/admin/` | /internal/manage commands: migrations, reset, SQL |
| Infra | `specs/infra/` | Deployment invariants, Lambda constraints, Valkey key patterns |

## Workflow

### Starting a new feature or change

```
/opsx:propose "brief description of change"
```

This creates `openspec/changes/<change-name>/` with:
- `proposal.md` — why + what
- `specs/` — delta specs (ADDED/MODIFIED/REMOVED requirements)
- `design.md` — technical approach
- `tasks.md` — implementation checklist

### Implementing

```
/opsx:apply
```

Works through `tasks.md` and implements the change.

### Completing a change

```
/opsx:archive
```

Merges delta specs into `openspec/specs/` and moves the change to `changes/archive/`.

## Spec Format

Requirements use RFC 2119 keywords:
- **SHALL / MUST** — absolute requirement
- **SHOULD** — recommended, exceptions exist
- **MAY** — optional

Scenarios use Given/When/Then format and MUST be testable.

## Relationship to ADRs

ADRs in `docs/adr/` record *architectural decisions* (why we chose X over Y).
OpenSpec specs record *behavioral requirements* (what the system must do).

They are complementary — specs reference ADR numbers where relevant.

## Relationship to Tests

Every scenario in a spec should map to either:
- A unit test in `crates/shared/src/engine.rs`
- An integration test in `crates/shared/src/integration_tests.rs`
- A smoke test in `tests/smoke/smoke_test.py`

If a scenario has no test, it's a gap.
