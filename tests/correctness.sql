-- Randomised differential tests: every predicate is evaluated through a forced Hippo bitmap
-- scan and through a forced sequential scan; results must be identical.
\set ON_ERROR_STOP on
SET client_min_messages = warning;
\o /dev/null
CREATE EXTENSION IF NOT EXISTS hippo;
DROP TABLE IF EXISTS hippo_chk_stats;
CREATE TABLE hippo_chk_stats (scenario text, checks int, used_hippo int);

CREATE OR REPLACE FUNCTION hippo_check(tbl regclass, idx text, wh text, scenario text) RETURNS void AS $$
DECLARE a record; b record; plan text; used int := 0;
BEGIN
  PERFORM set_config('enable_seqscan','off',true);
  PERFORM set_config('enable_indexscan','off',true);
  PERFORM set_config('enable_indexonlyscan','off',true);
  PERFORM set_config('enable_bitmapscan','on',true);
  EXECUTE format('SELECT count(*) c, coalesce(sum(id),0) s FROM %s WHERE %s', tbl, wh) INTO a;
  FOR plan IN EXECUTE format('EXPLAIN SELECT count(*) FROM %s WHERE %s', tbl, wh) LOOP
    IF plan LIKE '%Bitmap Index Scan on ' || idx || '%' THEN used := 1; END IF;
  END LOOP;
  PERFORM set_config('enable_seqscan','on',true);
  PERFORM set_config('enable_indexscan','off',true);
  PERFORM set_config('enable_bitmapscan','off',true);
  EXECUTE format('SELECT count(*) c, coalesce(sum(id),0) s FROM %s WHERE %s', tbl, wh) INTO b;
  IF a <> b THEN
    RAISE EXCEPTION 'MISMATCH [%] on % WHERE %: hippo=% seq=%', scenario, tbl, wh, a, b;
  END IF;
  INSERT INTO hippo_chk_stats VALUES (scenario, 1, used);
END $$ LANGUAGE plpgsql;

-- run n random predicates on column col; values drawn from the table itself
CREATE OR REPLACE FUNCTION hippo_fuzz(tbl regclass, idx text, col text, n int, scenario text, lit text DEFAULT '%s') RETURNS void AS $$
DECLARE ops text[] := ARRAY['<','<=','=','>=','>']; v1 text; v2 text; i int; wh text;
BEGIN
  FOR i IN 1..n LOOP
    EXECUTE format('SELECT %I::text FROM %s WHERE %I IS NOT NULL ORDER BY random() LIMIT 1', col, tbl, col) INTO v1;
    EXECUTE format('SELECT %I::text FROM %s WHERE %I IS NOT NULL ORDER BY random() LIMIT 1', col, tbl, col) INTO v2;
    IF v1 IS NULL THEN CONTINUE; END IF;
    CASE i % 4
      WHEN 0 THEN wh := format('%I %s %L', col, ops[1 + (random()*4)::int], v1);
      WHEN 1 THEN wh := format('%I >= %L AND %I <= %L', col, least(v1,v2), col, greatest(v1,v2));
      WHEN 2 THEN wh := format('%I = %L', col, v1);
      ELSE wh := format('%I > %L AND %I < %L', col, v1, col, v2);
    END CASE;
    PERFORM hippo_check(tbl, idx, wh, scenario);
  END LOOP;
END $$ LANGUAGE plpgsql;

-- ---------------------------------------------------------------- 1. every type, 3 layouts
DROP TABLE IF EXISTS t_types;
CREATE TABLE t_types (id serial, i2 int2, i4 int4, i4r int4, i8 int8, f4 float4, f8 float8, d date, ts timestamp, tz timestamptz, pad text);
INSERT INTO t_types (i2, i4, i4r, i8, f4, f8, d, ts, tz, pad)
SELECT (i/20 % 30000)::int2, i/7 + (random()*30)::int, (random()*1000000)::int,
       (i::int8 * 100000) + (random()*1000)::int8, (i/13 + random()*5)::float4, (i/11 + random()*7)::float8,
       date '2000-01-01' + (i/50), timestamp '2020-01-01' + (i||' seconds')::interval,
       timestamptz '2020-01-01 00:00:00+00' + (i||' seconds')::interval, repeat('x', 40)
