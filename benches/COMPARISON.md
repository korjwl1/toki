# toki vs ccusage Performance Comparison

## Basic Profile

| Item | toki | ccusage |
|------|------|---------|
| Language | Rust (Edition 2021) | TypeScript (Node.js/Bun) |
| Execution Model | Daemon/client (persistent) | Batch CLI (run → aggregate → exit) |
| DB | fjall (embedded LSM-tree TSDB) | None (stateless) |
| Incremental Processing | Checkpoint-based resume | None (full re-scan every time) |
| Deduplication | xxHash3-64 line hash | messageId:requestId Set |

---

## Running Benchmarks

```bash
# 1. Generate test data (based on real ~/.claude data)
python3 benches/benchmark.py generate --sizes 100,200,500,1000,2000

# 2. Run benchmarks
python3 benches/benchmark.py run                       # both
python3 benches/benchmark.py run --tool toki            # toki only
python3 benches/benchmark.py run --tool ccusage          # ccusage only
python3 benches/benchmark.py run --sizes 100,200         # specific sizes only

# 3. Generate charts
python3 benches/benchmark.py plot

# All-in-one (generate + run + plot)
python3 benches/benchmark.py all
```

- Process monitoring: 50ms interval CPU%/RSS sampling
- N iterations (default 3), averaged
- Results: `benches/results/` (CSV + JSON + PNG/SVG charts)

---

## Phase 1: Cold Start (Full File Indexing)

> **Fair comparison point**: Both tools read all JSONL files from scratch and aggregate.

| Tool | Action |
|------|--------|
| toki | `daemon reset` → `daemon start` (measured until cold start completion) |
| ccusage | `ccusage` (read all files → aggregate → output) |

### Why the Difference

1. **Parallel file processing**: toki uses rayon to process all session files in parallel up to CPU core count.
   ccusage uses sequential stream readline.
2. **Parsing performance (3-5x)**: Rust serde_json is 2-5x faster per line vs Node.js JSON.parse + Valibot validation.
3. **Memory**: toki accumulates events immediately (O(M) for model count).
   ccusage collects all entries into an array then groupBy → O(N) memory.
4. **Extra overhead**: toki writes events/rollups to TSDB + stores checkpoints.
   ccusage just outputs and exits. Despite this overhead, toki is faster.

---

## Phase 2: Report (Pre-indexed vs Full Re-read)

> **Core structural difference**: toki queries TSDB data already indexed in Phase 1.
> ccusage re-reads all files on every execution.

| Scenario | toki | ccusage |
|----------|------|---------|
| Full summary | TSDB rollup query | Full file re-scan |
| daily/weekly/monthly | TSDB time-range query | Full file re-scan + grouping |
| Session/project filter | TSDB index lookup | Full file re-scan + filter |
| PromQL query | TSDB query engine | Not supported |

```
toki report:   O(R)     R = rollup count (time buckets × model count, typically hundreds)
ccusage:       O(N)     N = total line count (tens to hundreds of thousands)
```

---

## Scaling Predictions by Data Size

| Scale | Line Count | toki cold start | ccusage | toki report | ccusage report |
|-------|------------|-----------------|---------|-------------|----------------|
| Small | 1K | ~100ms | ~300ms | ~5ms | ~300ms |
| Medium | 50K | ~3s | ~10s | ~5ms | ~10s |
| Large | 500K | ~20s | ~2min+ | ~5ms | ~2min+ |
| Very Large | 5M | ~3min | OOM risk | ~5ms | OOM |

> toki report queries TSDB rollups, so it's **~5ms regardless of source data size**.

---

## Real-time Collection (Watch Mode)

A toki-exclusive feature. ccusage has no equivalent.

| Aspect | toki | ccusage |
|--------|------|---------|
| Change detection | FSEvents → checkpoint resume | Full re-scan every time |
| Latency | ~1-2ms (immediate on event) | ~5-15s (batch re-execution) |
| CPU (idle) | ~0% | N/A (not running) |
| Processing 10 new lines | ~500µs | ~5-15s (full recalculation) |
| Server delivery | Event-driven push | Requires polling |
| Time Complexity | O(ΔL) new lines only | O(N) full reprocessing |

---

## Architecture Comparison

```
toki:     O(N) once initially + O(R) per report + O(ΔL) real-time
ccusage:  O(N) × number of executions
```

| Item | toki | ccusage |
|------|------|---------|
| Incremental processing | checkpoint + reverse-scan resume | None |
| File compaction recovery | Hash-based automatic recovery | N/A (full re-read every time) |
| Memory efficiency | O(F) checkpoint maintenance | O(N) all entries loaded in memory |
| Binary size | ~2-5MB (native) | ~hundreds of KB + Node.js runtime |
| Deployment | Single binary | npm/bun install |
| Pricing | LiteLLM (ETag caching, client-side) | LiteLLM (online/offline) |
| Report variety | summary/daily/weekly/monthly/yearly/hourly/session/project/PromQL | daily/weekly/monthly/session |

---

## Conclusion

| Use Case | Recommendation |
|----------|---------------|
| Repeated reports (daily check, etc.) | **toki** — ~5ms from indexed TSDB, ccusage re-scans everything |
| Real-time/near-real-time collection | **toki** — O(ΔL) incremental processing, ms latency |
| Large scale (500K+ lines) | **toki** — ccusage risks OOM, toki report is data-size independent |
| One-off quick check | **ccusage** — single npm install line, no daemon needed |
