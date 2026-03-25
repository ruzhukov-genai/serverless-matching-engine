-- migration: 20260314010000_idempotency
-- depends-on: 20260314000000_initial_schema
-- Idempotency support: client_order_id column and unique index

ALTER TABLE orders ADD COLUMN IF NOT EXISTS client_order_id VARCHAR(64);
CREATE UNIQUE INDEX IF NOT EXISTS idx_orders_idempotency ON orders(user_id, client_order_id) WHERE client_order_id IS NOT NULL;