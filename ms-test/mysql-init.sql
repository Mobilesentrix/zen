CREATE TABLE products (
  entity_id   INT UNSIGNED PRIMARY KEY,
  sku         VARCHAR(64) NOT NULL,
  site        VARCHAR(8)  NOT NULL,
  name        VARCHAR(255),
  price       DECIMAL(12,4) NOT NULL,
  weight      DOUBLE,
  qty         INT NOT NULL,
  big         BIGINT,
  active      BOOLEAN NOT NULL,
  created_on  DATE,
  updated_at  DATETIME,
  meta        JSON,
  meta_text   TEXT,
  blob_col    BLOB
);

INSERT INTO products VALUES
 (1, 'SKU-A', 'us', 'Screen A', 12.5000, 0.25, 10, 9007199254740993, TRUE,  '2024-01-02', '2024-01-02 03:04:05', '{"color":"black","tags":["a","b"]}', '{"nested":{"x":1}}', X'00FF'),
 (2, 'SKU-B', 'us', 'Battery B', 20.0000, 0.5, 3, NULL, FALSE, '2024-02-03', '2024-02-03 10:00:00', NULL, '[1,2,3]', NULL),
 (3, 'SKU-C', 'eu', 'Cable C', 3.9900, NULL, 100, -5, TRUE, NULL, NULL, '{}', 'plain text', NULL),
 (4, 'O''Brien', 'us', 'Quote test', 1.0000, 1, 1, 0, TRUE, NULL, NULL, NULL, NULL, NULL);
