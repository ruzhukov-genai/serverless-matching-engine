# Scripts

Dev and operational scripts.

## Planned

| Script | Purpose |
|--------|---------|
| `dev-up.sh` | `docker compose up -d` + wait for healthy |
| `dev-down.sh` | `docker compose down` |
| `seed-pairs.sh` | Seed PostgreSQL with test trading pairs |
| `seed-orderbook.sh` | Seed Dragonfly with test order book data |
| `load-test.sh` | Run order match load test and report throughput |
| `reset-db.sh` | Drop and recreate PostgreSQL schema |
