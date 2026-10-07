use loom_eval_b15::{libtest_failures, shape};

const ONE_FAILURE: &str = "\
running 2 tests
test tests::a ... ok
test tests::b ... FAILED

failures:

---- tests::b stdout ----

thread 'tests::b' panicked at src/lib.rs:12:5:
assertion `left == right` failed
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
";

#[test]
fn a_failed_libtest_run_is_shaped_into_evidence() {
    let shaped = shape("cargo test", 101, ONE_FAILURE, "", &["src/lib.rs"]);
    assert_eq!(
        shaped,
        "\
$ cargo test
exit status: 101
--- test failures ---
tests::b: panicked at src/lib.rs:12:5
assertion `left == right` failed
--- recent changes ---
- src/lib.rs
"
    );
}

#[test]
fn multiple_failures_keep_document_order() {
    let stdout = "\
running 2 tests
test tests::a ... FAILED
test tests::b ... FAILED

thread 'tests::a' panicked at src/a.rs:3:5:
assertion failed
thread 'tests::b' panicked at src/b.rs:7:9:
assertion failed

test result: FAILED. 0 passed; 2 failed
";
    let shaped = shape("cargo test", 101, stdout, "", &[]);
    assert_eq!(
        shaped,
        "\
$ cargo test
exit status: 101
--- test failures ---
tests::a: panicked at src/a.rs:3:5
assertion failed
tests::b: panicked at src/b.rs:7:9
assertion failed
"
    );
    let failures = libtest_failures(stdout);
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0].name, "tests::a");
    assert_eq!(failures[1].name, "tests::b");
}

#[test]
fn a_panic_without_a_message_shapes_the_location() {
    let stdout = "\
running 1 test
test tests::a ... FAILED

thread 'tests::a' panicked at src/lib.rs:5:9:
note: run with `RUST_BACKTRACE=1`

test result: FAILED. 0 passed; 1 failed
";
    let shaped = shape("cargo test", 101, stdout, "", &[]);
    assert_eq!(
        shaped,
        "\
$ cargo test
exit status: 101
--- test failures ---
tests::a: panicked at src/lib.rs:5:9
"
    );
}

#[test]
fn a_successful_run_keeps_its_streams() {
    let shaped = shape("cargo test", 0, "running 1 test\ntest a ... ok\n", "warning: unused", &[]);
    assert_eq!(
        shaped,
        "\
$ cargo test
exit status: 0
--- stdout ---
running 1 test
test a ... ok
--- stderr ---
warning: unused
"
    );
}

#[test]
fn an_unparseable_failure_falls_back_to_raw_output() {
    let shaped = shape("./run", 1, "some garbage output", "boom", &[]);
    assert_eq!(
        shaped,
        "\
$ ./run
exit status: 1
--- stdout ---
some garbage output
--- stderr ---
boom
"
    );
}

#[test]
fn empty_streams_are_omitted() {
    let shaped = shape("true", 0, "", "", &[]);
    assert_eq!(shaped, "$ true\nexit status: 0\n");
}
