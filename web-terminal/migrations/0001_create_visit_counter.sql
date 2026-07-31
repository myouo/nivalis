CREATE TABLE IF NOT EXISTS terminal_counters (
  name TEXT PRIMARY KEY,
  value INTEGER NOT NULL DEFAULT 0 CHECK (value >= 0)
);

INSERT INTO terminal_counters (name, value)
VALUES ('terminal', 0)
ON CONFLICT (name) DO NOTHING;
