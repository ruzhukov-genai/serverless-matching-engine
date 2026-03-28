-- OPT-3: Convert orders + trades to UNLOGGED tables
-- UNLOGGED skips WAL writes → ~2-3x faster INSERT/UPDATE on these hot tables.
-- Trade-off: data lost on crash. Acceptable for dev/bench; revert for production.
ALTER TABLE orders SET UNLOGGED;
ALTER TABLE trades SET UNLOGGED;
