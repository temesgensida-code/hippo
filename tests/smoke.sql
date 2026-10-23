\set ON_ERROR_STOP on
DROP TABLE IF EXISTS s CASCADE;
CREATE TABLE s (id int, v int, f float8, ts timestamptz, pad text);
-- v is correlated with physical order (with noise), f is random
INSERT INTO s SELECT i, i/10 + (random()*50)::int, random()*1000, now() - (i||' seconds')::interval, repeat('x', 50) FROM generate_series(1, 300000) i;
ANALYZE s;
CREATE INDEX s_v_hippo ON s USING hippo (v);
SELECT * FROM hippo_index_stats('s_v_hippo');
SELECT pg_size_pretty(pg_relation_size('s_v_hippo')) AS hippo_size, pg_size_pretty(pg_relation_size('s')) AS heap;
SET enable_seqscan = off; SET enable_indexscan = off; SET enable_bitmapscan = on;
EXPLAIN (COSTS OFF) SELECT count(*) FROM s WHERE v BETWEEN 1000 AND 1100;
SELECT count(*) FROM s WHERE v BETWEEN 1000 AND 1100;
SET enable_seqscan = on; SET enable_bitmapscan = off;
SELECT count(*) FROM s WHERE v BETWEEN 1000 AND 1100;
