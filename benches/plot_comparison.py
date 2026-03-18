#!/usr/bin/env python3
"""Generate comparison charts: toki vs ccusage vs zzusage.

Produces two figures:
  1. Cold Start — full file scan & index
  2. Report Query — indexed TSDB vs full re-scan

Each figure has 3 subplots: Execution Time, Peak CPU, Peak Memory.
Uses broken y-axis where value ranges differ drastically (time, memory).
"""

import json
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

try:
    from scipy.interpolate import make_interp_spline
    HAS_SCIPY = True
except ImportError:
    HAS_SCIPY = False

RESULTS_DIR = Path(__file__).parent / "results"
TOKI_PATH = RESULTS_DIR / "benchmark_toki_toki_warm_best_20260318_202150.json"
CCUSAGE_PATH = RESULTS_DIR / "benchmark_ccusage_20260316_025207.json"
ZZUSAGE_PATH = RESULTS_DIR / "benchmark_zzusage_20260316_025414.json"

SIZES = [100, 200, 300, 400, 500, 1000, 2000]

# Use evenly spaced positions so 100-500 don't bunch together
X_POS = list(range(len(SIZES)))
X_LABELS = ["100", "200", "300", "400", "500", "1K", "2K"]

# ── Colors ──
C_TOKI      = "#e63946"
C_TOKI_COLD = "#ff8fa3"
C_CCUSAGE   = "#2563eb"
C_ZZUSAGE   = "#7c3aed"

BG = "#ffffff"
PLOT_BG = "#f8f9fb"
GRID_COLOR = "#e5e9f0"
TEXT_COLOR = "#16213e"
SUBTLE = "#7b8794"


def setup_style():
    plt.rcParams.update({
        "font.family": "sans-serif",
        "font.sans-serif": ["Helvetica Neue", "Arial", "DejaVu Sans"],
        "font.size": 11,
        "figure.facecolor": BG,
        "axes.facecolor": PLOT_BG,
        "axes.edgecolor": "#d1d5db",
        "axes.linewidth": 0.6,
        "grid.color": GRID_COLOR,
        "grid.linewidth": 0.5,
        "lines.antialiased": True,
    })

setup_style()


# ── Data loading ──

def load_averages(path):
    with open(path) as f:
        return json.load(f)["averages"]

def get_avg(avgs, label, scenario, tool):
    for a in avgs:
        if a["data_label"] == label and a["scenario"] == scenario and a["tool"] == tool:
            return a
    return None

def avg_across_scenarios(avgs, label, tool, scenarios):
    vals = [get_avg(avgs, label, sc, tool) for sc in scenarios]
    vals = [v for v in vals if v]
    if not vals:
        return None
    n = len(vals)
    return {
        "wall_time_s": sum(v["wall_time_s"] for v in vals) / n,
        "peak_rss_mb": max(v["peak_rss_mb"] for v in vals),
        "peak_cpu_pct": max(v["peak_cpu_pct"] for v in vals),
    }

toki_avgs = load_averages(TOKI_PATH)
cc_avgs = load_averages(CCUSAGE_PATH)
zz_avgs = load_averages(ZZUSAGE_PATH)
report_scenarios = ["total", "daily", "weekly", "monthly", "yearly"]


# ── Plot helpers ──

def smooth_line(ax, xpos, y, color, label, lw=2.5, ls="-", marker="o", ms=6, alpha=1.0, zorder=3):
    xp = np.array(xpos, dtype=float)
    yp = np.array(y, dtype=float)
    if HAS_SCIPY and len(xpos) >= 4:
        xs = np.linspace(xp.min(), xp.max(), 200)
        spl = make_interp_spline(xp, yp, k=min(3, len(xpos)-1))
        ax.plot(xs, spl(xs), color=color, ls=ls, lw=lw, label=label, zorder=zorder, alpha=alpha)
        ax.plot(xpos, y, color=color, marker=marker, ms=ms, ls="none",
                markeredgecolor="white", markeredgewidth=1.2, zorder=zorder+1, alpha=alpha)
    else:
        ax.plot(xpos, y, color=color, ls=ls, lw=lw, marker=marker, ms=ms,
                label=label, markeredgecolor="white", markeredgewidth=1.2, zorder=zorder, alpha=alpha)

