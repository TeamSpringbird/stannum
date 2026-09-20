#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Render recorded observations; no database access. Requires matplotlib."""
import json
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.ticker import ScalarFormatter

ROOT = Path(__file__).resolve().parent

def read(name):
    return json.loads((ROOT / name).read_text())

count = next(r for r in read("published-trace-results.json") if r["run"] == "tin-count-100k-02")
engines = {r["engine"]: r for r in count["measurements"]}
tin = read("tin-expanded-results.json")["runs"]["visible-s4-m16"]
auto = next(g for g in tin["groups"] if g["name"] == "forced:wiki:0.25:10:r*:{}")
pushdown = next(g for g in tin["groups"] if g["name"] == "forced:wiki:0.25:10:r*:{'tin.debug_force_conjunction_mode': 'Pushdown'}")

plt.rcParams.update({"font.family": "DejaVu Sans", "font.size": 11, "svg.fonttype": "none"})
fig = plt.figure(figsize=(14, 9), facecolor="#f5f7fb")
fig.text(.055, .948, "Stannum performance: the evidence so far", fontsize=25, weight="bold", color="#17243b")
fig.text(.055, .910, "100,000 Wikipedia articles · recorded September 2026 · historical builds, not current-head results", fontsize=12, color="#526078")
fig.text(.055, .850, "A   Same-host count queries", fontsize=16, weight="bold", color="#17243b")
fig.text(.055, .818, "Client p95 latency · two clients · 302 AND + 302 OR forms within a mixed workload", color="#526078")
ax = fig.add_axes([.22, .555, .69, .23], facecolor="#f5f7fb")
colors = {"stannum": "#007f79", "postgres": "#5375b5"}
for engine, offset, label in [("stannum", .15, "Stannum"), ("postgres", -.15, "Postgres GIN (stored vector)")]:
    values = [engines[engine]["families"][f]["p95_ms"] for f in ("conjunction", "disjunction")]
    bars = ax.barh([1 + offset, offset], values, height=.26, color=colors[engine], label=label)
    ax.bar_label(bars, labels=[f" {v:.3f} ms" for v in values], padding=4, color="#17243b", fontsize=11)
ax.set_yticks([1, 0], ["AND counts", "OR counts"])
ax.set_xlim(0, 17.2)
ax.set_xlabel("Client p95 (ms) — lower is better")
ax.legend(loc="upper right", frameon=False, fontsize=10)
fig.text(.055, .479, "Local ARM64 Docker: 4 CPU / 4 GiB limit · PG 18.6 · run tin-count-100k-02", fontsize=10, color="#526078")
fig.text(.055, .455, "604 full counts agreed; unsampled membership is unproven. Phrase forms omitted because 11 full counts differed.", fontsize=10, color="#526078")

fig.text(.055, .397, "B   TIN only: two strategies on the same server", fontsize=16, weight="bold", color="#17243b")
fig.text(.055, .365, "Server median · history OR war · id ≤ 25,000 · top 10 IDs + scores · network excluded", color="#526078")
ax2 = fig.add_axes([.29, .158, .62, .17], facecolor="#f5f7fb")
values = [auto["median_ms"], pushdown["median_ms"]]
labels = ["TIN · automatic", "TIN · forced pushdown"]
for y, v, color in zip([1, 0], values, ["#ab6b2b", "#c99556"]):
    ax2.scatter(v, y, s=115, color=color, zorder=3)
    ax2.annotate(f" {v:.3f} ms", (v, y), xytext=(8, 0), textcoords="offset points", va="center", color="#17243b")
ax2.set_yticks([1, 0], labels)
ax2.set_ylim(-.5, 1.5)
ax2.set_xlim(0, 38)
ax2.set_xticks([0, 10, 20, 30])
ax2.xaxis.set_major_formatter(ScalarFormatter())
ax2.set_xlabel("EXPLAIN execution time (ms) — three observations per strategy")
fig.text(.055, .067, "TIN 1.0.2: PS-160 ARM / EBS, 2 vCPU / 16 GiB. No matched Stannum measurement exists on this server.", fontsize=10, color="#526078")
fig.text(.055, .042, "Server-only timing removes network, not hardware differences. Sources: docs/benchmarks/performance-snapshot.md", fontsize=10, color="#526078")
for a in (ax, ax2):
    a.set_axisbelow(True)
    a.grid(axis="x", alpha=.18)
    for spine in a.spines.values():
        spine.set_visible(False)
    a.tick_params(length=0, pad=8)
for suffix in ("svg", "png"):
    fig.savefig(ROOT / f"performance-snapshot.{suffix}", dpi=180, facecolor=fig.get_facecolor())
svg = ROOT / "performance-snapshot.svg"
svg.write_text("\n".join(line.rstrip() for line in svg.read_text().splitlines()) + "\n")
plt.close(fig)
