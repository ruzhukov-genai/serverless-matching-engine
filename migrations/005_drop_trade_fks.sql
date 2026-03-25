-- Drop foreign key constraints on trades table.
-- Referential integrity is guaranteed by the Lua matching engine (atomic Dragonfly operations).
-- FKs cause persist failures when concurrent Lambdas match against each other's orders
-- that haven't been committed to PG yet.
ALTER TABLE trades DROP CONSTRAINT IF EXISTS trades_buy_order_id_fkey;
ALTER TABLE trades DROP CONSTRAINT IF EXISTS trades_sell_order_id_fkey;
ALTER TABLE trades DROP CONSTRAINT IF EXISTS trades_pair_id_fkey;