def style_ax(ax, ylabel="", title="", show_x=True):
    if title:
        ax.set_title(title, fontsize=13, fontweight="bold", color=TEXT_COLOR, pad=12)
    if ylabel:
        ax.set_ylabel(ylabel, fontsize=10, color=SUBTLE)
    ax.set_ylim(bottom=0)
    ax.grid(axis="y", zorder=0)
    ax.grid(axis="x", visible=False)
    for sp in ["top", "right"]:
        ax.spines[sp].set_visible(False)
    ax.tick_params(length=3, width=0.5, colors=SUBTLE)
    if show_x:
        ax.set_xticks(X_POS)
        ax.set_xticklabels(X_LABELS, fontsize=9)
        ax.set_xlabel("Data Size (MB)", fontsize=10, color=SUBTLE)

def legend(ax, loc="upper left", ncol=1):
    """Fallback box legend (used if inline labels fail)."""
    leg = ax.legend(loc=loc, fontsize=8.5, frameon=True, framealpha=0.95,
                    edgecolor="#d1d5db", fancybox=True, borderpad=0.6,
                    handlelength=2.2, handletextpad=0.5, labelspacing=0.35, ncol=ncol)
    leg.get_frame().set_linewidth(0.4)

def inline_labels(ax, series_list, x_pos):
    """Place labels at end of each line. Pushes apart overlapping labels."""
    try:
        ax.legend().set_visible(False)
    except Exception:
        pass

    items = []
    for xpos, yvals, color, label, *_ in series_list:
        if not label:
            continue
        short = label.split(" (")[0]
        items.append({"x": xpos[-1], "y": yvals[-1], "color": color,
                       "label": short, "is_toki": "toki" in short.lower()})

    items.sort(key=lambda e: e["y"])

    # Push apart in pixel space
    ax.figure.canvas.draw()
    trans = ax.transData
    inv = trans.inverted()

    pxys = []
    for it in items:
        _, py = trans.transform((it["x"], it["y"]))
        pxys.append(py)

    min_gap = 14
    for i in range(1, len(pxys)):
        if pxys[i] - pxys[i-1] < min_gap:
            pxys[i] = pxys[i-1] + min_gap

    for it, py in zip(items, pxys):
        _, data_y = inv.transform((0, py))
        fs = 12 if it["is_toki"] else 8.5
        fw = "bold" if it["is_toki"] else "normal"
        ax.annotate(it["label"],
                    xy=(it["x"], data_y),
                    xytext=(10, 0), textcoords="offset points",
                    fontsize=fs, fontweight=fw, color=it["color"],
                    va="center", ha="left", clip_on=False)


def draw_broken_panel(fig, position, series_list, ylabel, title, low_lim, high_lim, ratio=(1, 2.5)):
    """Draw a broken-axis panel at the given gridspec position.
    series_list: [(xpos, yvals, color, label, lw, ls, ms, alpha), ...]
    """
    import matplotlib.gridspec as mgs
    inner = mgs.GridSpecFromSubplotSpec(2, 1, subplot_spec=position,
                                        height_ratios=[ratio[1], ratio[0]], hspace=0.08)
    ax_hi = fig.add_subplot(inner[0])
    ax_lo = fig.add_subplot(inner[1])

    ax_hi.set_ylim(high_lim)
    ax_lo.set_ylim(low_lim)

    for xpos, yvals, color, label, lw, ls, ms, alpha in series_list:
        smooth_line(ax_hi, xpos, yvals, color, label, lw=lw, ls=ls, ms=ms, alpha=alpha)
        smooth_line(ax_lo, xpos, yvals, color, "", lw=lw, ls=ls, ms=ms, alpha=alpha)

    # Style
    ax_hi.set_title(title, fontsize=13, fontweight="bold", color=TEXT_COLOR, pad=12)
    ax_hi.spines["bottom"].set_visible(False)
    ax_hi.tick_params(bottom=False, labelbottom=False, length=3, width=0.5, colors=SUBTLE)
    ax_hi.grid(axis="y", zorder=0); ax_hi.grid(axis="x", visible=False)
    for sp in ["top", "right"]: ax_hi.spines[sp].set_visible(False)

    ax_lo.spines["top"].set_visible(False)
    ax_lo.set_ylabel(ylabel, fontsize=10, color=SUBTLE)
    ax_lo.set_xticks(X_POS)
    ax_lo.set_xticklabels(X_LABELS, fontsize=9)
    ax_lo.set_xlabel("Data Size (MB)", fontsize=10, color=SUBTLE)
    ax_lo.grid(axis="y", zorder=0); ax_lo.grid(axis="x", visible=False)
    for sp in ["right"]: ax_lo.spines[sp].set_visible(False)
    ax_lo.tick_params(length=3, width=0.5, colors=SUBTLE)

    # Break marks
    d = 0.015
    kwargs = dict(color=SUBTLE, clip_on=False, lw=0.9)
    for dx in (-d, 1-d):
        ax_hi.plot((dx, dx+2*d), (-d, +d), transform=ax_hi.transAxes, **kwargs)
        ax_lo.plot((dx, dx+2*d), (1-d, 1+d), transform=ax_lo.transAxes, **kwargs)

    legend(ax_hi)
    return ax_hi, ax_lo


