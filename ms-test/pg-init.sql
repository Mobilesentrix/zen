CREATE TABLE customers (
  id          INT PRIMARY KEY,
  email       TEXT NOT NULL,
  site        VARCHAR(8) NOT NULL,
  tier        SMALLINT,
  credit      NUMERIC(12,4),
  score       DOUBLE PRECISION,
  vip         BOOLEAN NOT NULL,
  big         BIGINT,
  joined_on   DATE,
  updated_at  TIMESTAMP,
  seen_at     TIMESTAMPTZ,
  profile     JSONB,
  tags        TEXT[],
  ext_id      UUID,
  raw         BYTEA
);

INSERT INTO customers VALUES
 (1, 'a@example.com', 'us', 1, 100.5000, 0.75, TRUE, 9007199254740993, '2024-01-02', '2024-01-02 03:04:05', '2024-01-02 03:04:05+00', '{"plan":"gold"}', ARRAY['x','y'], 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '\x00ff'),
 (2, 'b@example.com', 'us', 2, 0, NULL, FALSE, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
 (3, 'c@example.com', 'eu', NULL, 5.2500, 1.5, TRUE, -1, '2023-12-31', NULL, NULL, '[]', ARRAY[]::TEXT[], NULL, NULL);
