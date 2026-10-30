//! Paired same-task contrast report over existing SWE-bench artifacts
//! (roadmap item 1). Costs zero new runs: it joins two official scorer
//! reports instance-by-instance and, when given the two eval dirs,
//! annotates each pair with turns/token/request deltas from the run
//! event streams.
//!
//! Usage:
//! ```text
//! swebench-contrast --a <report-a.json> --b <report-b.json> \
//!     [--dir-a <eval-dir-a>] [--dir-b <eval-dir-b>] \
//!     [--input-per-m F] [--cache-per-m F] [--output-per-m F]
//! ```
//!
//! Reports are the scorer's JSON objects keyed by instance id. Metrics
//! come from `<dir>/swebench/instances/<id>/run-1/run.json` — run 1
//! only; a resampled instance compares its first attempt on each side.
//! Cost columns appear only when all three per-million prices are
//! given: `(input − cached)·in + cached·cache + output·out`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use loom_eval::swebench::{RunMetrics, run_metrics, scorer_outcomes};

struct Args {
    a: PathBuf,
    b: PathBuf,
    dir_a: Option<PathBuf>,
    dir_b: Option<PathBuf>,
    input_per_m: Option<f64>,
    cache_per_m: Option<f64>,
    output_per_m: Option<f64>,
}

fn usage() -> ! {
    eprintln!(
        "usage: swebench-contrast --a <report-a.json> --b <report-b.json> \
         [--dir-a <eval-dir>] [--dir-b <eval-dir>] \
         [--input-per-m F] [--cache-per-m F] [--output-per-m F]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let argv = std::env::args().skip(1);
    let mut args = Args {
        a: PathBuf::new(),
        b: PathBuf::new(),
        dir_a: None,
        dir_b: None,
        input_per_m: None,
        cache_per_m: None,
        output_per_m: None,
    };
    let mut flag = String::new();
    for arg in argv {
        match arg.as_str() {
            "--a" | "--b" | "--dir-a" | "--dir-b" => flag = arg,
            "--input-per-m" | "--cache-per-m" | "--output-per-m" => flag = arg,
            "-h" | "--help" => usage(),
            _ => {
                let value = arg;
                match flag.as_str() {
                    "--a" => args.a = PathBuf::from(&value),
                    "--b" => args.b = PathBuf::from(&value),
                    "--dir-a" => args.dir_a = Some(PathBuf::from(&value)),
                    "--dir-b" => args.dir_b = Some(PathBuf::from(&value)),
                    "--input-per-m" => {
                        args.input_per_m = value.parse().ok();
                    }
                    "--cache-per-m" => {
                        args.cache_per_m = value.parse().ok();
                    }
                    "--output-per-m" => {
                        args.output_per_m = value.parse().ok();
                    }
                    _ => usage(),
                }
            }
        }
    }
    if args.a.as_os_str().is_empty() || args.b.as_os_str().is_empty() {
        usage();
    }
    args
}

/// Dollar cost of one usage record at per-million prices, when all
/// three are known; `None` keeps the report cost-free by default.
fn cost(metrics: &RunMetrics, args: &Args) -> Option<f64> {
    let (in_p, cache_p, out_p) = (args.input_per_m?, args.cache_per_m?, args.output_per_m?);
    let usage = &metrics.usage;
    let cached = usage.cached_input_tokens.unwrap_or(0);
    let billed_input = usage.input_tokens.saturating_sub(cached) as f64;
    Some(
        (billed_input * in_p + cached as f64 * cache_p + usage.output_tokens as f64 * out_p)
            / 1_000_000.0,
    )
}

fn glyph(resolved: bool) -> &'static str {
    if resolved { "res" } else { "unres" }
}

/// Loads run-1 metrics for one instance from an eval dir, if present.
fn metrics_for(dir: Option<&Path>, id: &str) -> Option<RunMetrics> {
    let path = dir?
        .join("swebench")
        .join("instances")
        .join(id)
        .join("run-1")
        .join("run.json");
    path.is_file().then(|| run_metrics(&path))
}

