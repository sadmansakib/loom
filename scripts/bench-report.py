#!/usr/bin/env python3
"""Render loom eval suite reports as markdown for docs/benchmarks/.

Reads one or more report-suite-<model>[-arm][-union].json files (as
written by eval_live / eval_suite_union) and prints a markdown summary:
headline, per-tier table, per-model comparison table (when the suite
carries a `models` section — old reports without one render unchanged),
failure categories, economics, per-task rows. When two files share a
model but differ in arm, an A/B delta section closes the report.
Stdlib only; deterministic output.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def load(path: Path) -> dict:
    data = json.loads(path.read_text())
    data["_path"] = str(path)
    return data


def fmt_pct(x: float | None) -> str:
    return "—" if x is None else f"{100.0 * x:.1f}%"


def fmt_int(x) -> str:
    return "—" if x is None else str(x)


def render_one(r: dict) -> list[str]:
    out: list[str] = []
    name = Path(r["_path"]).name
    out.append(f"### `{name}`")
    out.append("")
    out.append(
        f"- model `{r.get('model')}` · arm **{r.get('arm')}** · mode {r.get('mode')}"
    )
    out.append(
        f"- **pass rate: {r.get('gated_solved')}/{r.get('gated_runs')}"
        f" ({fmt_pct(r.get('pass_rate'))})**"
    )
    out.append("")

    tiers = r.get("by_tier") or []
    if tiers:
        out.append("| tier | solved/runs | rate |")
        out.append("|---|---|---|")
        for t in tiers:
            out.append(
                f"| {t.get('tier')} | {t.get('solved')}/{t.get('runs')}"
                f" | {fmt_pct(t.get('rate'))} |"
            )
        out.append("")

    # Per-model comparison dimension: present only when gated runs named
    # their model (suites merged across models); absent in old reports.
    models = r.get("models") or {}
    if models:
        out.append("| model | solved/runs | rate |")
        out.append("|---|---|---|")
        for model_id in sorted(models):
            m = models[model_id]
            out.append(
                f"| `{model_id}` | {m.get('solved')}/{m.get('runs')}"
                f" | {fmt_pct(m.get('rate'))} |"
            )
        out.append("")

    cats = r.get("failure_categories") or {}
    if cats:
        out.append("Failure categories:")
        out.append("")
        for label, count in sorted(cats.items(), key=lambda kv: -kv[1]):
            out.append(f"- {label}: {count}")
        out.append("")

    econ = r.get("economics") or {}
    if econ:
        out.append("| economics | value |")
        out.append("|---|---|")
        rows = [
            ("total cost USD", f"${econ.get('total_cost_usd', 0.0):.2f}"),
            ("USD / solved", fmt_pct(None) if econ.get("usd_per_solved") is None else f"${econ['usd_per_solved']:.2f}"),
            ("tokens / solved", fmt_int(econ.get("tokens_per_solved"))),
            (
                "expensive failure ratio",
                fmt_pct(econ.get("expensive_failure_ratio")),
            ),
            (
                "turns / solved (mean/median)",
                f"{fmt_int(econ.get('turns_per_solved_mean'))}/{fmt_int(econ.get('turns_per_solved_median'))}",
            ),
            (
                "idle turns (count/rate)",
                f"{fmt_int(econ.get('idle_turns'))}/{fmt_pct(econ.get('idle_turn_rate'))}",
            ),
            ("cache read ratio", fmt_pct(econ.get("cache_read_ratio"))),
        ]
        tf = econ.get("tool_failure_rate") or {}
        if tf:
            rows.append(
                (
                    "tool failures (failed/total)",
                    f"{fmt_int(tf.get('failed'))}/{fmt_int(tf.get('total'))}"
                    f" ({fmt_pct(tf.get('rate'))})",
                )
            )
        for k, v in rows:
            out.append(f"| {k} | {v} |")
        out.append("")

    tasks = r.get("tasks") or {}
    if tasks:
        out.append("| task | kind | solved/runs | rate |")
        out.append("|---|---|---|---|")
        for task_id in sorted(tasks):
            t = tasks[task_id]
            out.append(
                f"| {task_id} | {t.get('kind')} | {t.get('solved')}/{t.get('runs')}"
                f" | {fmt_pct(t.get('rate'))} |"
            )
        out.append("")
    return out


def render_delta(a: dict, b: dict) -> list[str]:
    out: list[str] = []
    out.append(f"### A/B delta — `{a.get('model')}` full vs bare")
    out.append("")
    out.append(
        f"- pass rate: {fmt_pct(a.get('pass_rate'))} → {fmt_pct(b.get('pass_rate'))}"
        f" (Δ {100.0 * (b.get('pass_rate', 0) - a.get('pass_rate', 0)):+.1f} pts)"
    )
    ta, tb = a.get("tasks") or {}, b.get("tasks") or {}
    flips: list[str] = []
    for task_id in sorted(set(ta) & set(tb)):
        ra = ta[task_id].get("rate")
        rb = tb[task_id].get("rate")
        if ra != rb:
            flips.append(
                f"| {task_id} | {ta[task_id].get('solved')}/{ta[task_id].get('runs')}"
                f" ({fmt_pct(ra)}) | {tb[task_id].get('solved')}/{tb[task_id].get('runs')}"
                f" ({fmt_pct(rb)}) |"
            )
    if flips:
        out.append("")
        out.append("| task | full | bare |")
        out.append("|---|---|---|")
        out.extend(flips)
    out.append("")
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("reports", nargs="+", help="suite report JSON paths")
    args = ap.parse_args()

    docs = [load(Path(p)) for p in args.reports]

    out: list[str] = ["# loom eval suite report", ""]
    for d in docs:
        out.extend(render_one(d))

    full = next((d for d in docs if d.get("arm") == "full"), None)
    bare = next((d for d in docs if d.get("arm") == "bare"), None)
    if full and bare and full.get("model") == bare.get("model"):
        out.extend(render_delta(full, bare))

    sys.stdout.write("\n".join(out) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
