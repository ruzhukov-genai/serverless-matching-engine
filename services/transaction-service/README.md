# Transaction Service

> **Status:** 📋 Spec phase — implementation not started

Lambda-based trade transaction processor.

## Spec

→ [`docs/specs/transaction-service.md`](../../docs/specs/transaction-service.md)

## Entry Points

| Handler | Trigger | Description |
|---------|---------|-------------|
| `handler.processTransaction` | SQS / Queue | Persist trade, update balances |

## Structure (planned)

```
transaction-service/
├── src/
│   ├── handler.ts              # Lambda entry point
│   ├── transactions/
│   │   └── processor.ts        # Transaction logic
│   └── redis/
│       └── lock.ts             # Order Book Locking (shared)
├── tests/
├── package.json
└── tsconfig.json
```