def draw_normal_panel(fig, position, series_list, ylabel, title):
    """Draw a normal (non-broken) panel with inline end-of-line labels."""
    ax = fig.add_subplot(position)
    for xpos, yvals, color, label, lw, ls, ms, alpha in series_list:
        smooth_line(ax, xpos, yvals, color, label, lw=lw, ls=ls, ms=ms, alpha=alpha)
    style_ax(ax, ylabel, title)
    # Add right margin for inline labels
    xlim = ax.get_xlim()
    ax.set_xlim(xlim[0], xlim[1] + (xlim[1] - xlim[0]) * 0.18)
    inline_labels(ax, series_list, X_POS)
    return ax


# ── Collect data ──

def collect_cs():
    out = {}
    for tool, src, scn, scenarios in [
        ("toki", toki_avgs, "cold_start", None),
        ("ccusage", cc_avgs, None, report_scenarios),
        ("zzusage", zz_avgs, None, ["total", "daily", "weekly", "monthly"]),
    ]:
        vals = []
        for s in SIZES:
            label = f"{s}mb"
            if scn:
                v = get_avg(src, label, scn, tool)
            else:
                v = avg_across_scenarios(src, label, tool, scenarios)
            vals.append(v)
        out[tool] = vals
    return out

def collect_rp():
    out = {}
    for key, src, tool, scenarios in [
        ("toki_warm", toki_avgs, "toki_warm", report_scenarios),
        ("toki_cold", toki_avgs, "toki", report_scenarios),
        ("ccusage", cc_avgs, "ccusage", report_scenarios),
        ("zzusage", zz_avgs, "zzusage", ["total", "daily", "weekly", "monthly"]),
    ]:
        vals = []
        for s in SIZES:
            label = f"{s}mb"
            v = avg_across_scenarios(src, label, tool, scenarios) if key != "toki_warm" and key != "toki_cold" else avg_across_scenarios(src, label, tool, scenarios)
            vals.append(v)
        out[key] = vals
    return out

cs = collect_cs()
rp = collect_rp()
chart_data = {"cold_start": {}, "report": {}}


# ── Figure 1: Cold Start ──

def make_series(data_dict, field, tools_spec):
    """Build series list for plotting."""
    series = []
    for key, color, label, lw, ls, ms, alpha in tools_spec:
        vals = data_dict[key]
        y = [v[field] if v else 0 for v in vals]
        series.append((X_POS, y, color, label, lw, ls, ms, alpha))
    return series

cs_tools = [
    ("toki",    C_TOKI,    "toki (Rust)",       3.2, "-", 7, 1.0),
    ("ccusage", C_CCUSAGE, "ccusage (Node.js)", 2.0, "-", 5, 0.85),
    ("zzusage", C_ZZUSAGE, "zzusage (Zig)",     2.0, "-", 5, 0.85),
]

