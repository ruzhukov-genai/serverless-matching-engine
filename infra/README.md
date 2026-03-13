# Infrastructure

IaC for the serverless matching engine stack.

**Tooling decision:** TBD (AWS CDK / SAM / Terraform)
→ See [`docs/brainstorm/README.md`](../docs/brainstorm/README.md)

## Planned Resources

| Resource | Purpose |
|----------|---------|
| Lambda (Matching Engine) | Order matching |
| Lambda (Order Service) | Order lifecycle |
| Lambda (StatDispatcher) | Scheduled stat triggers |
| Lambda (StatsUpdater) | Stat computation |
| Lambda (Transaction Service) | Trade persistence |
| SQS Queues | Service triggers + DLQs |
| ElastiCache Redis | Locking + order book cache |
| EventBridge Rule | 1-min StatDispatcher schedule |
| RDS / DynamoDB | Persistent order + transaction storage (TBD) |
| CloudWatch | Logs, metrics, alarms |
| X-Ray | Distributed tracing |

## Structure (planned)

```
infra/
├── stacks/
│   ├── matching-engine-stack.ts
│   ├── order-service-stack.ts
│   └── transaction-service-stack.ts
├── shared/
│   ├── redis.ts
│   └── queues.ts
└── bin/
    └── app.ts
```
