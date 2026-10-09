use loom_eval_b12::{schedule, Access, CallAccess};

fn reads(paths: &[&str]) -> CallAccess {
    CallAccess {
        reads: Access::Known(paths.iter().map(|p| (*p).to_owned()).collect()),
        writes: Access::Known(Vec::new()),
    }
}

fn writes(paths: &[&str]) -> CallAccess {
    CallAccess {
        reads: Access::Known(Vec::new()),
        writes: Access::Known(paths.iter().map(|p| (*p).to_owned()).collect()),
    }
}

#[test]
fn disjoint_writes_run_concurrently() {
    let waves = schedule(&[writes(&["a.txt"]), writes(&["b.txt"])]);
    assert_eq!(waves, vec![vec![0, 1]]);
}

#[test]
fn a_write_and_a_disjoint_read_run_concurrently() {
    let waves = schedule(&[writes(&["a.txt"]), reads(&["b.txt"])]);
    assert_eq!(waves, vec![vec![0, 1]]);
}

#[test]
fn an_unrelated_call_rejoins_the_first_wave() {
    // Read x conflicts with the write of x; the write of y conflicts
    // with nothing, so it belongs to the first wave alongside the read.
    let waves = schedule(&[reads(&["x"]), writes(&["x"]), writes(&["y"])]);
    assert_eq!(waves, vec![vec![0, 2], vec![1]]);
}

#[test]
fn write_then_read_of_the_same_path_keeps_model_order() {
    let waves = schedule(&[writes(&["x"]), reads(&["x"])]);
    assert_eq!(waves, vec![vec![0], vec![1]]);
}

#[test]
fn writes_to_the_same_path_keep_model_order() {
    let waves = schedule(&[writes(&["x"]), writes(&["x"])]);
    assert_eq!(waves, vec![vec![0], vec![1]]);
}

#[test]
fn a_read_only_batch_runs_in_one_wave() {
    let waves = schedule(&[reads(&["a"]), reads(&["b"]), reads(&["a"])]);
    assert_eq!(waves, vec![vec![0, 1, 2]]);
}

#[test]
fn a_directory_write_covers_reads_beneath_it() {
    let waves = schedule(&[writes(&["src"]), reads(&["src/lib.rs"])]);
    assert_eq!(waves, vec![vec![0], vec![1]]);
}

#[test]
fn unknown_access_serializes_with_everything() {
    let waves = schedule(&[
        CallAccess {
            reads: Access::Unknown,
            writes: Access::Unknown,
        },
        reads(&["a"]),
    ]);
    assert_eq!(waves, vec![vec![0], vec![1]]);
}

#[test]
fn waves_keep_indices_in_model_order() {
    let waves = schedule(&[writes(&["x"]), writes(&["x"]), reads(&["y"])]);
    for wave in &waves {
        assert!(wave.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
