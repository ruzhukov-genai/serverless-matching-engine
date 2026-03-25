-- Order lifecycle timestamps for latency instrumentation

ALTER TABLE orders ADD COLUMN IF NOT EXISTS received_at TIMESTAMPTZ;
ALTER TABLE orders ADD COLUMN IF NOT EXISTS matched_at TIMESTAMPTZ;
ALTER TABLE orders ADD COLUMN IF NOT EXISTS persisted_at TIMESTAMPTZ;