fn main() {
    let args = parse_args();
    // Honesty rule: one headline, one engine build. The contrast is a
    // build-vs-build claim; if both eval dirs record binary digests
    // and any pair differs, the contrast would attribute harness
    // changes across a build boundary — refuse. Legacy reports (no
    // provenance) warn.
    //
    // Same rule for the model: a scaffold contrast (loom vs mini) is
    // only a scaffold claim when both sides ran the same provider/
    // model pair. Different models across sides conflate scaffold
    // effect with model effect — refuse rather than print a number
    // that reads as a harness delta.
    if let (Some(dir_a), Some(dir_b)) = (args.dir_a.as_deref(), args.dir_b.as_deref()) {
        let sweep_field = |dir: &Path, field: &str| -> Vec<String> {
            std::fs::read_dir(dir.join("swebench"))
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| {
                    entry
                        .path()
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("swebench-sweep-"))
                        && entry.path().extension().is_some_and(|e| e == "json")
                })
                .filter_map(|entry| {
                    std::fs::read_to_string(entry.path())
                        .ok()
                        .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
                        .and_then(|report| {
                            if field == "loom_sha256" {
                                loom_eval::swebench::recorded_loom_digest(&report)
                                    .map(str::to_owned)
                            } else {
                                report
                                    .get(field)
                                    .and_then(|value| value.as_str())
                                    .map(str::to_owned)
                            }
                        })
                })
                .collect()
        };
        let (left, right) = (
            sweep_field(dir_a, "loom_sha256"),
            sweep_field(dir_b, "loom_sha256"),
        );
        if !left.is_empty() && !right.is_empty() {
            for a in &left {
                for b in &right {
                    if a != b {
                        eprintln!(
                            "cannot contrast: sweep reports span two engine builds ({a} vs {b});\n\
                             a headline number must come from one build — re-run the later\n\
                             batch under a single binary before contrasting"
                        );
                        std::process::exit(1);
                    }
                }
            }
        } else if left.is_empty() || right.is_empty() {
            eprintln!(
                "warning: a sweep report predates build provenance; the contrast may span\n\
                 engine builds (one headline, one build — docs/eval.md)"
            );
        }
        // Model parity: both sides must name the same provider/model
        // pair. A side with no recorded pair (or none readable) warns
        // rather than refuses — parity simply cannot be verified.
        let pairs = |dir: &Path| -> Vec<String> {
            let providers = sweep_field(dir, "provider");
            let models = sweep_field(dir, "model");
            providers
                .into_iter()
                .zip(models)
                .map(|(provider, model)| format!("{provider}/{model}"))
                .collect::<Vec<_>>()
        };
        let (pairs_a, pairs_b) = (pairs(dir_a), pairs(dir_b));
        match (
            (pairs_a.first(), pairs_a.len()),
            (pairs_b.first(), pairs_b.len()),
        ) {
            ((Some(a), 1), (Some(b), 1)) => {
                if a != b {
                    eprintln!(
                        "cannot contrast: sides ran different models ({a} vs {b});\n\
                         a scaffold contrast must hold the model fixed — re-run one\n\
                         side on the other's model before contrasting"
                    );
                    std::process::exit(1);
                }
            }
            ((Some(_), n), _) if n > 1 => {
                eprintln!(
                    "warning: eval dir A mixes multiple provider/model pairs; the contrast headline is not single-model"
                )
            }
            (_, (Some(_), n)) if n > 1 => {
                eprintln!(
                    "warning: eval dir B mixes multiple provider/model pairs; the contrast headline is not single-model"
                )
            }
            _ => {
                eprintln!(
                    "warning: provider/model not recorded in both eval dirs; model parity unverified"
                )
            }
        }
    }

    let a = match scorer_outcomes(&args.a) {
        Ok(a) => a,
        Err(error) => {
            eprintln!("cannot read {}: {error}", args.a.display());
            std::process::exit(1);
        }
    };
    let b = match scorer_outcomes(&args.b) {
        Ok(b) => b,
        Err(error) => {
            eprintln!("cannot read {}: {error}", args.b.display());
            std::process::exit(1);
        }
    };
    let report = render(
        &a,
        &b,
        args.dir_a.as_deref(),
        args.dir_b.as_deref(),
        &args,
        args.a.display().to_string(),
        args.b.display().to_string(),
    );
    print!("{report}");
}

