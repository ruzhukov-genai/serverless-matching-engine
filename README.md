# Serverless Matching / Order Management

> **Status:** 🏗️ Prototype — agentic spec-driven development

A fully-serverless exchange matching engine and order management system designed for near-infinite scalability (many trading pairs) and significant cost reduction over stateful deployments.

## Architecture at a Glance

```
RabbitMQ / SQS Queue ──► Lambda (Matching Engine) ──► DB
                                  │
                          Redis (Order Book Cache + Locking)
                                  │
                     Lambda (Order Service)
                     Lambda (Transaction Service)
```

## Monorepo Structure

```
.
├── docs/
│   ├── architecture.md          # System design & data flows
│   ├── decisions/               # Architecture Decision Records (ADRs)
│   ├── specs/                   # Per-service detailed specs
│   ├── brainstorm/              # Open questions & explorations
│   └── planning/                # Roadmap & phased delivery plan
├── services/
│   ├── matching-engine/         # Core order matching Lambda
│   ├── order-service/           # Order lifecycle + StatDispatcher
│   └── transaction-service/     # Transaction processing Lambda
├── shared/
│   └── redis/                   # Shared Redis utilities (locking, cache)
├── infra/                       # IaC (CDK / SAM / Terraform — TBD)
└── scripts/                     # Dev & operational scripts
```

## Key Design Decisions

- **Stateless Matching Engine** — Order Book loaded fresh per invocation (configurable via `STATELESS_MODE`)
- **Redis Order Book Locking** — Prevents race conditions across concurrent Lambda invocations
- **RedLock** — Planned post-migration for multi-node Redis resilience
- **Single Queue per Service** — Simplified fan-in, easier scaling

## Getting Started

> Implementation not yet started. See [`docs/planning/roadmap.md`](docs/planning/roadmap.md) for phased plan.

## Links

- [Architecture](docs/architecture.md)
- [Specs](docs/specs/)
- [ADRs](docs/decisions/)
- [Brainstorm](docs/brainstorm/)
- [Roadmap](docs/planning/roadmap.md)