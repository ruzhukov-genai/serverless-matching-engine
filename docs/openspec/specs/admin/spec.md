# Admin / Internal Management Specification

## Purpose
Internal management commands for deployment, reset, migrations, and diagnostics.
All commands are delivered via POST /internal/manage. This endpoint is for
admin/deploy use only — not exposed to end users.

---

## Requirements

### Requirement: Run Migrations
The system SHALL apply all pending SQL migrations on demand.

#### Scenario: Successful migration
- GIVEN pending migrations exist
- WHEN POST /internal/manage {"command":"run_migrations"} is called
- THEN all pending migrations are applied and {"status":"ok"} is returned

#### Scenario: No-op when up to date
- GIVEN no pending migrations
- WHEN run_migrations is called
- THEN the system returns ok without error

---

### Requirement: Reset All (Full Reset)
The system SHALL support a full state reset: truncate orders and trades,
reset balances for N users, flush Valkey order book and queues.

#### Scenario: reset_all clears state
- GIVEN live orders and trades in the system
- WHEN POST /internal/manage {"command":"reset_all","users":100} is called
- THEN orders and trades are truncated
- AND balances are reset to 10 BTC + 1,000,000 USDT per user
- AND Valkey book, cache, and queues are flushed for all pairs
- AND {"ok":true,"users":100} is returned

#### Scenario: Default user count
- GIVEN reset_all called without "users" parameter
- WHEN the command executes
- THEN 100 users are seeded

---

### Requirement: Reset Balances Only
The system SHALL support resetting balances without clearing order history.

#### Scenario: reset_balances leaves orders intact
- GIVEN existing order records
- WHEN POST /internal/manage {"command":"reset_balances","users":50} is called
- THEN balances are reset to defaults
- AND order/trade records are not truncated

---

### Requirement: Truncate Orders
The system SHALL support truncating order and trade data without resetting balances.

#### Scenario: truncate_orders clears book and DB
- GIVEN live orders in the book and DB
- WHEN POST /internal/manage {"command":"truncate_orders"} is called
- THEN orders and trades tables are truncated
- AND Valkey book keys, cache keys, and order queues are deleted for all pairs
- AND a reset_version counter is incremented (to trigger container recycle)

---

### Requirement: Arbitrary SQL Execution
The system SHALL support executing arbitrary SQL for admin diagnostics.

#### Scenario: exec_sql runs DDL/DML
- GIVEN a valid SQL statement
- WHEN POST /internal/manage {"command":"exec_sql","sql":"..."} is called
- THEN the statement executes and rows_affected is returned

#### Scenario: query returns rows as JSON
- GIVEN a SELECT statement
- WHEN POST /internal/manage {"command":"query","sql":"SELECT count(*) FROM orders"} is called
- THEN {"rows":[{"count":N}]} is returned

---

### Requirement: Sweep Stale Orders
The system SHALL support sweeping ghost order references from the Valkey book.

Ghost orders: order IDs present in book sorted sets whose order hash has expired.

#### Scenario: sweep_stale_orders removes ghosts
- GIVEN stale order IDs in a book sorted set whose order hashes are gone
- WHEN POST /internal/manage {"command":"sweep_stale_orders"} is called
- THEN ghost entries are removed and removed count is returned

---

### Requirement: Command Validation
The system SHALL return 400 Bad Request for unknown management commands.

#### Scenario: Unknown command rejected
- GIVEN POST /internal/manage {"command":"destroy_everything"}
- WHEN the endpoint processes the request
- THEN {"error":"unknown command: destroy_everything"} is returned with 400

---

### Requirement: Management Command Error Handling
The system SHALL return 500 with an error message if a management command fails
(e.g., DATABASE_URL not set, PG connection failure).

#### Scenario: Missing DATABASE_URL
- GIVEN run_migrations called but DATABASE_URL env var is not set
- WHEN the command runs
- THEN {"error":"DATABASE_URL not set"} is returned with 500
