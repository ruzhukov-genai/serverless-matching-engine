-- migration: 20260314020000_performance_indexes
-- depends-on: 20260314010000_idempotency
-- Performance indexes: user status and creation time ordering

CREATE INDEX IF NOT EXISTS idx_orders_user_status_created ON orders(user_id, status, created_at DESC);