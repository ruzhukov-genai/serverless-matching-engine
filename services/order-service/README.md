# Order Service

> **Status:** 📋 Spec phase — implementation not started

Lambda-based order lifecycle management service.

## Spec

→ [`docs/specs/order-service.md`](../../docs/specs/order-service.md)

## Entry Points

| Handler | Trigger | Description |
|---------|---------|-------------|
| `handler.processOrder` | SQS / Queue | Order create/update/cancel |
| `handler.statDispatcher` | EventBridge (1 min) | Dispatch stat update messages |
| `handler.statsUpdater` | SQS `update_stats` | Compute and persist stats per pair |

## Structure (planned)

```
order-service/
├── src/
│   ├── handler.ts              # Lambda entry points
│   ├── orders/
│   │   └── lifecycle.ts        # Order state machine
│   ├── stats/
│   │   ├── dispatcher.ts       # StatDispatcher logic
│   │   └── updater.ts          # StatsUpdater logic
│   └── redis/
│       └── lock.ts             # Order Book Locking (shared)
├── tests/
├── package.json
└── tsconfig.json
```
