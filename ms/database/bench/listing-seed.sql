-- Product listing search benchmark data. Synthetic, deterministic.
-- Own schema `bench_listing`; never touches the corpus schemas. Re-runnable (drops and recreates).
-- Volume: 600 devices, 50 000 products, ~200 000 compatibility rows (4 per product, duplicates
-- allowed like real feeds), ~135 000 inventory rows (3 warehouses, 10 % of products have none).
-- Load: docker exec -i ms-ruleengine-starrocks mysql -uroot -h127.0.0.1 -P9030 < listing-seed.sql
DROP DATABASE IF EXISTS bench_listing FORCE;
CREATE DATABASE bench_listing;
USE bench_listing;

CREATE TABLE devices (
  device_id INT NOT NULL,
  brand VARCHAR(32) NOT NULL,
  model_name VARCHAR(64) NOT NULL
) PRIMARY KEY (device_id) DISTRIBUTED BY HASH(device_id) BUCKETS 1
PROPERTIES ("replication_num" = "1");

CREATE TABLE products (
  product_id INT NOT NULL,
  sku VARCHAR(16) NOT NULL,
  name VARCHAR(128) NOT NULL,
  part_type VARCHAR(32) NOT NULL,
  colour VARCHAR(16) NULL,
  quality VARCHAR(16) NOT NULL,
  rank_score INT NOT NULL,
  status VARCHAR(16) NOT NULL
) PRIMARY KEY (product_id) DISTRIBUTED BY HASH(product_id) BUCKETS 4
PROPERTIES ("replication_num" = "1");

CREATE TABLE compatibility (
  device_id INT NOT NULL,
  product_id INT NOT NULL
) DUPLICATE KEY (device_id, product_id) DISTRIBUTED BY HASH(product_id) BUCKETS 4
PROPERTIES ("replication_num" = "1");

CREATE TABLE inventory (
  product_id INT NOT NULL,
  warehouse_id INT NOT NULL,
  qty INT NOT NULL
) PRIMARY KEY (product_id, warehouse_id) DISTRIBUTED BY HASH(product_id) BUCKETS 4
PROPERTIES ("replication_num" = "1");

-- Devices: Apple 1-150, Samsung 151-330, Google 331-390, Motorola 391-470, OnePlus 471-520, Xiaomi 521-600.
INSERT INTO devices
SELECT g,
  CASE WHEN g <= 150 THEN 'Apple' WHEN g <= 330 THEN 'Samsung' WHEN g <= 390 THEN 'Google'
       WHEN g <= 470 THEN 'Motorola' WHEN g <= 520 THEN 'OnePlus' ELSE 'Xiaomi' END,
  CASE WHEN g <= 150 THEN concat(
         CASE g % 5 WHEN 0 THEN 'iPhone ' WHEN 1 THEN 'iPhone ' WHEN 2 THEN 'iPhone ' WHEN 3 THEN 'iPad ' ELSE 'iPad Air ' END,
         cast(g DIV 5 - 14 AS STRING),
         CASE g % 4 WHEN 1 THEN ' Pro' WHEN 2 THEN ' Pro Max' WHEN 3 THEN ' Plus' ELSE '' END)
       WHEN g <= 330 THEN concat('Galaxy ', CASE g % 3 WHEN 0 THEN 'S' WHEN 1 THEN 'A' ELSE 'Z Fold ' END, cast(g - 150 AS STRING))
       WHEN g <= 390 THEN concat('Pixel ', cast(g - 330 AS STRING))
       WHEN g <= 470 THEN concat('Moto G', cast(g - 390 AS STRING))
       WHEN g <= 520 THEN concat('OnePlus ', cast(g - 470 AS STRING))
       ELSE concat('Redmi Note ', cast(g - 520 AS STRING)) END
FROM TABLE(generate_series(1, 600)) AS t(g);

-- 'iPhone 16' must exist exactly once (the example search). g=150: 150 % 5 = 0 -> iPhone, 150 DIV 5 - 14 = 16, 150 % 4 = 2 -> ' Pro Max'.
-- Pin device 150 to the plain model.
INSERT INTO devices VALUES (150, 'Apple', 'iPhone 16');

INSERT INTO products
SELECT g,
  concat('107', lpad(cast(g AS STRING), 9, '0')),
  concat('Part ', cast(g AS STRING)),
  element_at(['screen', 'battery', 'back glass', 'camera', 'charging port', 'housing', 'speaker', 'flex'],
             1 + abs(murmur_hash3_32(g, 1)) % 8),
  CASE WHEN abs(murmur_hash3_32(g, 2)) % 10 < 2 THEN NULL
       ELSE element_at(['black', 'white', 'blue', 'red', 'gold', 'silver', 'green', 'purple'],
                       1 + abs(murmur_hash3_32(g, 3)) % 8) END,
  element_at(['OEM', 'Premium', 'Aftermarket', 'Refurbished'], 1 + abs(murmur_hash3_32(g, 4)) % 4),
  abs(murmur_hash3_32(g, 5)) % 5000,
  CASE WHEN abs(murmur_hash3_32(g, 6)) % 20 = 0 THEN 'disabled' ELSE 'active' END
FROM TABLE(generate_series(1, 50000)) AS t(g);

INSERT INTO compatibility
SELECT 1 + abs(murmur_hash3_32(g, 7)) % 600, (g - 1) DIV 4 + 1
FROM TABLE(generate_series(1, 200000)) AS t(g);

INSERT INTO inventory
SELECT (g - 1) DIV 3 + 1, (g - 1) % 3 + 1, abs(murmur_hash3_32(g, 8)) % 14 - 7
FROM TABLE(generate_series(1, 150000)) AS t(g)
WHERE abs(murmur_hash3_32((g - 1) DIV 3 + 1, 9)) % 10 <> 0;

SELECT 'devices', count(*) FROM devices
UNION ALL SELECT 'products', count(*) FROM products
UNION ALL SELECT 'compatibility', count(*) FROM compatibility
UNION ALL SELECT 'inventory', count(*) FROM inventory
UNION ALL SELECT 'in stock', count(*) FROM (SELECT product_id FROM inventory GROUP BY product_id HAVING sum(qty) > 0) s;
