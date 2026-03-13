# Infrastructure

> Deferred until PoC is proven (Phase 6).

When the stateless matching pattern is validated, this directory will contain IaC for AWS deployment:
- Lambda functions (Matching Engine, Order Service, Transaction Service)
- SQS queues (replacing Dragonfly Streams)
- ElastiCache / Dragonfly Cloud
- Aurora PostgreSQL Serverless v2
- EventBridge (StatDispatcher schedule)

For now, the entire stack runs locally via `docker-compose.yml`.