FROM generate_series(1, 200000) i;
-- NULLs, special floats
UPDATE t_types SET i4 = NULL, f8 = NULL WHERE id % 97 = 0;
INSERT INTO t_types (f4, f8, i4r, pad) VALUES ('NaN', 'NaN', 1, 'x'), ('Infinity','Infinity', 2, 'x'), ('-Infinity','-Infinity', 3, 'x'), (-0.0, -0.0, 4, 'x'), (0.0, 0.0, 5, 'x');
ANALYZE t_types;
CREATE INDEX t_i2 ON t_types USING hippo (i2);
CREATE INDEX t_i4 ON t_types USING hippo (i4);
CREATE INDEX t_i4r ON t_types USING hippo (i4r) WITH (buckets = 64, density = 30);
CREATE INDEX t_i8 ON t_types USING hippo (i8);
CREATE INDEX t_f4 ON t_types USING hippo (f4);
CREATE INDEX t_f8 ON t_types USING hippo (f8);
CREATE INDEX t_d ON t_types USING hippo (d);
CREATE INDEX t_ts ON t_types USING hippo (ts);
CREATE INDEX t_tz ON t_types USING hippo (tz);
SELECT hippo_fuzz('t_types','t_i2','i2',60,'int2');
SELECT hippo_fuzz('t_types','t_i4','i4',60,'int4+null');
SELECT hippo_fuzz('t_types','t_i4r','i4r',60,'int4 random');
SELECT hippo_fuzz('t_types','t_i8','i8',60,'int8');
SELECT hippo_fuzz('t_types','t_f4','f4',60,'float4');
SELECT hippo_fuzz('t_types','t_f8','f8',60,'float8+null');
SELECT hippo_fuzz('t_types','t_d','d',60,'date');
SELECT hippo_fuzz('t_types','t_ts','ts',60,'timestamp');
SELECT hippo_fuzz('t_types','t_tz','tz',60,'timestamptz');
-- explicit edge predicates
SELECT hippo_check('t_types','t_f8', w, 'float edge') FROM unnest(ARRAY[
  'f8 = ''NaN''', 'f8 > ''NaN''', 'f8 >= ''NaN''', 'f8 < ''NaN''', 'f8 = ''Infinity''', 'f8 = ''-Infinity''',
  'f8 = 0', 'f8 = -0.0', 'f8 >= 0 AND f8 <= 0', 'f8 > -1e308', 'f8 < 1e308']) w;
SELECT hippo_check('t_types','t_f4', w, 'float4 edge') FROM unnest(ARRAY[
  'f4 = ''NaN''', 'f4 = 0', 'f4 = -0.0', 'f4 > 1.5', 'f4 < -1e30', 'f4 = 1.5::float8']) w;
-- cross-type constants (int8 col vs int4/int2 const, int4 col vs int8 const, float4 vs float8)
SELECT hippo_check('t_types','t_i8', w, 'cross type') FROM unnest(ARRAY[
  'i8 > 5', 'i8 < 1000000::int4', 'i8 = 100::int2', 'i8 <= 9999999999']) w;
SELECT hippo_check('t_types','t_i4', w, 'cross type') FROM unnest(ARRAY[
  'i4 > 5::int8', 'i4 = 7000::int8', 'i4 < 99999999999::int8', 'i4 >= 100::int2']) w;
SELECT hippo_check('t_types','t_i2', w, 'cross type') FROM unnest(ARRAY[
  'i2 > 5::int8', 'i2 = 100::int4', 'i2 < 40000']) w;
-- IN / ANY lists, impossible predicates, IS NULL, boundaries, int overflow edges
SELECT hippo_check('t_types','t_i4', w, 'misc') FROM unnest(ARRAY[
  'i4 IN (1, 5, 777, 20000, 99999)', 'i4 = ANY(ARRAY[3,4,5])', 'i4 > 10 AND i4 < 5', 'i4 IS NULL',
  'i4 IS NOT NULL AND i4 < 100', 'i4 = -2147483648', 'i4 = 2147483647', 'i4 < 0', 'i4 > 2147483646']) w;

