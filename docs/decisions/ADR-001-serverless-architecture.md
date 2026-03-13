# ADR-001: Adopt Serverless (Lambda) Architecture

**Status:** Accepted

## Context

The current matching engine runs as a stateful, always-on service. This limits scalability to a fixed number of trading pairs and incurs continuous infrastructure cost regardless of load.

## Decision

Migrate the Matching Engine, Order Service, and Transaction Service to AWS Lambda, triggered by message queues. Use Redis for shared state (order book cache, distributed locking).

## Consequences

**Benefits:**
- Near-infinite horizontal scalability per trading pair
- Pay-per-invocation cost model
- Reduced operational overhead

**Risks:**
- Lambda cold start latency may affect matching throughput — requires load testing
- Stateless design requires careful cache invalidation and lock management
- Complexity shifts from service management to distributed coordination

## Follow-ups

- [ ] Load test to establish baseline throughput (matches/sec)
- [ ] Evaluate cold start mitigation (provisioned concurrency, snapstart)
- [ ] Decide on DB backend (see ADR-004, TBD)
