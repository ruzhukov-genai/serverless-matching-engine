# Roadmap

## Phase 0 — Prototype Setup *(current)*
- [x] Create monorepo structure
- [ ] Finalize ADRs (DB choice, queue choice)
- [ ] Define data schemas (order, transaction, trade event)
- [ ] Set up IaC skeleton (CDK or SAM)
- [ ] Set up local dev environment (LocalStack / Docker Compose)

## Phase 1 — Stateless Matching Engine
- [ ] Implement `StatelessMode` flag
- [ ] Order Book load from Redis (sorted sets)
- [ ] Lazy loading implementation
- [ ] Price-filtered queries
- [ ] Load test: baseline throughput (matches/sec)

## Phase 2 — Distributed Locking
- [ ] Implement Order Book Locking (per `common-functions.md` spec)
- [ ] Exponential backoff with jitter
- [ ] Lock metrics instrumentation

## Phase 3 — Service Refactoring
- [ ] Single queue for all pairs (Matching Engine)
- [ ] Single order queue (Order Service)
- [ ] StatDispatcher + StatsUpdater implementation

## Phase 4 — Lambda Deployment
- [ ] Lambda packaging (Matching Engine)
- [ ] Lambda packaging (Order Service + StatDispatcher + StatsUpdater)
- [ ] Lambda packaging (Transaction Service)
- [ ] SQS/RabbitMQ trigger integration
- [ ] End-to-end integration test

## Phase 5 — Hardening
- [ ] RedLock implementation (multi-node Redis)
- [ ] DLQ handling for all queues
- [ ] Observability: X-Ray tracing, CloudWatch dashboards
- [ ] Load test: production-scale throughput target

## Phase 6 — Production Readiness
- [ ] Security review (IAM least-privilege, VPC placement)
- [ ] Canary deployment strategy
- [ ] Runbooks for common failure scenarios
- [ ] Cost analysis vs. current stateful setup
