# Roadmap

## Phase 0 — Local Dev Environment *(current)*
- [x] Create monorepo structure
- [x] Finalize cache/queue choice → Dragonfly (ADR-004)
- [x] Finalize DB choice → PostgreSQL (ADR-004)
- [ ] Docker Compose: Dragonfly + PostgreSQL + service containers
- [ ] Define data schemas (order, transaction, trade event, pair)
- [ ] PostgreSQL migrations setup (e.g. node-pg-migrate or Prisma)
- [ ] Dragonfly connection + health check utility
- [ ] Basic project scaffolding (TypeScript, shared packages)

## Phase 1 — Stateless Matching Engine
- [ ] Implement `StatelessMode` flag
- [ ] Order Book load from Dragonfly (sorted sets → DB fallback)
- [ ] Lazy loading implementation
- [ ] Price-filtered queries
- [ ] Load test: baseline throughput (matches/sec)

## Phase 2 — Distributed Locking
- [ ] Implement Order Book Locking (per `common-functions.md` spec)
- [ ] Exponential backoff with jitter
- [ ] Lock metrics instrumentation

## Phase 3 — Service Refactoring
- [ ] Dragonfly Streams for all inter-service communication
- [ ] Consumer groups per service
- [ ] StatDispatcher + StatsUpdater implementation

## Phase 4 — Lambda Deployment
- [ ] Lambda packaging (Matching Engine)
- [ ] Lambda packaging (Order Service + StatDispatcher + StatsUpdater)
- [ ] Lambda packaging (Transaction Service)
- [ ] SQS trigger integration (replacing Dragonfly Streams)
- [ ] End-to-end integration test

## Phase 5 — Hardening
- [ ] RedLock implementation (multi-node Dragonfly/Redis)
- [ ] DLQ handling for all streams
- [ ] Observability: X-Ray tracing, CloudWatch dashboards
- [ ] Load test: production-scale throughput target

## Phase 6 — Production Readiness
- [ ] Security review (IAM least-privilege, VPC placement)
- [ ] Migration path: Dragonfly → ElastiCache / Dragonfly Cloud
- [ ] Migration path: PostgreSQL → Aurora Serverless v2
- [ ] Canary deployment strategy
- [ ] Runbooks for common failure scenarios
- [ ] Cost analysis vs. current stateful setup