-- ---------------------------------------------------------------- 2. maintenance
DROP TABLE IF EXISTS t_maint;
CREATE TABLE t_maint (id serial, v int, pad text);
INSERT INTO t_maint (v, pad) SELECT i/5, repeat('y', 100) FROM generate_series(1, 50000) i;
CREATE INDEX t_maint_v ON t_maint USING hippo (v) WITH (density = 25);
SELECT hippo_fuzz('t_maint','t_maint_v','v',30,'maint: after build');
-- append beyond the end (grows ranges, adds entries and entry pages)
INSERT INTO t_maint (v, pad) SELECT i/5, repeat('y', 100) FROM generate_series(50001, 120000) i;
SELECT hippo_fuzz('t_maint','t_maint_v','v',40,'maint: append');
-- out-of-range values appended
INSERT INTO t_maint (v, pad) SELECT -i, 'z' FROM generate_series(1, 2000) i;
INSERT INTO t_maint (v, pad) SELECT 1000000 + i, 'z' FROM generate_series(1, 2000) i;
SELECT hippo_check('t_maint','t_maint_v', w, 'maint: extremes') FROM unnest(ARRAY['v < -1000','v > 1000000','v = -5','v = 1000005','v < 0','v >= 10000']) w;
-- updates that change the key (HOT where possible)
UPDATE t_maint SET v = v + 100000 WHERE id % 13 = 0;
UPDATE t_maint SET v = -v WHERE id % 29 = 0;
SELECT hippo_fuzz('t_maint','t_maint_v','v',40,'maint: update');
-- delete + vacuum + refill holes
DELETE FROM t_maint WHERE id % 3 = 0;
VACUUM t_maint;
INSERT INTO t_maint (v, pad) SELECT (random()*30000)::int, repeat('w', 100) FROM generate_series(1, 30000);
SELECT hippo_fuzz('t_maint','t_maint_v','v',40,'maint: delete+vacuum+refill');
-- truncate the tail
DELETE FROM t_maint WHERE id > 100000;
VACUUM t_maint;
SELECT hippo_fuzz('t_maint','t_maint_v','v',30,'maint: tail truncated');
REINDEX INDEX t_maint_v;
SELECT hippo_fuzz('t_maint','t_maint_v','v',30,'maint: reindex');


-- ---------------------------------------------------------------- 2b. range-size cap extremes
DROP TABLE IF EXISTS t_cap;
CREATE TABLE t_cap (id serial, v int, pad text);
INSERT INTO t_cap (v, pad) SELECT (i/8) + (random()*20)::int, repeat('k', 120) FROM generate_series(1, 60000) i;
CREATE INDEX t_cap1 ON t_cap USING hippo (v) WITH (max_range_pages = 1, buckets = 16);
CREATE INDEX t_cap2 ON t_cap USING hippo (v) WITH (max_range_pages = 2, buckets = 1000, density = 1);
CREATE INDEX t_cap3 ON t_cap USING hippo (v) WITH (max_range_pages = 131072, density = 100);
SELECT hippo_fuzz('t_cap','t_cap1','v',20,'cap=1');
INSERT INTO t_cap (v, pad) SELECT i/8, repeat('k', 120) FROM generate_series(1, 30000) i;
SELECT hippo_fuzz('t_cap','t_cap1','v',20,'cap=1 after inserts');

