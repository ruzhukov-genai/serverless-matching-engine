-- migration: 20260325000000_lifecycle_timestamps
-- depends-on: 20260314020000_performance_indexes
-- Order lifecycle timestamps for latency instrumentation

ALTER TABLE orders ADD COLUMN IF NOT EXISTS received_at TIMESTAMPTZ;
ALTER TABLE orders ADD COLUMN IF NOT EXISTS matched_at TIMESTAMPTZ;
ALTER TABLE orders ADD COLUMN IF NOT EXISTS persisted_at TIMESTAMPTZ;