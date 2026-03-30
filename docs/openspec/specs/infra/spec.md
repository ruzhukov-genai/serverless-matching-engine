# Infrastructure & Deployment Specification

## Purpose
Deployment constraints, Lambda behavior, and infrastructure invariants that
the system must uphold. These are operational requirements, not feature specs.

---

## Requirements

### Requirement: Stateless Workers
The system SHALL ensure all worker/gateway Lambdas are stateless.

No in-memory state may be assumed to persist between invocations.
Only Valkey and PostgreSQL are durable state.

#### Scenario: Cold start produces correct results
- GIVEN a Lambda container that has never processed an order
- WHEN the first order arrives
- THEN it is matched and persisted correctly with no dependency on prior in-memory state

---

### Requirement: Inline Dispatch is the Production Default
The system SHALL use ORDER_DISPATCH_MODE=inline in production.

In inline mode, matching runs inside the gateway Lambda — no cross-Lambda invoke.

#### Scenario: Inline mode env var set
- GIVEN the gateway Lambda is deployed
- WHEN ORDER_DISPATCH_MODE is checked
- THEN the value is "inline"

---

### Requirement: Database Connection via RDS Proxy
The system SHALL connect to PostgreSQL via RDS Proxy, not directly to the instance.

DATABASE_URL MUST point to the proxy endpoint, not the RDS instance.

#### Scenario: DATABASE_URL uses proxy endpoint
- GIVEN the deployed Lambda environment
- WHEN DATABASE_URL is inspected
- THEN it contains the proxy hostname, not the RDS instance hostname

---

### Requirement: Valkey Cache Seeded on Cold Start
The system SHALL seed the pairs cache (cache:pairs) during Lambda cold start.

#### Scenario: pairs cache populated on init
- GIVEN a freshly initialized Lambda container
- WHEN GET /api/pairs is called
- THEN a non-empty pairs list is returned (not corrupt or empty)

---

### Requirement: Reserved Concurrency Safe Limit
The system SHALL set Lambda reserved concurrency to a value that prevents
mass cold-start storms (≤100 concurrent containers).

#### Scenario: Concurrency limit respected under load
- GIVEN 100 reserved concurrency slots
- WHEN a benchmark fires 50 concurrent clients
- THEN no 5MB memory containers or LWA init timeout failures occur

---

### Requirement: Migrations Run After Deploy
The system SHALL run database migrations after every SAM deploy.

#### Scenario: deploy.sh triggers migrations
- GIVEN a successful sam deploy
- WHEN deploy.sh completes
- THEN POST /internal/manage run_migrations is called and succeeds

---

### Requirement: Per-Pair Matching Isolation in Valkey
The system SHALL store order books in per-pair Valkey keys.

Key patterns:
- `book:{pair_id}:bids` — sorted set of bids
- `book:{pair_id}:asks` — sorted set of asks
- `version:{pair_id}` — monotonically increasing version counter
- `queue:orders:{pair_id}` — pending order queue (queue mode only)

#### Scenario: Book keys are pair-scoped
- GIVEN BTC-USDT and ETH-USDT orders in the system
- WHEN book:BTC-USDT:bids is inspected
- THEN only BTC-USDT orders appear
