<p align="center">
  <img src="assets/hippo.jpg" alt="Hippo Logo" width="360" />
</p>

# hippo — a sparse histogram index for PostgreSQL, written in Rust

> **Proof of Concept**: `hippo` is an open-source proof-of-concept implementation of the indexing approach proposed in the research paper **[Hippo: A Fast, yet Scalable, Database Indexing Approach](https://arxiv.org/html/1604.03234v1)** (Jia Yu & Mohamed Sarwat, *SIGMOD 2016*).

`hippo` is a PostgreSQL **index access method** (`CREATE INDEX … USING hippo`) built with [pgrx](https://github.com/pgcentralfoundation/pgrx). It implements the sparse histogram index structure from the paper, adding features required for production databases: crash-safe Generic WAL logging, a data-layout-aware cost model, multi-type support, and tunable page-range sizing.

Like BRIN, a Hippo index stores one small summary per **range of heap pages**, keeping it orders of magnitude smaller than a B-tree and cheap to maintain. Unlike BRIN's min/max bounds, Hippo's summary is a **bitmap over an equi-depth histogram** (*"which value buckets occur in these pages?"*), which stays selective even when a page range contains outlier values.

```sql
CREATE EXTENSION hippo;

CREATE TABLE readings (id serial, val int, created_at timestamp);
CREATE INDEX ON readings USING hippo (val)
    WITH (buckets = 128, density = 20, max_range_pages = 128);

SELECT * FROM hippo_index_stats('readings_val_idx');   -- entries, ranges, overlap profile
```

---

## Benchmarks

### 1. Query Latency (PostgreSQL 16/18, 3 M rows, median of 7, warm cache)

| layout | index | size | 0.1 % range | 1 % range |
|---|---|---|---|---|
| sequential | seq scan | – | 190 ms | 173 ms |
| | B-tree | 64 MB | 1 ms | 6 ms |
| | BRIN minmax | 24 kB | 1 ms | 3 ms |
| | **Hippo** | 32 kB | 3 ms | 7 ms |
| noisy (±3 %) | seq scan | – | 206 ms | 171 ms |
| | B-tree | 57 MB | 1 ms | 10 ms |
| | BRIN minmax | 24 kB | 18 ms | 18 ms |
| | BRIN multi | 56 kB | 23 ms | 16 ms |
| | **Hippo** | 32 kB | 15 ms | 19 ms |
| random | seq scan | – | 199 ms | 197 ms |
| | B-tree | 56 MB | 2 ms | 28 ms |
| | BRIN minmax | 24 kB | 265 ms | 244 ms |
| | **Hippo** | 752 kB | 247 ms | 197 ms |

### 2. Memory Footprint & Buffer I/O (3 M rows, 242 MB heap; `tests/memory_bench.py`)

| layout | index | size on disk (= RAM to cache it) | build: private memory | 0.1 % query: 8 kB buffers (index + heap) | 1 % query |
|---|---|---|---|---|---|
| sequential | B-tree | 64.3 MB (26.6 % of heap) | 57.6 MB | 43 + 0 | 395 |
| | BRIN minmax | 24 kB | 0.1 MB | 9 + 128 = 137 | 393 |
| | **Hippo** | 32 kB | 0.9 MB | 3 + 512 = 515 | 1027 |
| noisy | B-tree | 56.6 MB | 57.6 MB | 3175 | 31815 |
| | BRIN minmax | 24 kB | 0.1 MB | 9 + 1920 = 1929 | 2313 |
| | **Hippo** | 32 kB | 0.9 MB | 3 + 2176 = 2179 | 2435 |
| random | B-tree | 56.4 MB | 57.6 MB | 10 + 2939 = 2949 | 19492 |
| | BRIN minmax | 24 kB | 0.1 MB | 9 + 30976 (whole table) | 30985 |
| | **Hippo** | 752 kB (0.3 %) | 0.9 MB | 93 + 16662 = 16755 | 24192 |

* **Private memory** is `RssAnon` growth of the building backend (what the index code allocates); B-tree's 57.6 MB is its sort buffer (`maintenance_work_mem`), whereas Hippo requires only ~0.9 MB for its sample.
* **Correlated data**: Hippo is as small as BRIN (~32 kB vs 24 kB) and 1000× smaller than a B-tree.
* **Random data**: Hippo is the only small index that still prunes pages (~54% of heap scanned vs. 100% for BRIN).

---

## Status & Capabilities

| Feature | Details |
|---|---|
| **Key types** | `int2`, `int4`, `int8`, `float4`, `float8`, `date`, `timestamp`, `timestamptz` (cross-type comparisons included) |
| **Operators** | `<  <=  =  >=  >` |
| **Scan type** | Bitmap index scans (heap pages are rechecked by PostgreSQL executor, guaranteeing exact results) |
| **Crash safety / replicas** | Yes — all page modifications are logged via PostgreSQL Generic WAL |
| **DML Support** | Inserts, updates (HOT updates preserved), deletes, `VACUUM`, `REINDEX`, `CREATE INDEX CONCURRENTLY` |
| **Index features** | Partial and expression indexes, temporary tables |
| **Not supported** | Unlogged tables, multi-column indexes, `INCLUDE`, parallel index scans, unique indexes, ordered scans, text/numeric/uuid |
| **Tested PG versions** | PostgreSQL 16 and 18. PostgreSQL 14, 15, and 17 are supported via feature flags and CI. |

---

## How It Works

* **Build** — Samples the column, derives equi-depth split points ("complete histogram"), scans the heap sequentially, and groups blocks into ranges until either the bucket **density** threshold or **`max_range_pages`** is reached. Each range produces one fixed-size entry: `{ start_page, end_page, bucket bitmap }`.
* **Search** — Converts query predicate operators and constants into an interval of buckets, creates a predicate bitmask, bitwise-ANDs it with each entry bitmap, and emits matching heap blocks to PostgreSQL's lossy TID bitmap.
* **Insert** — Looks up the entry covering the heap page. If the bucket bit is already set (the hot path), no disk writes or WAL logging are needed (shared lock only). Otherwise, sets the bit or appends a new entry at the tail.
* **Delete / VACUUM** — Over-approximations ensure deletions never invalidate index correctness; `VACUUM` requires zero index reorganization.
* **Planner** — Uses an empirical profile curve gathered at build time to predict selectivity based on actual physical data correlation.

### Differences from the 2016 Paper
* **Fixed-size entries**: Entries have uniform byte length and grow only at the tail, eliminating the need for the paper's sorted pointer list (section 5.3). Entry $i$ is directly computed in $O(1)$.
* **`max_range_pages` parameter**: Prevents well-correlated sequential data from growing entries that cover excessively large table ranges.
* **No lazy-delete re-summarisation**: Not required due to PostgreSQL's lossy bitmap heap recheck.

---

## Setup & Installation

### 1. Prerequisites
- **64-bit Linux** system
- **Rust toolchain** (1.80+)
- **PostgreSQL development headers** and compiler tools:
  - **Fedora / RHEL**:
    ```bash
    sudo dnf install -y postgresql-server-devel clang gcc make
    ```
  - **Debian / Ubuntu**:
    ```bash
    sudo apt install -y postgresql-server-dev-16 clang gcc make  # or postgresql-server-dev-18
    ```
- **`cargo-pgrx`** (v0.19.3):
  ```bash
  cargo install --locked cargo-pgrx --version 0.19.3
  ```

---

### 2. Building & Installing

#### Option A: Using Your System's PostgreSQL (e.g. PG 18 or 16)
Point `pgrx` to your system's `pg_config` and install:
```bash
# 1. Initialize pgrx against your local pg_config (instantaneous)
cargo pgrx init --pg18 $(which pg_config)     # or --pg16

# 2. Compile and install extension into PostgreSQL
cargo pgrx install --release --no-default-features --features pg18 --sudo
```

#### Option B: Using an Isolated `pgrx`-Managed PostgreSQL
If you don't have a local Postgres server running, let `pgrx` download and build an isolated cluster:
```bash
cargo pgrx init --pg16 download
cargo pgrx run pg16
```

---

## Running the Tests

### 1. Rust Unit Tests
Run in-process unit tests using `cargo-pgrx`:
```bash
cargo pgrx test pg18       # or pg16
```

### 2. Quick Smoke Test (`tests/smoke.sql`)
Verifies table creation, index construction, `hippo_index_stats`, and bitmap scan matching:
```bash
psql -d <your_db> -f tests/smoke.sql
```

### 3. Full Differential Correctness Suite (`tests/correctness.sql`)
Runs 900+ randomised differential checks comparing Hippo against forced sequential scans across all data types, edge cases (`NaN`, `±Infinity`, `NULL`), DDL/DML, and index variations:
```bash
psql -d <your_db> -f tests/correctness.sql
```
*(Prints scenario statistics and `ALL OK` upon completion).*

### 4. Memory Footprint & Buffer Benchmark (`tests/memory_bench.py`)
Measures resident memory (`VmHWM` / `RssAnon`) and 8 kB shared buffer hits across 3 million rows:
```bash
python3 tests/memory_bench.py psql -d <your_db>
```

---

## Tuning Options

```sql
CREATE INDEX ON events USING hippo (created_at)
    WITH (buckets = 128, density = 20, max_range_pages = 128);
```

| Option | Default | Description |
|---|---|---|
| `buckets` | `128` | Resolution of the complete histogram (8–1000). Higher values reduce false-positive page reads at the expense of larger entry bitmaps. |
| `density` | `20` | Percentage of buckets observed in a page range before finalizing the entry. Lower values create more, smaller entries. |
| `max_range_pages` | `128` | Maximum number of heap pages per entry. Keeps range sizes bounded on correlated data. |

---

## Known Limitations

* The histogram is fixed at build time. If the value distribution drifts significantly over time, run `REINDEX`.
* Values outside the build-time range land in the first/last bucket, which maintains correctness but reduces selectivity.
* Bitmaps are currently uncompressed (`buckets / 8` bytes per entry).
* Generic WAL logs page deltas, which is reliable and crash-safe but larger than a dedicated custom WAL resource manager.

---

## License

Licensed under either of [Apache License, Version 2.0](LICENSE) or [MIT License](LICENSE) at your option.