-- ---------------------------------------------------------------- 3. build on empty/tiny tables
DROP TABLE IF EXISTS t_empty;
CREATE TABLE t_empty (id serial, v int, pad text);
CREATE INDEX t_empty_v ON t_empty USING hippo (v);
INSERT INTO t_empty (v, pad) SELECT i % 1000, repeat('e', 80) FROM generate_series(1, 20000) i;
SELECT hippo_fuzz('t_empty','t_empty_v','v',30,'empty then insert');
DROP TABLE IF EXISTS t_tiny;
CREATE TABLE t_tiny (id serial, v int);
INSERT INTO t_tiny (v) VALUES (1),(2),(3);
CREATE INDEX t_tiny_v ON t_tiny USING hippo (v);
SELECT hippo_check('t_tiny','t_tiny_v', w, 'tiny') FROM unnest(ARRAY['v = 2','v > 1','v < 3','v = 99','v >= 1']) w;
DROP TABLE IF EXISTS t_const;
CREATE TABLE t_const (id serial, v int, pad text);
INSERT INTO t_const (v, pad) SELECT 7, repeat('c', 80) FROM generate_series(1, 30000);
CREATE INDEX t_const_v ON t_const USING hippo (v);
INSERT INTO t_const (v, pad) SELECT 8, 'c' FROM generate_series(1, 10);
SELECT hippo_check('t_const','t_const_v', w, 'constant col') FROM unnest(ARRAY['v = 7','v = 8','v > 7','v < 7','v = 9']) w;

-- ---------------------------------------------------------------- 4. partial, expression, temp, concurrent, join
DROP TABLE IF EXISTS t_misc;
CREATE TABLE t_misc (id serial, v int, w int, pad text);
INSERT INTO t_misc (v, w, pad) SELECT i/3, i % 100, repeat('m', 60) FROM generate_series(1, 80000) i;
CREATE INDEX t_misc_partial ON t_misc USING hippo (v) WHERE w < 50;
CREATE INDEX t_misc_expr ON t_misc USING hippo ((v * 2));
SELECT hippo_check('t_misc','t_misc_partial', w, 'partial') FROM unnest(ARRAY['v > 1000 AND v < 1100 AND w < 50','v = 5000 AND w < 10','v < 50 AND w < 49']) w;
SELECT hippo_check('t_misc','t_misc_expr', w, 'expression') FROM unnest(ARRAY['(v*2) > 20000 AND (v*2) < 20100','(v*2) = 1000']) w;
CREATE INDEX CONCURRENTLY t_misc_conc ON t_misc USING hippo (v);
SELECT hippo_fuzz('t_misc','t_misc_conc','v',20,'concurrent build');
CREATE TEMP TABLE t_temp (id serial, v int, pad text);
INSERT INTO t_temp (v, pad) SELECT i/4, repeat('t', 60) FROM generate_series(1, 30000) i;
CREATE INDEX t_temp_v ON t_temp USING hippo (v);
INSERT INTO t_temp (v, pad) SELECT i/4, repeat('t', 60) FROM generate_series(30001, 40000) i;
SELECT hippo_fuzz('t_temp','t_temp_v','v',20,'temp table');
-- parameterised (join) scans
SET enable_seqscan = off; SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_nestloop = on;
CREATE TEMP TABLE probe AS SELECT (random()*26000)::int AS x FROM generate_series(1, 200);
DO $$ DECLARE a bigint; b bigint; BEGIN
  SET LOCAL enable_seqscan = off; SET LOCAL enable_indexscan = off; SET LOCAL enable_bitmapscan = on;
  SELECT count(*) INTO a FROM probe p JOIN t_misc m ON m.v = p.x;
  SET LOCAL enable_seqscan = on; SET LOCAL enable_bitmapscan = off;
  SELECT count(*) INTO b FROM probe p JOIN t_misc m ON m.v = p.x;
  IF a <> b THEN RAISE EXCEPTION 'join mismatch % vs %', a, b; END IF;
END $$;
RESET enable_seqscan; RESET enable_hashjoin; RESET enable_mergejoin;

\o
-- ---------------------------------------------------------------- summary
SELECT scenario, count(*) AS checks, sum(used_hippo) AS used_hippo_plan FROM hippo_chk_stats GROUP BY scenario ORDER BY 1;
SELECT 'ALL OK' AS result, count(*) AS total_checks, sum(used_hippo) AS used_hippo_plan FROM hippo_chk_stats;
