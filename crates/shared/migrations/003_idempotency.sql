-- Idempotency: client_order_id per user prevents duplicate order submission.
-- If a client retries with the same client_order_id, the API returns the existing order.
ALTER TABLE orders ADD COLUMN IF NOT EXISTS client_order_id VARCHAR(64);
CREATE UNIQUE INDEX IF NOT EXISTS idx_orders_idempotency ON orders(user_id, client_order_id) WHERE client_order_id IS NOT NULL;
