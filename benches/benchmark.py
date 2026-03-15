#!/usr/bin/env python3
"""
toki vs ccusage benchmark tool.

Generates test data sets of various sizes from real ~/.claude session data,
then measures execution time, peak/avg CPU%, and peak/avg RSS for both tools
across multiple report scenarios.

Architecture:
  toki uses a daemon/client model:
    Phase 1 (cold start): daemon reset → daemon start → index all files into TSDB
    Phase 2 (report):     daemon running → toki report (reads pre-indexed TSDB)
  ccusage is stateless:
    Every report re-reads all files from scratch.

Usage:
    python3 benchmark.py generate [--sizes 100,200,500]
    python3 benchmark.py run [--runs 3] [--sizes 100,200] [--tool toki|ccusage|both]
    python3 benchmark.py all [--runs 3]
    python3 benchmark.py plot [--file results/benchmark_xxx.json]
"""

import argparse
import csv
import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
import uuid
from dataclasses import dataclass, asdict
from datetime import datetime
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent
DATA_DIR = SCRIPT_DIR / "data"
RESULTS_DIR = SCRIPT_DIR / "results"
PROJECT_ROOT = SCRIPT_DIR.parent

DEFAULT_SIZES_MB = [100, 200, 300, 400, 500, 1000, 1500, 2000]
DEFAULT_RUNS = 3
POLL_INTERVAL_S = 0.05  # 50ms sampling

# Report scenarios to benchmark (Phase 2).
# (name, toki_args, ccusage_args, zzusage_args)
SCENARIOS = [
    ("total",   [],                              [],          []),
    ("daily",   ["daily", "--from-beginning"],    ["daily"],   ["daily"]),
    ("weekly",  ["weekly", "--from-beginning"],   ["weekly"],  ["weekly"]),
    ("monthly", ["monthly"],                      ["monthly"], ["monthly"]),
    ("yearly",  ["yearly"],                       ["yearly"],  []),
]


# ---------------------------------------------------------------------------
# Data generation
# ---------------------------------------------------------------------------

def get_dir_size_bytes(path: Path) -> int:
    """Get actual disk usage (not pre-allocated size) using st_blocks."""
    total = 0
    for dirpath, _, filenames in os.walk(path):
        for f in filenames:
            fp = os.path.join(dirpath, f)
            if os.path.isfile(fp):
                stat = os.stat(fp)
                # Use st_blocks (512-byte blocks) for actual disk usage,
                # avoiding fjall's 64MB pre-allocated WAL inflation.
                total += stat.st_blocks * 512
    return total


def collect_projects(source_root: Path):
    """Return [(project_path, size_bytes)] sorted by size descending."""
    projects_dir = source_root / "projects"
    if not projects_dir.exists():
        print(f"Error: {projects_dir} not found")
        sys.exit(1)

    projects = []
    for entry in sorted(projects_dir.iterdir()):
        if entry.is_dir():
            size = get_dir_size_bytes(entry)
            if size > 0:
                projects.append((entry, size))

    projects.sort(key=lambda x: -x[1])
    return projects


def randomize_jsonl(src_path: Path, dst_path: Path, ts_offset_s: int):
    """Copy a JSONL file, randomizing message IDs and shifting timestamps."""
    import random

    with open(src_path, "r") as fin, open(dst_path, "w") as fout:
        for line in fin:
            stripped = line.strip()
            if not stripped:
                fout.write(line)
                continue

            try:
                d = json.loads(stripped)
            except json.JSONDecodeError:
                fout.write(line)
                continue

            # Randomize timestamp (shift by offset + small jitter)
            if "timestamp" in d and isinstance(d["timestamp"], str):
                try:
                    from datetime import datetime, timedelta, timezone
                    dt = datetime.fromisoformat(d["timestamp"].replace("Z", "+00:00"))
                    jitter = random.randint(-3600, 3600)  # ±1h jitter
                    dt = dt + timedelta(seconds=ts_offset_s + jitter)
                    d["timestamp"] = dt.strftime("%Y-%m-%dT%H:%M:%S.") + f"{random.randint(0,999):03d}Z"
                except (ValueError, OSError):
                    pass

            # Randomize message.id (unique per event)
            if "message" in d and isinstance(d["message"], dict) and "id" in d["message"]:
                d["message"]["id"] = f"msg_{uuid.uuid4().hex[:24]}"

            fout.write(json.dumps(d, separators=(",", ":")) + "\n")


def copy_project(src_dir: Path, dst_dir: Path, round_idx: int):
    """Copy a project dir with randomized session IDs, message IDs, and timestamps."""
    dst_dir.mkdir(parents=True, exist_ok=True)

    # Shift timestamps by round * 30 days to spread across time
    ts_offset = round_idx * 30 * 86400

    for item in src_dir.iterdir():
        if item.is_file() and item.suffix == ".jsonl":
            new_uuid = str(uuid.uuid4())
            randomize_jsonl(item, dst_dir / f"{new_uuid}.jsonl", ts_offset)

            # Copy subagent dir if it exists
            sub_dir = src_dir / item.stem / "subagents"
            if sub_dir.exists():
                new_sub_dir = dst_dir / new_uuid / "subagents"
                new_sub_dir.mkdir(parents=True, exist_ok=True)
                for sub in sub_dir.iterdir():
                    if sub.is_file() and sub.suffix == ".jsonl":
                        randomize_jsonl(sub, new_sub_dir / sub.name, ts_offset)


def generate_data_set(projects, target_bytes: int, dest: Path):
    """Build a data set directory targeting the given size."""
    projects_dst = dest / "projects"
    projects_dst.mkdir(parents=True, exist_ok=True)

    current = 0
    copy_round = 0

    while current < target_bytes:
        for src_path, src_size in projects:
            if current >= target_bytes:
                break

            name = src_path.name
            if copy_round > 0:
                name = f"{name}-bench{copy_round}"

            dst = projects_dst / name
            if dst.exists():
                copy_round += 1
                name = f"{src_path.name}-bench{copy_round}"
                dst = projects_dst / name

            copy_project(src_path, dst, copy_round)
            current += src_size

        copy_round += 1


