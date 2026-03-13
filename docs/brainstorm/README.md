# Brainstorm

Open questions, explorations, and half-formed ideas. Nothing here is decided.

## Open Questions

### Infrastructure
- **SQS vs RabbitMQ?**
  - SQS natively triggers Lambda, simpler ops
  - RabbitMQ gives more control (routing keys, exchanges, priority queues)
  - Current system uses RabbitMQ — migration cost?

- **Database backend?**
  - DynamoDB — serverless, scales with Lambda, but query flexibility limited
  - Aurora Serverless v2 — SQL, scales to zero (v2 slower cold start than v1)
  - RDS + RDS Proxy — Lambda-friendly connection pooling
  - ⚡ Decision needed before DB schema design

- **Redis: self-managed vs ElastiCache?**
  - ElastiCache Redis: managed, supports cluster mode (RedLock-ready)
  - Upstash: serverless Redis, pay-per-request, simpler for prototype

### Matching Engine
- **Cold start impact on P99 latency?**
  - Provisioned concurrency: eliminates cold starts, adds cost
  - SnapStart (Java only): not applicable if TypeScript/Node
  - Keep-warm pings: hack but works for low-traffic pairs

- **Order book size limits?**
  - Lazy loading breaks down when a single order fills across 100s of price levels
  - May need a "full load" fallback threshold

### Observability
- How do we trace a single order across 3 Lambdas?
  - X-Ray correlation IDs?
  - Custom `order_id` propagation through all queues

### Multi-tenancy
- One Lambda per exchange? One per pair? Shared?
- Cost vs isolation trade-off

## Ideas to Explore

- [ ] Event sourcing for order book reconstruction
- [ ] WebSocket push for order status updates (API Gateway WebSocket)
- [ ] Canary deployments per trading pair
- [ ] Chaos testing: lock expiry under load

## References

- [Original Confluence Spec](../specs/) (converted from source doc)
- RedLock: https://redis.io/docs/latest/develop/use/patterns/distributed-locks/
- [@es-node/redis-manager](https://www.npmjs.com/package/@es-node/redis-manager)
