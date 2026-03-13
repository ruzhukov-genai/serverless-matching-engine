# Matching Engine

> **Status:** 📋 Spec phase — implementation not started

Lambda-based serverless order matching service.

## Spec

→ [`docs/specs/matching-engine.md`](../../docs/specs/matching-engine.md)

## Entry Points

| Handler | Trigger | Description |
|---------|---------|-------------|
| `handler.matchOrder` | SQS / Queue | Process incoming order against order book |

## Structure (planned)

```
matching-engine/
├── src/
│   ├── handler.ts          # Lambda entry point
│   ├── engine/
│   │   ├── matcher.ts      # Core matching logic
│   │   └── orderBook.ts    # Order book load/save
│   ├── redis/
│   │   └── lock.ts         # Order Book Locking
│   └── db/
│       └── orders.ts       # DB read/write
├── tests/
├── package.json
└── tsconfig.json
```

## Development

```bash
# TODO: local dev setup
```