def cmd_generate(args):
    source = Path(args.source).expanduser()
    projects = collect_projects(source)
    total_src = sum(s for _, s in projects)

    print(f"Source: {source / 'projects'}")
    print(f"  {len(projects)} projects, {total_src / 1024 / 1024:.0f} MB")
    print()

    sizes = parse_sizes(args.sizes)
    DATA_DIR.mkdir(parents=True, exist_ok=True)

    force = getattr(args, "force", False)

    for target_mb in sizes:
        target_bytes = target_mb * 1024 * 1024
        set_dir = DATA_DIR / f"{target_mb}mb"

        if set_dir.exists():
            if force:
                print(f"  {target_mb}MB: regenerating (--force)")
                shutil.rmtree(set_dir)
            else:
                existing = get_dir_size_bytes(set_dir)
                existing_mb = existing / 1024 / 1024
                if abs(existing_mb - target_mb) < target_mb * 0.15:
                    print(f"  {target_mb}MB: exists ({existing_mb:.0f} MB), skipping")
                    continue
                print(f"  {target_mb}MB: exists but wrong size ({existing_mb:.0f} MB), regenerating")
                shutil.rmtree(set_dir)

        print(f"  {target_mb}MB: generating...", end="", flush=True)
        generate_data_set(projects, target_bytes, set_dir)
        actual = get_dir_size_bytes(set_dir) / 1024 / 1024
        print(f" done ({actual:.0f} MB)")

    print("\nData generation complete.")


# ---------------------------------------------------------------------------
# Process monitoring
# ---------------------------------------------------------------------------

class ProcessMonitor:
    """Poll ps for CPU% and RSS of a running process."""

    def __init__(self, pid: int):
        self.pid = pid
        self._samples: list[tuple[float, float, int]] = []  # (time, cpu%, rss_kb)
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self):
        self._thread = threading.Thread(target=self._poll, daemon=True)
        self._thread.start()

    def stop(self):
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=2)

    def _poll(self):
        while not self._stop.is_set():
            try:
                r = subprocess.run(
                    ["ps", "-p", str(self.pid), "-o", "%cpu=,rss="],
                    capture_output=True, text=True, timeout=1,
                )
                if r.returncode == 0 and r.stdout.strip():
                    parts = r.stdout.split()
                    if len(parts) >= 2:
                        cpu = float(parts[0])
                        rss = int(parts[1])  # KB
                        self._samples.append((time.monotonic(), cpu, rss))
            except (subprocess.TimeoutExpired, ValueError, IndexError):
                pass
            self._stop.wait(POLL_INTERVAL_S)

    def stats(self) -> dict:
        if not self._samples:
            return {
                "samples": 0,
                "peak_rss_mb": 0, "avg_rss_mb": 0,
                "peak_cpu_pct": 0, "avg_cpu_pct": 0,
            }
        rss = [s[2] for s in self._samples]
        cpu = [s[1] for s in self._samples]
        return {
            "samples": len(self._samples),
            "peak_rss_mb": round(max(rss) / 1024, 2),
            "avg_rss_mb": round(sum(rss) / len(rss) / 1024, 2),
            "peak_cpu_pct": round(max(cpu), 1),
            "avg_cpu_pct": round(sum(cpu) / len(cpu), 1),
        }


# ---------------------------------------------------------------------------
# Benchmark execution
# ---------------------------------------------------------------------------

@dataclass
class BenchResult:
    tool: str
    data_label: str
    data_size_mb: int
    scenario: str
    run: int
    wall_time_s: float
    peak_rss_mb: float
    avg_rss_mb: float
    peak_cpu_pct: float
    avg_cpu_pct: float
    samples: int
    exit_code: int
    db_size_mb: float = 0.0


