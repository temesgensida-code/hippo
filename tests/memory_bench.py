#!/usr/bin/env python3
"""Memory footprint comparison: Hippo vs B-tree vs BRIN.

Measures, per data layout and index type:
  * index size on disk (== RAM needed to cache the whole index in shared_buffers)
  * peak resident memory of the backend that builds the index (VmHWM delta, Linux only)
  * 8 kB buffers touched by a 0.1% and a 1% range query, split into index and heap blocks
Usage: memory_bench.py <psql command prefix...>   e.g. psql -h /tmp/hippo_pg -p 54329 -d t
"""
import json, re, subprocess, sys, tempfile, os

PSQL = sys.argv[1:] or ["psql", "-d", "postgres"]
N = 3_000_000
LAYOUTS = {
    "sequential": "i",
    "noisy (+/-3%)": "i + ((random()-0.5) * 0.06 * 3000000)::int",
    "random": "(random() * 3000000)::int",
}
INDEXES = {
    "btree": "CREATE INDEX bidx ON bm USING btree (v)",
    "brin minmax": "CREATE INDEX bidx ON bm USING brin (v)",
    "brin multi": "CREATE INDEX bidx ON bm USING brin (v int4_minmax_multi_ops)",
    "hippo": "CREATE INDEX bidx ON bm USING hippo (v)",
}

def psql(sql, tuples=True):
    args = PSQL + ["-X", "-q", "-v", "ON_ERROR_STOP=1"] + (["-At"] if tuples else []) + ["-c", sql]
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout.strip()

def vmhwm_kb(pid):
    with open(f"/proc/{pid}/status") as f:
        return int(re.search(r"VmHWM:\s+(\d+)", f.read()).group(1))

def rss_anon_kb(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            return int(re.search(r"RssAnon:\s+(\d+)", f.read()).group(1))
    except (OSError, AttributeError):
        return 0

def build_rss(create_sql):
    """Run CREATE INDEX in a fresh backend.
    Returns (peak private memory growth MB, peak total RSS growth MB).
    Private = RssAnon, polled every 5 ms (malloc'd/palloc'd memory: what the index code allocates).
    Total RSS (VmHWM) additionally counts shared_buffers pages the backend touched while reading the heap."""
    import threading, time
    script = f"SELECT pg_backend_pid();\n\\! echo BEFORE\n{create_sql};\n\\! echo AFTER\nSELECT pg_sleep(1);\n"
    p = subprocess.Popen(PSQL + ["-X", "-At", "-v", "ON_ERROR_STOP=1"], stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    p.stdin.write(script); p.stdin.close()
    st = {"pid": None, "hwm0": 0, "anon0": 0, "anon_peak": 0, "stop": False, "hwm1": 0}
    def poll():
        while not st["stop"]:
            st["anon_peak"] = max(st["anon_peak"], rss_anon_kb(st["pid"]))
            time.sleep(0.005)
    th = None
    for line in p.stdout:
        line = line.strip()
        if st["pid"] is None and line.isdigit():
            st["pid"] = int(line)
        elif line == "BEFORE":
            st["hwm0"] = vmhwm_kb(st["pid"]); st["anon0"] = rss_anon_kb(st["pid"]); st["anon_peak"] = st["anon0"]
            th = threading.Thread(target=poll, daemon=True); th.start()
        elif line == "AFTER":
            st["stop"] = True; th.join()
            st["hwm1"] = vmhwm_kb(st["pid"])
    p.wait()
    return max(0, st["anon_peak"] - st["anon0"]) / 1024.0, max(0, st["hwm1"] - st["hwm0"]) / 1024.0

def buffers(lo, hi):
    plan = json.loads(psql(f"EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT count(*) FROM bm WHERE v BETWEEN {lo} AND {hi}"))[0]["Plan"]
    def blocks(n): return n.get("Shared Hit Blocks", 0) + n.get("Shared Read Blocks", 0)
    idx = []
    def walk(n):
        if "Index" in n.get("Node Type", "") and n.get("Index Name") == "bidx": idx.append(blocks(n))
        for c in n.get("Plans", []): walk(c)
    walk(plan)
    total, i = blocks(plan), sum(idx)
    return i, total - i

rows = []
for layout, gen in LAYOUTS.items():
    psql("DROP TABLE IF EXISTS bm")
    psql(f"CREATE TABLE bm AS SELECT i AS id, {gen} AS v, repeat('p', 40) AS pad FROM generate_series(1, {N}) i")
    psql("ANALYZE bm")
    heap_mb = int(psql("SELECT pg_relation_size('bm')")) / 2**20
    mn, mx = map(int, psql("SELECT min(v), max(v) FROM bm").split("|"))
    lo = int(mn + (mx - mn) * 0.37)
    w1, w2 = int((mx - mn) * 0.001), int((mx - mn) * 0.01)
    for name, ddl in INDEXES.items():
        psql("DROP INDEX IF EXISTS bidx")
        rss, rss_total = build_rss(ddl)
        size = int(psql("SELECT pg_relation_size('bidx')")) / 1024
        psql("SET enable_seqscan = off")  # harmless in a separate session; real forcing below
        res = {}
        for tag, w in (("0.1%", w1), ("1%", w2)):
            args = PSQL + ["-X", "-At", "-v", "ON_ERROR_STOP=1", "-c", "SET enable_seqscan=off; SET max_parallel_workers_per_gather=0;",
                           "-c", f"EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT count(*) FROM bm WHERE v BETWEEN {lo} AND {lo + w}"]
            out = subprocess.run(args, check=True, capture_output=True, text=True).stdout
            plan = json.loads(out[out.index("["):])[0]["Plan"]
            def blocks(n): return n.get("Shared Hit Blocks", 0) + n.get("Shared Read Blocks", 0)
            idx = []
            def walk(n):
                if n.get("Index Name") == "bidx": idx.append(blocks(n))
                for c in n.get("Plans", []): walk(c)
            walk(plan)
            res[tag] = (sum(idx), blocks(plan) - sum(idx))
        rows.append((layout, name, size, size / 1024 / heap_mb * 100, (rss, rss_total), res["0.1%"], res["1%"]))
    rows.append((layout, "(heap)", heap_mb * 1024, 100.0, (0.0, 0.0), None, None))

print(f"{'layout':<15}{'index':<13}{'size':>10}{'% of heap':>11}{'build priv':>11}{'build RSS':>11}   {'0.1% query: idx+heap bufs':<27}{'1% query: idx+heap bufs'}")
for layout, name, size, pct, rss, a, b in rows:
    if name == "(heap)":
        print(f"{layout:<15}{'table heap':<13}{size/1024:>8.1f}MB{'':>11}{'':>11}"); continue
    sz = f"{size:.0f}kB" if size < 1024 else f"{size/1024:.1f}MB"
    f = lambda t: f"{t[0]:>6} + {t[1]:<7}= {t[0]+t[1]:<7}"
    print(f"{layout:<15}{name:<13}{sz:>10}{pct:>10.2f}%{rss[0]:>9.1f}MB{rss[1]:>9.1f}MB   {f(a):<27}{f(b)}")
