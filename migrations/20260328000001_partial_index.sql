-- OPT-6: Partial index for active orders only (status IN New, PartiallyFilled)
-- Used by the resting order UPDATE CTE in persist_order.
-- Much smaller index than the full idx_orders_pair_status.
CREATE INDEX IF NOT EXISTS idx_orders_active
  ON orders(id, pair_id)
  WHERE status IN ('New', 'PartiallyFilled');
