-- Hippo vs B-tree vs BRIN. 3M rows, three physical layouts, two selectivities.
\set ON_ERROR_STOP on
\timing off
SET client_min_messages = warning;
CREATE EXTENSION IF NOT EXISTS hippo;
DROP TABLE IF EXISTS bench_res; CREATE TABLE bench_res (layout text, idx text, build_ms numeric, size_kb bigint, sel text, ms numeric, plan_uses_idx bool);

CREATE OR REPLACE FUNCTION bench_q(lo int, hi int, idxname text, OUT ms numeric, OUT used bool) AS $$
DECLARE t0 timestamptz; r int[] := '{}'; i int; p text; c bigint;
BEGIN
  used := false;
  FOR p IN EXECUTE format('EXPLAIN SELECT count(*) FROM bench WHERE v BETWEEN %s AND %s', lo, hi) LOOP
    IF p LIKE '%' || idxname || '%' THEN used := true; END IF;
  END LOOP;
  FOR i IN 1..7 LOOP
    t0 := clock_timestamp();
    EXECUTE format('SELECT count(*) FROM bench WHERE v BETWEEN %s AND %s', lo, hi) INTO c;
    r := r || (extract(epoch FROM clock_timestamp() - t0) * 1000)::int;
  END LOOP;
  SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x)::numeric INTO ms FROM unnest(r) x;
END $$ LANGUAGE plpgsql;

CREATE OR REPLACE PROCEDURE run_layout(layout text, gen text) AS $$
DECLARE idef text[] := ARRAY['btree','brin_minmax','brin_multi','hippo']; ix text; t0 timestamptz; sql text;
        sels text[] := ARRAY['0.1%','1%']; s text; n int := 3000000; lo int; w int; res record; sz bigint; bms numeric;
BEGIN
  DROP TABLE IF EXISTS bench;
  EXECUTE format('CREATE TABLE bench AS SELECT i AS id, %s AS v, repeat(''p'', 40) AS pad FROM generate_series(1, %s) i', gen, n);
  EXECUTE 'ANALYZE bench';
  FOREACH ix IN ARRAY idef LOOP
    sql := CASE ix
      WHEN 'btree' THEN 'CREATE INDEX bench_idx ON bench USING btree (v)'
      WHEN 'brin_minmax' THEN 'CREATE INDEX bench_idx ON bench USING brin (v)'
      WHEN 'brin_multi' THEN 'CREATE INDEX bench_idx ON bench USING brin (v int4_minmax_multi_ops)'
      ELSE 'CREATE INDEX bench_idx ON bench USING hippo (v)' END;
    t0 := clock_timestamp(); EXECUTE sql; bms := extract(epoch FROM clock_timestamp() - t0) * 1000;
    sz := pg_relation_size('bench_idx') / 1024;
    PERFORM set_config('enable_seqscan','off',true);
    FOREACH s IN ARRAY sels LOOP
      w := CASE s WHEN '0.1%' THEN n/1000 ELSE n/100 END;
      SELECT (min(v) + (max(v)-min(v))*0.37)::int INTO lo FROM bench;
      -- window by value range in "row count" units of the value domain
      SELECT * INTO res FROM bench_q(lo, lo + (SELECT ((max(v)-min(v))::numeric * (CASE s WHEN '0.1%' THEN 0.001 ELSE 0.01 END))::int FROM bench), 'bench_idx');
      INSERT INTO bench_res VALUES (layout, ix, round(bms), sz, s, res.ms, res.used);
    END LOOP;
    PERFORM set_config('enable_seqscan','on',true);
    DROP INDEX bench_idx;
  END LOOP;
  -- seq scan baseline
  FOREACH s IN ARRAY sels LOOP
    SELECT (min(v) + (max(v)-min(v))*0.37)::int INTO lo FROM bench;
    SELECT * INTO res FROM bench_q(lo, lo + (SELECT ((max(v)-min(v))::numeric * (CASE s WHEN '0.1%' THEN 0.001 ELSE 0.01 END))::int FROM bench), 'nonexistent');
    INSERT INTO bench_res VALUES (layout, 'seqscan', 0, 0, s, res.ms, false);
  END LOOP;
END $$ LANGUAGE plpgsql;

SET max_parallel_workers_per_gather = 0;
CALL run_layout('sequential (v = id)', 'i');
CALL run_layout('noisy (id +/- 3%)', 'i + ((random()-0.5) * 0.06 * 3000000)::int');
CALL run_layout('random', '(random() * 3000000)::int');

SELECT layout, idx, size_kb, build_ms,
       max(ms) FILTER (WHERE sel = '0.1%') AS "ms @0.1%", max(ms) FILTER (WHERE sel = '1%') AS "ms @1%",
       bool_and(plan_uses_idx) AS used
FROM bench_res GROUP BY layout, idx, size_kb, build_ms
ORDER BY layout, CASE idx WHEN 'seqscan' THEN 0 WHEN 'btree' THEN 1 WHEN 'brin_minmax' THEN 2 WHEN 'brin_multi' THEN 3 ELSE 4 END;