def purge_disk_cache():
    """Purge macOS disk cache for fair cold-cache benchmarking."""
    subprocess.run(["sudo", "-n", "purge"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(0.5)


def run_once(cmd: list[str], env_extra: dict | None = None) -> BenchResult:
    """Run a command and measure its performance."""
    merged_env = os.environ.copy()
    if env_extra:
        merged_env.update(env_extra)

    t0 = time.perf_counter()
    proc = subprocess.Popen(
        cmd, env=merged_env,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )

    mon = ProcessMonitor(proc.pid)
    mon.start()
    proc.wait()
    elapsed = time.perf_counter() - t0
    mon.stop()

    s = mon.stats()
    return BenchResult(
        tool="", data_label="", data_size_mb=0, scenario="", run=0,
        wall_time_s=round(elapsed, 4),
        exit_code=proc.returncode,
        **s,
    )


def run_daemon_cold_start(toki: str, data_path: Path) -> BenchResult:
    """Start toki daemon, wait for cold start to complete, measure performance.

    Returns BenchResult with cold start timing. Daemon keeps running for report benchmarks.
    """
    # Reset DB and set claude_code_root to benchmark data path
    subprocess.run(
        [toki, "daemon", "reset"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    subprocess.run(
        [toki, "settings", "set", "claude_code_root", str(data_path)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )

    # Start daemon with TOKI_DEBUG=1 to capture cold_start timing
    log_path = SCRIPT_DIR / ".daemon.log"
    log_file = open(log_path, "w")

    env = os.environ.copy()
    env["TOKI_DEBUG"] = "1"

    proc = subprocess.Popen(
        [toki, "daemon", "start"],
        stdout=log_file, stderr=subprocess.STDOUT,
        env=env,
    )

    mon = ProcessMonitor(proc.pid)
    mon.start()

    # Wait for "Watching:" = daemon ready
    timeout = 600  # 10 min max
    start = time.monotonic()
    while time.monotonic() - start < timeout:
        if proc.poll() is not None:
            break
        try:
            with open(log_path, "r") as f:
                content = f.read()
            if "Watching:" in content:
                break
        except OSError:
            pass
        time.sleep(0.05)

    mon.stop()
    log_file.close()

    # Parse pure cold_start time from debug log (excludes daemon startup overhead)
    elapsed = 0.0
    try:
        with open(log_path, "r") as f:
            for line in f:
                # Format: [toki:debug] cold_start — ... (123456µs)
                if "cold_start" in line and "µs)" in line:
                    import re
                    m = re.search(r'\((\d+)µs\)', line)
                    if m:
                        elapsed = int(m.group(1)) / 1_000_000  # µs → seconds
                    break
    except OSError:
        pass

    # Fallback: if debug log parsing fails, use wall time
    if elapsed == 0.0:
        elapsed = time.monotonic() - start

    s = mon.stats()
    result = BenchResult(
        tool="toki", data_label="", data_size_mb=0,
        scenario="cold_start", run=0,
        wall_time_s=round(elapsed, 4),
        exit_code=0 if proc.poll() is None else proc.returncode,
        **s,
    )

    # DB size (default path)
    default_db = Path.home() / ".config" / "toki" / "toki.fjall"
    result.db_size_mb = round(get_dir_size_bytes(default_db) / 1024 / 1024, 2) if default_db.exists() else 0.0

    return result


def stop_daemon(toki: str):
    """Stop the running daemon."""
    subprocess.run(
        [toki, "daemon", "stop"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(0.3)



def find_toki() -> str:
    print("Building toki (release)...")
    subprocess.run(
        ["cargo", "build", "--release"],
        cwd=PROJECT_ROOT, check=True,
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
    )
    binary = PROJECT_ROOT / "target" / "release" / "toki"
    if not binary.exists():
        print(f"Error: {binary} not found")
        sys.exit(1)
    print(f"  toki: {binary}")
    return str(binary)


def find_ccusage() -> list[str] | None:
    """Find ccusage command. Returns command list or None."""
    for name in ["ccusage"]:
        try:
            r = subprocess.run(
                [name, "--version"], capture_output=True, text=True, timeout=10,
            )
            if r.returncode == 0:
                print(f"  ccusage: {name} (v{r.stdout.strip()})")
                return [name]
        except (FileNotFoundError, subprocess.TimeoutExpired):
            pass

    # Try npx
    try:
        r = subprocess.run(
            ["npx", "--yes", "ccusage", "--version"],
            capture_output=True, text=True, timeout=30,
        )
        if r.returncode == 0:
            print(f"  ccusage: npx (v{r.stdout.strip()})")
            return ["npx", "--yes", "ccusage"]
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass

    print("  ccusage: not found, will skip")
    return None


def find_zzusage() -> str | None:
    """Find zzusage binary. Returns path or None."""
    for path in ["zzusage", "/tmp/zzusage/zig-out/bin/zzusage"]:
        try:
            r = subprocess.run(
                [path, "--help"], capture_output=True, text=True, timeout=5,
            )
            if r.returncode == 0:
                print(f"  zzusage: {path}")
                return path
        except (FileNotFoundError, subprocess.TimeoutExpired):
            pass
    print("  zzusage: not found, will skip")
    return None


def discover_data_sets(sizes: list[int] | None) -> list[tuple[str, Path, int]]:
    """Return [(label, path, size_mb)] for available data sets."""
    if not DATA_DIR.exists():
        return []

    sets = []
    for d in sorted(DATA_DIR.iterdir()):
        if d.is_dir() and d.name.endswith("mb"):
            size_mb = get_dir_size_bytes(d) // (1024 * 1024)
            label = d.name
            nominal = int(label.replace("mb", ""))
            if sizes and nominal not in sizes:
                continue
            sets.append((label, d, size_mb))

    sets.sort(key=lambda x: x[2])
    return sets


def cmd_run(args):
    runs = args.runs
    sizes = parse_sizes(args.sizes)
    tool_filter = getattr(args, "tool", "both")
    use_purge = getattr(args, "purge", False)

    print("=== Benchmark Setup ===")

    toki = find_toki() if tool_filter in ("toki", "both", "all") else None
    ccusage = find_ccusage() if tool_filter in ("ccusage", "both", "all") else None
    zzusage = find_zzusage() if tool_filter in ("zzusage", "both", "all") else None

    if use_purge:
        # Verify sudo works without password (user should run: sudo -v first)
        r = subprocess.run(["sudo", "-n", "purge"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if r.returncode != 0:
            print("Error: --purge requires passwordless sudo. Run 'sudo -v' first.")
            sys.exit(1)
        print("  purge: enabled (disk cache cleared before each run)")

    if not toki and not ccusage and not zzusage:
        print("Error: No tools available to benchmark.")
        sys.exit(1)

    data_sets = discover_data_sets(sizes)
    if not data_sets:
        print("\nError: No data sets found. Run 'generate' first.")
        sys.exit(1)

    print(f"\nData sets: {', '.join(d[0] for d in data_sets)}")
    print(f"Tool(s): {tool_filter}")
    print(f"Runs per scenario: {runs}")

    # Count total runs
    # toki: cold_start × runs + len(SCENARIOS) × runs reports per data set
    # ccusage/zzusage: len(SCENARIOS) × runs per data set
    toki_runs_per_ds = (runs + len(SCENARIOS) * runs) if toki else 0
    cc_runs_per_ds = (len(SCENARIOS) * runs) if ccusage else 0
    zz_scenarios = sum(1 for _, _, _, zz in SCENARIOS if zz or _ == "total")
    zz_runs_per_ds = (zz_scenarios * runs) if zzusage else 0
    total_runs = len(data_sets) * (toki_runs_per_ds + cc_runs_per_ds + zz_runs_per_ds)
    print(f"Total benchmark runs: {total_runs}\n")

    # Warm-up
    if ccusage:
        print("Warming up ccusage...")
        first_data = data_sets[0][1]
        warm_env = {"CLAUDE_CONFIG_DIR": str(first_data)}
        subprocess.run(
            ccusage + ["--offline"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            env={**os.environ, **warm_env},
        )
    print()

    results: list[BenchResult] = []
    run_idx = 0

    print("=== Running Benchmarks ===")
    for label, data_path, size_mb in data_sets:
        print(f"\n--- {label} ({size_mb} MB) ---")

        # ── toki: Phase 1 (cold start × runs) + Phase 2 (reports) ──
        if toki:
            for i in range(1, runs + 1):
                if use_purge:
                    purge_disk_cache()
                run_idx += 1
                tag = f"[{run_idx}/{total_runs}]"
                print(f"  {tag} toki       | {label:8s} | cold_start | run {i}", end="", flush=True)

                r = run_daemon_cold_start(toki, data_path)
                r.data_label = label
                r.data_size_mb = size_mb
                r.run = i
                results.append(r)

                status = "OK" if r.exit_code == 0 else f"EXIT={r.exit_code}"
                print(f"  {r.wall_time_s:7.3f}s  {r.peak_rss_mb:6.1f}MB  DB:{r.db_size_mb:.1f}MB  {status}")

                # Stop daemon after each cold start run (next run needs fresh start)
                if i < runs:
                    stop_daemon(toki)

            # Phase 2: reports while daemon is running (last cold start's daemon still up)
            for scenario_name, toki_args, _, _ in SCENARIOS:
                for i in range(1, runs + 1):
                    run_idx += 1
                    tag = f"[{run_idx}/{total_runs}]"
                    print(f"  {tag} toki       | {label:8s} | {scenario_name:8s} | run {i}", end="", flush=True)

                    if use_purge:
                        purge_disk_cache()
                    cmd = [toki, "report", "--no-cost"] + toki_args
                    r2 = run_once(cmd)
                    r2.tool = "toki"
                    r2.data_label = label
                    r2.data_size_mb = size_mb
                    r2.scenario = scenario_name
                    r2.run = i
                    results.append(r2)

                    status = "OK" if r2.exit_code == 0 else f"EXIT={r2.exit_code}"
                    print(f"  {r2.wall_time_s:7.3f}s  {r2.peak_rss_mb:6.1f}MB  {status}")

            # Stop daemon
            stop_daemon(toki)

        # ── ccusage: every run re-reads all files ──
        if ccusage:
            for scenario_name, _, cc_args, _ in SCENARIOS:
                for i in range(1, runs + 1):
                    run_idx += 1
                    tag = f"[{run_idx}/{total_runs}]"
                    print(f"  {tag} ccusage    | {label:8s} | {scenario_name:8s} | run {i}", end="", flush=True)

                    if use_purge:
                        purge_disk_cache()
                    cmd = list(ccusage) + ["--offline"] + cc_args
                    env_extra = {"CLAUDE_CONFIG_DIR": str(data_path)}
                    r3 = run_once(cmd, env_extra)
                    r3.tool = "ccusage"
                    r3.data_label = label
                    r3.data_size_mb = size_mb
                    r3.scenario = scenario_name
                    r3.run = i
                    results.append(r3)

                    status = "OK" if r3.exit_code == 0 else f"EXIT={r3.exit_code}"
                    print(f"  {r3.wall_time_s:7.3f}s  {r3.peak_rss_mb:6.1f}MB  {status}")

        # ── zzusage: every run re-reads all files (Zig) ──
        if zzusage:
            for scenario_name, _, _, zz_args in SCENARIOS:
                if not zz_args and scenario_name != "total":
                    continue  # zzusage doesn't support yearly
                for i in range(1, runs + 1):
                    run_idx += 1
                    tag = f"[{run_idx}/{total_runs}]"
                    print(f"  {tag} zzusage    | {label:8s} | {scenario_name:8s} | run {i}", end="", flush=True)

                    if use_purge:
                        purge_disk_cache()
                    cmd = [zzusage, "--data-dir", str(data_path)] + zz_args
                    r4 = run_once(cmd)
                    r4.tool = "zzusage"
                    r4.data_label = label
                    r4.data_size_mb = size_mb
                    r4.scenario = scenario_name
                    r4.run = i
                    results.append(r4)

                    status = "OK" if r4.exit_code == 0 else f"EXIT={r4.exit_code}"
                    print(f"  {r4.wall_time_s:7.3f}s  {r4.peak_rss_mb:6.1f}MB  {status}")

    # Save results
    json_path = save_results(results)
    print_summary(results)

    # Auto-generate charts
    print("\n=== Generating Charts ===")
    charts = generate_charts(json_path)
    if charts:
        print(f"\n{len(charts)} chart files saved to {RESULTS_DIR / 'charts'}/")


def save_results(results: list[BenchResult]) -> Path:
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    tools = sorted(set(r.tool for r in results))
    tool_tag = "_".join(tools)  # e.g. "toki", "ccusage", "ccusage_toki"

    # CSV
    csv_path = RESULTS_DIR / f"benchmark_{tool_tag}_{ts}.csv"
    with open(csv_path, "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=list(asdict(results[0]).keys()))
        writer.writeheader()
        for r in results:
            writer.writerow(asdict(r))

    # JSON (with averages pre-computed)
    json_path = RESULTS_DIR / f"benchmark_{tool_tag}_{ts}.json"
    averages = compute_averages(results)
    with open(json_path, "w") as f:
        json.dump({
            "timestamp": ts,
            "raw": [asdict(r) for r in results],
            "averages": averages,
        }, f, indent=2)

    print(f"\nResults saved:")
    print(f"  CSV:  {csv_path}")
    print(f"  JSON: {json_path}")
    return json_path


def compute_averages(results: list[BenchResult]) -> list[dict]:
    """Group by (tool, data_label, scenario) and average metrics."""
    groups: dict[tuple, list[BenchResult]] = {}
    for r in results:
        key = (r.tool, r.data_label, r.data_size_mb, r.scenario)
        groups.setdefault(key, []).append(r)

    avgs = []
    for (tool, label, size, scenario), runs in sorted(groups.items()):
        n = len(runs)
        entry = {
            "tool": tool,
            "data_label": label,
            "data_size_mb": size,
            "scenario": scenario,
            "runs": n,
            "wall_time_s": round(sum(r.wall_time_s for r in runs) / n, 4),
            "peak_rss_mb": round(max(r.peak_rss_mb for r in runs), 2),
            "avg_rss_mb": round(sum(r.avg_rss_mb for r in runs) / n, 2),
            "peak_cpu_pct": round(max(r.peak_cpu_pct for r in runs), 1),
            "avg_cpu_pct": round(sum(r.avg_cpu_pct for r in runs) / n, 1),
        }
        # Include db_size_mb for cold_start entries
        if scenario == "cold_start":
            entry["db_size_mb"] = round(max(r.db_size_mb for r in runs), 2)
        avgs.append(entry)
    return avgs


def print_summary(results: list[BenchResult]):
    avgs = compute_averages(results)
    tools = sorted(set(a["tool"] for a in avgs))
    all_scenarios = ["cold_start"] + [s[0] for s in SCENARIOS]
    scenarios = [s for s in all_scenarios if any(a["scenario"] == s for a in avgs)]
    labels = sorted(set(a["data_label"] for a in avgs),
                    key=lambda l: int(l.replace("mb", "")))

    print("\n=== Summary (averaged over runs) ===\n")

    # Per-scenario summary
    for scenario in scenarios:
        print(f"--- {scenario} ---")
        header = f"{'Data':>8s}"
        for tool in tools:
            header += f"  {'time(s)':>8s}  {'RSS(MB)':>8s}  {'CPU%':>6s}"
            if scenario == "cold_start" and tool == "toki":
                header += f"  {'DB(MB)':>7s}"
        print(header)

        for label in labels:
            row = f"{label:>8s}"
            for tool in tools:
                match = [a for a in avgs
                         if a["tool"] == tool and a["data_label"] == label
                         and a["scenario"] == scenario]
                if match:
                    a = match[0]
                    row += f"  {a['wall_time_s']:8.3f}  {a['peak_rss_mb']:8.1f}  {a['avg_cpu_pct']:6.1f}"
                    if scenario == "cold_start" and tool == "toki":
                        row += f"  {a.get('db_size_mb', 0):7.1f}"
                else:
                    row += f"  {'—':>8s}  {'—':>8s}  {'—':>6s}"
                    if scenario == "cold_start" and tool == "toki":
                        row += f"  {'—':>7s}"
            print(row)
        print()

    # Speedup comparison (toki report vs ccusage for same scenarios)
    if "toki" in tools and "ccusage" in tools:
        report_scenarios = [s for s in scenarios if s != "cold_start"]
        if report_scenarios:
            print("--- Speedup: toki report (TSDB) vs ccusage (full scan) ---")
            print(f"{'Data':>8s}  {'Scenario':>8s}  {'Speedup':>8s}")
            for label in labels:
                for scenario in report_scenarios:
                    toki_r = [a for a in avgs if a["tool"] == "toki"
                              and a["data_label"] == label and a["scenario"] == scenario]
                    cc_r = [a for a in avgs if a["tool"] == "ccusage"
                            and a["data_label"] == label and a["scenario"] == scenario]
                    if toki_r and cc_r and toki_r[0]["wall_time_s"] > 0:
                        speedup = cc_r[0]["wall_time_s"] / toki_r[0]["wall_time_s"]
                        print(f"{label:>8s}  {scenario:>8s}  {speedup:8.1f}x")
            print()

        # Cold start vs ccusage total (fair 1:1)
        print("--- Cold Start vs ccusage total (both read all files) ---")
        print(f"{'Data':>8s}  {'toki(s)':>8s}  {'ccusage(s)':>10s}  {'Speedup':>8s}")
        for label in labels:
            toki_cs = [a for a in avgs if a["tool"] == "toki"
                       and a["data_label"] == label and a["scenario"] == "cold_start"]
            cc_total = [a for a in avgs if a["tool"] == "ccusage"
                        and a["data_label"] == label and a["scenario"] == "total"]
            if toki_cs and cc_total and toki_cs[0]["wall_time_s"] > 0:
                speedup = cc_total[0]["wall_time_s"] / toki_cs[0]["wall_time_s"]
                print(f"{label:>8s}  {toki_cs[0]['wall_time_s']:8.3f}  {cc_total[0]['wall_time_s']:10.3f}  {speedup:8.1f}x")
        print()


# ---------------------------------------------------------------------------
# Chart generation
# ---------------------------------------------------------------------------

TOOL_COLORS = {
    "toki": "#e74c3c",      # red
    "ccusage": "#3498db",    # blue
}

TOOL_LABELS = {
    "toki": "toki (Rust, TSDB)",
    "ccusage": "ccusage (Node.js)",
}


def generate_charts(json_path: Path) -> list[Path]:
    """Generate benchmark charts from a JSON results file."""
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:
        print("Warning: matplotlib not installed. Skipping chart generation.")
        print("  Install with: pip3 install matplotlib")
        return []

    # ── Modern style ──
    BG = "#ffffff"
    PLOT_BG = "#f8f9fa"
    GRID_COLOR = "#e9ecef"
    TEXT_COLOR = "#212529"
    SUBTLE_TEXT = "#6c757d"

    plt.rcParams.update({
        "font.family": "sans-serif",
        "font.sans-serif": ["Helvetica Neue", "Arial", "DejaVu Sans"],
        "font.size": 11,
        "axes.titlesize": 14,
        "axes.titleweight": 600,
        "axes.labelsize": 11,
        "axes.labelcolor": SUBTLE_TEXT,
        "axes.edgecolor": "#dee2e6",
        "axes.linewidth": 0.6,
        "xtick.color": SUBTLE_TEXT,
        "ytick.color": SUBTLE_TEXT,
        "xtick.labelsize": 10,
        "ytick.labelsize": 10,
        "figure.facecolor": BG,
        "axes.facecolor": PLOT_BG,
        "grid.color": GRID_COLOR,
        "grid.linewidth": 0.7,
        "grid.alpha": 1.0,
        "lines.antialiased": True,
        "patch.antialiased": True,
    })

    with open(json_path) as f:
        data = json.load(f)

    avgs = data["averages"]
    tools = sorted(set(a["tool"] for a in avgs))
    report_scenarios = [s[0] for s in SCENARIOS]
    scenarios = [s for s in report_scenarios if any(a["scenario"] == s for a in avgs)]

    charts_dir = RESULTS_DIR / "charts"
    charts_dir.mkdir(parents=True, exist_ok=True)
    ts = json_path.stem.replace("benchmark_", "")
    saved = []

    has_scipy = True
    try:
        from scipy.interpolate import make_interp_spline
        import numpy as np
    except ImportError:
        has_scipy = False
        import numpy as np

    # ── Line plot helper ──
    STYLES = {
        ("toki", "val"):   {"color": "#e74c3c", "ls": "-",  "marker": "o", "ms": 5, "lw": 2.2, "label": "toki"},
        ("toki", "avg"):   {"color": "#e74c3c", "ls": "-",  "marker": "o", "ms": 5, "lw": 2.2, "label": "toki avg"},
        ("toki", "peak"):  {"color": "#f5a6a0", "ls": "--", "marker": "D", "ms": 4, "lw": 1.6, "label": "toki peak"},
        ("ccusage", "val"):  {"color": "#3498db", "ls": "-",  "marker": "s", "ms": 5, "lw": 2.2, "label": "ccusage"},
        ("ccusage", "avg"):  {"color": "#3498db", "ls": "-",  "marker": "s", "ms": 5, "lw": 2.2, "label": "ccusage avg"},
        ("ccusage", "peak"): {"color": "#85c1e9", "ls": "--", "marker": "^", "ms": 4, "lw": 1.6, "label": "ccusage peak"},
    }

    def _plot(ax, x, y, tool, kind):
        s = STYLES[(tool, kind)]
        if has_scipy and len(x) >= 3:
            x_arr = np.array(x, dtype=float)
            y_arr = np.array(y, dtype=float)
            x_smooth = np.linspace(x_arr.min(), x_arr.max(), 200)
            spl = make_interp_spline(x_arr, y_arr, k=min(3, len(x) - 1))
            y_smooth = spl(x_smooth)
            ax.plot(x_smooth, y_smooth, color=s["color"], linestyle=s["ls"],
                    linewidth=s["lw"], label=s["label"], zorder=2)
            ax.plot(x, y, color=s["color"], marker=s["marker"],
                    markersize=s["ms"], linestyle="none",
                    markeredgecolor="white", markeredgewidth=0.8, zorder=3)
        else:
            ax.plot(x, y, color=s["color"], linestyle=s["ls"], marker=s["marker"],
                    markersize=s["ms"], linewidth=s["lw"], label=s["label"],
                    markeredgecolor="white", markeredgewidth=0.8, zorder=3)

    def _style_ax(ax, ylabel, show_xlabel=True):
        ax.set_ylim(bottom=0)
        ax.set_ylabel(ylabel, fontsize=11, color=SUBTLE_TEXT)
        if show_xlabel:
            ax.set_xlabel("Data Size (MB)", fontsize=11, color=SUBTLE_TEXT)
        ax.grid(axis="y", zorder=0)
        ax.grid(axis="x", visible=False)
        for spine in ["top", "right"]:
            ax.spines[spine].set_visible(False)
        ax.tick_params(axis="both", length=3, width=0.6)

    def _legend(ax, loc="upper right"):
        leg = ax.legend(
            loc=loc, fontsize=9, frameon=True, framealpha=0.92,
            edgecolor="#dee2e6", fancybox=True, borderpad=0.6,
            handlelength=1.8, handletextpad=0.5, labelspacing=0.35,
        )
        leg.get_frame().set_linewidth(0.5)

    # ── Chart 1: Cold start vs ccusage total (fair comparison) ──
    cold_starts = sorted(
        [a for a in avgs if a["tool"] == "toki" and a["scenario"] == "cold_start"],
        key=lambda a: a["data_size_mb"],
    )
    cc_totals = sorted(
        [a for a in avgs if a["tool"] == "ccusage" and a["scenario"] == "total"],
        key=lambda a: a["data_size_mb"],
    )

    if cold_starts or cc_totals:
        fig, axes = plt.subplots(1, 3, figsize=(19, 5.5))
        fig.suptitle("Phase 1: Full File Scan  —  toki cold start vs ccusage report",
                     fontsize=17, fontweight="bold", color=TEXT_COLOR, y=0.98)

        if cold_starts:
            x = [int(a["data_label"].replace("mb", "")) for a in cold_starts]
            _plot(axes[0], x, [a["wall_time_s"] for a in cold_starts], "toki", "val")
            _plot(axes[1], x, [a["avg_cpu_pct"] for a in cold_starts], "toki", "avg")
            _plot(axes[1], x, [a["peak_cpu_pct"] for a in cold_starts], "toki", "peak")
            _plot(axes[2], x, [a["avg_rss_mb"] for a in cold_starts], "toki", "avg")
            _plot(axes[2], x, [a["peak_rss_mb"] for a in cold_starts], "toki", "peak")

        if cc_totals:
            x = [int(a["data_label"].replace("mb", "")) for a in cc_totals]
            _plot(axes[0], x, [a["wall_time_s"] for a in cc_totals], "ccusage", "val")
            _plot(axes[1], x, [a["avg_cpu_pct"] for a in cc_totals], "ccusage", "avg")
            _plot(axes[1], x, [a["peak_cpu_pct"] for a in cc_totals], "ccusage", "peak")
            _plot(axes[2], x, [a["avg_rss_mb"] for a in cc_totals], "ccusage", "avg")
            _plot(axes[2], x, [a["peak_rss_mb"] for a in cc_totals], "ccusage", "peak")

        titles = ["Execution Time", "CPU Usage", "Memory"]
        ylabels = ["Time (s)", "CPU (%)", "Memory (MB)"]
        for i, ax in enumerate(axes):
            ax.set_title(titles[i], fontsize=13, fontweight=600, color=TEXT_COLOR, pad=10)
            _style_ax(ax, ylabels[i])
            _legend(ax, loc="upper left")

        fig.tight_layout(rect=[0, 0, 1, 0.94], w_pad=3.5)
        for fmt in ["png", "svg"]:
            out = charts_dir / f"benchmark_cold_start_{ts}.{fmt}"
            fig.savefig(str(out), dpi=220 if fmt == "png" else 150,
                        bbox_inches="tight", facecolor=BG)
            saved.append(out)
        plt.close(fig)
        print(f"  cold_start: saved PNG + SVG")

    # ── Per-scenario charts: toki report (TSDB) vs ccusage ──
    for scenario in scenarios:
        fig, axes = plt.subplots(1, 3, figsize=(19, 5.5))
        fig.suptitle(f"Phase 2: Report  —  toki (TSDB query) vs ccusage (full scan)  —  {scenario}",
                     fontsize=17, fontweight="bold", color=TEXT_COLOR, y=0.98)

        for tool in tools:
            entries = sorted(
                [a for a in avgs if a["tool"] == tool and a["scenario"] == scenario],
                key=lambda a: a["data_size_mb"],
            )
            if not entries:
                continue
            x = [int(a["data_label"].replace("mb", "")) for a in entries]

            _plot(axes[0], x, [a["wall_time_s"] for a in entries], tool, "val")
            _plot(axes[1], x, [a["avg_cpu_pct"] for a in entries], tool, "avg")
            _plot(axes[1], x, [a["peak_cpu_pct"] for a in entries], tool, "peak")
            _plot(axes[2], x, [a["avg_rss_mb"] for a in entries], tool, "avg")
            _plot(axes[2], x, [a["peak_rss_mb"] for a in entries], tool, "peak")

        titles = ["Execution Time", "CPU Usage", "Memory"]
        ylabels = ["Time (s)", "CPU (%)", "Memory (MB)"]
        for i, ax in enumerate(axes):
            ax.set_title(titles[i], fontsize=13, fontweight=600, color=TEXT_COLOR, pad=10)
            _style_ax(ax, ylabels[i])
            _legend(ax, loc="upper right" if i > 0 else "upper left")

        fig.tight_layout(rect=[0, 0, 1, 0.94], w_pad=3.5)

        for fmt in ["png", "svg"]:
            out = charts_dir / f"benchmark_{scenario}_{ts}.{fmt}"
            fig.savefig(str(out), dpi=220 if fmt == "png" else 150,
                        bbox_inches="tight", facecolor=BG)
            saved.append(out)
        plt.close(fig)
        print(f"  {scenario}: saved PNG + SVG")

    # ── DB size chart ──
    if cold_starts:
        fig, ax = plt.subplots(figsize=(10, 5))
        x = [int(a["data_label"].replace("mb", "")) for a in cold_starts]
        db_sizes = [a.get("db_size_mb", 0) for a in cold_starts]
        ax.bar(x, db_sizes, width=max(1, (max(x) - min(x)) / len(x) * 0.6),
               color="#e74c3c", alpha=0.85)
        for xi, di, si in zip(x, db_sizes, x):
            if di > 0:
                ratio = di / si * 100
                ax.text(xi, di * 1.02, f"{ratio:.0f}%", ha="center", fontsize=9,
                        fontweight="bold", color=SUBTLE_TEXT)
        ax.set_title("toki TSDB Size vs Source Data", fontsize=15,
                     fontweight="bold", color=TEXT_COLOR, pad=12)
        _style_ax(ax, "DB Size (MB)")
        fig.tight_layout()
        for fmt in ["png", "svg"]:
            out = charts_dir / f"benchmark_db_size_{ts}.{fmt}"
            fig.savefig(str(out), dpi=220 if fmt == "png" else 150,
                        bbox_inches="tight", facecolor=BG)
            saved.append(out)
        plt.close(fig)
        print(f"  db_size: saved PNG + SVG")

    # ── Speedup chart ──
    SPEEDUP_COLORS = ["#e74c3c", "#7c3aed", "#059669", "#ea580c", "#d946ef"]
    if "toki" in tools and "ccusage" in tools:
        fig, ax = plt.subplots(figsize=(13, 5.5))
        color_idx = 0
        for scenario in scenarios:
            speedups = []
            x_vals = []
            toki_entries = sorted(
                [a for a in avgs if a["tool"] == "toki" and a["scenario"] == scenario],
                key=lambda a: a["data_size_mb"],
            )
            for entry in toki_entries:
                cc = [a for a in avgs if a["tool"] == "ccusage"
                      and a["data_label"] == entry["data_label"]
                      and a["scenario"] == scenario]
                if cc and entry["wall_time_s"] > 0:
                    speedups.append(cc[0]["wall_time_s"] / entry["wall_time_s"])
                    x_vals.append(int(entry["data_label"].replace("mb", "")))

            if speedups:
                c = SPEEDUP_COLORS[color_idx % len(SPEEDUP_COLORS)]
                if has_scipy and len(x_vals) >= 3:
                    x_arr = np.array(x_vals, dtype=float)
                    y_arr = np.array(speedups, dtype=float)
                    x_sm = np.linspace(x_arr.min(), x_arr.max(), 200)
                    spl = make_interp_spline(x_arr, y_arr, k=min(3, len(x_vals) - 1))
                    ax.plot(x_sm, spl(x_sm), color=c, linewidth=2.2, label=scenario, zorder=2)
                    ax.plot(x_vals, speedups, marker="o", markersize=5, linestyle="none",
                            color=c, markeredgecolor="white", markeredgewidth=0.8, zorder=3)
                else:
                    ax.plot(x_vals, speedups, marker="o", markersize=5, linewidth=2.2,
                            label=scenario, color=c, markeredgecolor="white",
                            markeredgewidth=0.8, zorder=3)
                for xi, si in zip(x_vals, speedups):
                    ax.annotate(f"{si:.0f}x", (xi, si), textcoords="offset points",
                                xytext=(0, 10), ha="center", fontsize=9, color=c, fontweight=500)
                color_idx += 1

        ax.set_title("Speedup: toki report (TSDB) vs ccusage (full scan)  —  higher = faster",
                     fontsize=15, fontweight="bold", color=TEXT_COLOR, pad=12)
        _style_ax(ax, "Speedup (x)")
        _legend(ax, loc="upper right")
        fig.tight_layout()

        for fmt in ["png", "svg"]:
            out = charts_dir / f"benchmark_speedup_{ts}.{fmt}"
            fig.savefig(str(out), dpi=220 if fmt == "png" else 150,
                        bbox_inches="tight", facecolor=BG)
            saved.append(out)
        plt.close(fig)
        print(f"  speedup: saved PNG + SVG")

    return saved


def cmd_plot(args):
    """Generate charts from an existing results JSON file."""
    if args.file:
        json_path = Path(args.file)
    else:
        # Find latest JSON result
        if not RESULTS_DIR.exists():
            print("Error: No results directory found. Run benchmarks first.")
            sys.exit(1)
        jsons = sorted(RESULTS_DIR.glob("benchmark_*.json"))
        if not jsons:
            print("Error: No result JSON files found.")
            sys.exit(1)
        json_path = jsons[-1]

    print(f"\n=== Generating Charts ===")
    print(f"Source: {json_path}")
    charts = generate_charts(json_path)
    if charts:
        print(f"\n{len(charts)} chart files saved to {RESULTS_DIR / 'charts'}/")


# ---------------------------------------------------------------------------
# Utilities
# ---------------------------------------------------------------------------

def parse_sizes(sizes_str: str | None) -> list[int] | None:
    if not sizes_str:
        return None
    return [int(s.strip()) for s in sizes_str.split(",")]


def cmd_all(args):
    cmd_generate(args)
    print()
    cmd_run(args)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="toki vs ccusage benchmark tool",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  python3 benchmark.py generate
  python3 benchmark.py generate --sizes 100,200,500
  python3 benchmark.py run --runs 5
  python3 benchmark.py run --sizes 100,200 --tool toki
  python3 benchmark.py run --tool ccusage
  python3 benchmark.py all
  python3 benchmark.py plot
  python3 benchmark.py plot --file results/benchmark_xxx.json

Architecture:
  toki (daemon/client):
    Phase 1: daemon reset → daemon start → cold start (index all files)
    Phase 2: toki report (TSDB query, near-instant)
  ccusage (stateless):
    Every report re-reads all files from scratch.
        """,
    )
    sub = parser.add_subparsers(dest="command", required=True)

    # generate
    gen = sub.add_parser("generate", help="Generate test data sets")
    gen.add_argument("--source", default="~/.claude",
                     help="Source Claude data directory (default: ~/.claude)")
    gen.add_argument("--sizes", default=None,
                     help="Comma-separated sizes in MB (default: 100,200,...,2000)")
    gen.add_argument("--force", action="store_true",
                     help="Regenerate all data sets even if they exist")

    # run
    run = sub.add_parser("run", help="Run benchmarks")
    run.add_argument("--runs", type=int, default=DEFAULT_RUNS,
                     help=f"Runs per scenario (default: {DEFAULT_RUNS})")
    run.add_argument("--sizes", default=None,
                     help="Comma-separated sizes to benchmark (default: all available)")
    run.add_argument("--tool", choices=["toki", "ccusage", "zzusage", "both", "all"], default="both",
                     help="Which tool(s) to benchmark (default: both)")
    run.add_argument("--purge", action="store_true",
                     help="Purge disk cache before each run (requires sudo -v first)")

    # all
    a = sub.add_parser("all", help="Generate data + run benchmarks")
    a.add_argument("--source", default="~/.claude",
                   help="Source Claude data directory (default: ~/.claude)")
    a.add_argument("--sizes", default=None,
                   help="Comma-separated sizes in MB")
    a.add_argument("--runs", type=int, default=DEFAULT_RUNS,
                   help=f"Runs per scenario (default: {DEFAULT_RUNS})")
    a.add_argument("--tool", choices=["toki", "ccusage", "zzusage", "both", "all"], default="both",
                   help="Which tool(s) to benchmark (default: both)")
    a.add_argument("--force", action="store_true",
                   help="Regenerate all data sets even if they exist")
    a.add_argument("--purge", action="store_true",
                   help="Purge disk cache before each run (requires sudo -v first)")

    # plot
    p = sub.add_parser("plot", help="Generate charts from benchmark results")
    p.add_argument("--file", default=None,
                   help="Path to benchmark JSON file (default: latest)")

    args = parser.parse_args()

    # Apply default sizes for generate/all
    if args.command in ("generate", "all") and not args.sizes:
        args.sizes = ",".join(str(s) for s in DEFAULT_SIZES_MB)

    if args.command == "generate":
        cmd_generate(args)
    elif args.command == "run":
        cmd_run(args)
    elif args.command == "all":
        cmd_all(args)
    elif args.command == "plot":
        cmd_plot(args)


if __name__ == "__main__":
    main()