/// The whole report as markdown. Pure: every input is data, so the
/// unit test drives it with synthetic outcome vectors.
#[allow(clippy::too_many_arguments)]
fn render(
    a: &[(String, bool)],
    b: &[(String, bool)],
    dir_a: Option<&Path>,
    dir_b: Option<&Path>,
    args: &Args,
    label_a: String,
    label_b: String,
) -> String {
    let map_a: HashMap<&str, bool> = a.iter().map(|(id, r)| (id.as_str(), *r)).collect();
    let map_b: HashMap<&str, bool> = b.iter().map(|(id, r)| (id.as_str(), *r)).collect();
    let mut shared: Vec<&str> = map_a
        .keys()
        .filter(|id| map_b.contains_key(*id))
        .copied()
        .collect();
    shared.sort();

    let priced =
        args.input_per_m.is_some() && args.cache_per_m.is_some() && args.output_per_m.is_some();
    let with_metrics = dir_a.is_some() && dir_b.is_some();

    let mut out = String::new();
    out.push_str("# Paired contrast\n\n");
    out.push_str(&format!("- A: `{label_a}`\n- B: `{label_b}`\n"));
    out.push_str(&format!(
        "- Instances: {} shared, {} A-only, {} B-only\n",
        shared.len(),
        map_a.len() - shared.len(),
        map_b.len() - shared.len()
    ));

    let (mut a_res, mut b_res, mut wins, mut losses, mut ties) = (0u32, 0u32, 0u32, 0u32, 0u32);
    let mut rows = String::new();
    let (mut turns_da, mut in_da, mut out_da, mut req_da, mut cost_da) =
        (0i64, 0i64, 0i64, 0i64, 0.0f64);
    let (mut ma_sum, mut mb_sum) = (RunMetrics::default(), RunMetrics::default());

    for id in &shared {
        let (ra, rb) = (map_a[*id], map_b[*id]);
        a_res += u32::from(ra);
        b_res += u32::from(rb);
        match (ra, rb) {
            (true, false) => wins += 1,
            (false, true) => losses += 1,
            _ => ties += 1,
        }
        let mut cells = vec![id.to_string(), glyph(ra).into(), glyph(rb).into()];
        if with_metrics {
            let ma = metrics_for(dir_a, id);
            let mb = metrics_for(dir_b, id);
            match (&ma, &mb) {
                (Some(ma), Some(mb)) => {
                    let d_turns = mb.turns as i64 - ma.turns as i64;
                    let d_in = mb.usage.input_tokens as i64 - ma.usage.input_tokens as i64;
                    let d_out = mb.usage.output_tokens as i64 - ma.usage.output_tokens as i64;
                    let d_req = mb.usage.model_requests as i64 - ma.usage.model_requests as i64;
                    turns_da += d_turns;
                    in_da += d_in;
                    out_da += d_out;
                    req_da += d_req;
                    ma_sum.turns += ma.turns;
                    ma_sum.usage.merge(ma.usage);
                    mb_sum.turns += mb.turns;
                    mb_sum.usage.merge(mb.usage);
                    cells.push(format!("{d_turns:+}"));
                    cells.push(format!("{d_in:+}"));
                    cells.push(format!("{d_out:+}"));
                    cells.push(format!("{d_req:+}"));
                    if priced {
                        let d_cost = cost(mb, args).unwrap_or(0.0) - cost(ma, args).unwrap_or(0.0);
                        cost_da += d_cost;
                        cells.push(format!("{d_cost:+.4}"));
                    }
                }
                _ => {
                    cells.push(String::new());
                    cells.push(String::new());
                    cells.push(String::new());
                    cells.push(String::new());
                    if priced {
                        cells.push(String::new());
                    }
                }
            }
        }
        rows.push_str("| ");
        rows.push_str(&cells.join(" | "));
        rows.push_str(" |\n");
    }

    out.push_str(&format!(
        "\nResolved: A {a_res}/{}, B {b_res}/{} — A wins {}, losses {}, ties {}\n",
        shared.len(),
        shared.len(),
        wins,
        losses,
        ties
    ));

    if with_metrics {
        let mut head = "| instance | A | B | Δturns | Δin | Δout | Δreq |".to_string();
        let mut sep = "|---|---|---|---|---|---|---|".to_string();
        if priced {
            head.push_str(" Δcost |");
            sep.push_str("---|");
        }
        out.push('\n');
        out.push_str(&head);
        out.push('\n');
        out.push_str(&sep);
        out.push('\n');
        out.push_str(&rows);
        out.push_str(&format!(
            "\nTotals: turns {} → {} (Δ{turns_da:+}), input {} → {} (Δ{in_da:+}), \
             output {} → {} (Δ{out_da:+}), requests {} → {} (Δ{req_da:+})",
            ma_sum.turns,
            mb_sum.turns,
            ma_sum.usage.input_tokens,
            mb_sum.usage.input_tokens,
            ma_sum.usage.output_tokens,
            mb_sum.usage.output_tokens,
            ma_sum.usage.model_requests,
            mb_sum.usage.model_requests,
        ));
        if priced {
            out.push_str(&format!(
                ", cost ${:.4} → ${:.4} (Δ{cost_da:+.4})",
                cost(&ma_sum, args).unwrap_or(0.0),
                cost(&mb_sum, args).unwrap_or(0.0),
            ));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_dirs_args() -> Args {
        Args {
            a: PathBuf::from("a.json"),
            b: PathBuf::from("b.json"),
            dir_a: None,
            dir_b: None,
            input_per_m: None,
            cache_per_m: None,
            output_per_m: None,
        }
    }

    #[test]
    fn contrast_counts_wins_losses_and_ties() {
        let a = vec![
            ("a__a-1".to_owned(), true),
            ("b__b-2".to_owned(), true),
            ("c__c-3".to_owned(), false),
            ("d__d-4".to_owned(), false),
        ];
        let b = vec![
            ("a__a-1".to_owned(), false),
            ("b__b-2".to_owned(), true),
            ("c__c-3".to_owned(), true),
            ("e__e-5".to_owned(), true),
        ];
        let report = render(
            &a,
            &b,
            None,
            None,
            &no_dirs_args(),
            "a.json".into(),
            "b.json".into(),
        );
        // Shared: a-1, b-2, c-3; d-4 is A-only, e-5 is B-only.
        assert!(report.contains("3 shared, 1 A-only, 1 B-only"), "{report}");
        assert!(report.contains("Resolved: A 2/3, B 2/3"), "{report}");
        assert!(report.contains("A wins 1, losses 1, ties 1"), "{report}");
    }
}
