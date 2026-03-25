# Scripts

Dev and operational scripts.

| Script | Purpose |
|--------|---------|
| `bench-mode.sh` | Run benchmark mode (parallel process management) |

## Planned

| Script | Purpose |
|--------|---------|
| `dev-up.sh` | `docker compose up -d` + wait for healthy |
| `dev-down.sh` | `docker compose down` |
| `seed-pairs.sh` | Seed PostgreSQL with test trading pairs |
| `seed-orderbook.sh` | Seed Valkey with test order book data |
| `load-test.sh` | Run order match load test and report throughput |
| `reset-db.sh` | Drop and recreate PostgreSQL schema |