fig = plt.figure(figsize=(21, 7.5))
gs = fig.add_gridspec(1, 3, wspace=0.32)
fig.suptitle("Cold Start — Full File Scan & Index",
             fontsize=19, fontweight="bold", color=TEXT_COLOR, y=0.99)

s_time = make_series(cs, "wall_time_s", cs_tools)
for s in s_time:
    chart_data["cold_start"].setdefault("time", {})[s[3].split(" (")[0]] = dict(zip([f"{sz}mb" for sz in SIZES], s[1]))
draw_normal_panel(fig, gs[0], s_time, "Time (s)", "Execution Time")

# CPU (normal — all in same range, no break needed)
s_cpu = make_series(cs, "peak_cpu_pct", cs_tools)
for s in s_cpu:
    chart_data["cold_start"].setdefault("cpu_peak", {})[s[3].split(" (")[0]] = dict(zip([f"{sz}mb" for sz in SIZES], s[1]))
draw_normal_panel(fig, gs[1], s_cpu, "CPU (%)", "Peak CPU Usage")

s_mem = make_series(cs, "peak_rss_mb", cs_tools)
for s in s_mem:
    chart_data["cold_start"].setdefault("memory_peak", {})[s[3].split(" (")[0]] = dict(zip([f"{sz}mb" for sz in SIZES], s[1]))
draw_normal_panel(fig, gs[2], s_mem, "Memory (MB)", "Peak Memory Usage")

for fmt in ["png", "svg"]:
    fig.savefig(str(RESULTS_DIR / f"chart_cold_start.{fmt}"),
                dpi=250 if fmt == "png" else 150, bbox_inches="tight", facecolor=BG)
plt.close(fig)
print("Saved: chart_cold_start.png / .svg")


# ── Figure 2: Report Query ──

rp_tools = [
    ("toki_warm", C_TOKI,      "toki warm (Rust)",      3.2, "-",  7, 1.0),
    ("toki_cold", C_TOKI_COLD, "toki cold disk (Rust)",  2.2, "--", 5, 0.85),
    ("ccusage",   C_CCUSAGE,   "ccusage (Node.js)",     2.0, "-",  5, 0.85),
    ("zzusage",   C_ZZUSAGE,   "zzusage (Zig)",         2.0, "-",  5, 0.85),
]

fig = plt.figure(figsize=(21, 7.5))
gs = fig.add_gridspec(1, 3, wspace=0.32)
fig.suptitle("Report Query — Indexed TSDB vs Full Re-scan",
             fontsize=19, fontweight="bold", color=TEXT_COLOR, y=0.99)

s_time = make_series(rp, "wall_time_s", rp_tools)
for s in s_time:
    chart_data["report"].setdefault("time", {})[s[3].split(" (")[0]] = dict(zip([f"{sz}mb" for sz in SIZES], s[1]))
draw_normal_panel(fig, gs[0], s_time, "Time (s)", "Execution Time")

# CPU (normal)
s_cpu = make_series(rp, "peak_cpu_pct", rp_tools)
for s in s_cpu:
    chart_data["report"].setdefault("cpu_peak", {})[s[3].split(" (")[0]] = dict(zip([f"{sz}mb" for sz in SIZES], s[1]))
draw_normal_panel(fig, gs[1], s_cpu, "CPU (%)", "Peak CPU Usage")

s_mem = make_series(rp, "peak_rss_mb", rp_tools)
for s in s_mem:
    chart_data["report"].setdefault("memory_peak", {})[s[3].split(" (")[0]] = dict(zip([f"{sz}mb" for sz in SIZES], s[1]))
draw_normal_panel(fig, gs[2], s_mem, "Memory (MB)", "Peak Memory Usage")

for fmt in ["png", "svg"]:
    fig.savefig(str(RESULTS_DIR / f"chart_report.{fmt}"),
                dpi=250 if fmt == "png" else 150, bbox_inches="tight", facecolor=BG)
plt.close(fig)
print("Saved: chart_report.png / .svg")


# ── Export JSON ──

json_path = RESULTS_DIR / "chart_data.json"
with open(json_path, "w") as f:
    json.dump(chart_data, f, indent=2)
print(f"Saved: {json_path}")


