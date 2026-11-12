use super::*;

#[test]
fn empty_evidence_says_so() {
    let evidence = Evidence::default();
    assert!(evidence.is_empty());
    let rendered = evidence.render();
    assert!(rendered.contains("diagnostics: none"));
    assert!(rendered.contains("tests: none failing"));
}

#[test]
fn cargo_json_diagnostics_are_extracted() {
    let output = "\
    Checking app v0.1.0
{\"reason\":\"compiler-message\",\"message\":{\"level\":\"error\",\"message\":\"mismatched types\",\"code\":{\"code\":\"E0308\"},\"spans\":[{\"file_name\":\"src/lib.rs\",\"line_start\":12,\"column_start\":5,\"is_primary\":true}]}}
{\"reason\":\"build-finished\",\"success\":false}
";
    let diagnostics = cargo_diagnostics(output);
    assert_eq!(diagnostics.len(), 1);
    let diagnostic = &diagnostics[0];
    assert_eq!(diagnostic.level, "error");
    assert_eq!(diagnostic.message, "mismatched types");
    assert_eq!(diagnostic.code.as_deref(), Some("E0308"));
    assert_eq!(
        diagnostic.location,
        Some(Location::new("src/lib.rs", 12, 5))
    );
}

#[test]
fn non_error_levels_and_non_json_lines_are_ignored() {
    let output = "\
Compiling app
{\"reason\":\"compiler-message\",\"message\":{\"level\":\"note\",\"message\":\"fyi\",\"spans\":[]}}
not json at all
";
    assert!(cargo_diagnostics(output).is_empty());
}

#[test]
fn libtest_human_failures_are_parsed() {
    let output = "\
running 2 tests
test payment::rejects_negative ... FAILED

failures:

---- payment::rejects_negative stdout ----
thread 'payment::rejects_negative' panicked at src/payment/validator.rs:42:5:
assertion `left == right` failed
  left: 200
 right: 400
note: run with `RUST_BACKTRACE=1` for a backtrace

failures:
    payment::rejects_negative

test result: FAILED. 1 passed; 1 failed
";
    let failures = libtest_failures(output);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].name, "payment::rejects_negative");
    assert_eq!(
        failures[0].location,
        Some(Location::new("src/payment/validator.rs", 42, 5))
    );
    assert!(
        failures[0]
            .message
            .contains("assertion `left == right` failed")
    );
}

#[test]
fn libtest_failure_keeps_the_multi_line_panic_message() {
    let output = "\
---- payment::rejects_negative stdout ----
thread 'payment::rejects_negative' panicked at src/payment/validator.rs:42:5:
assertion `left == right` failed
  left: 200
 right: 400
note: run with `RUST_BACKTRACE=1` for a backtrace

failures:
    payment::rejects_negative

test result: FAILED. 0 passed; 1 failed; 0 ignored
";
    let failures = libtest_failures(output);
    assert_eq!(failures.len(), 1);
    let message = &failures[0].message;
    for expected in [
        "assertion `left == right` failed",
        "left: 200",
        "right: 400",
    ] {
        assert!(message.contains(expected), "{expected} missing: {message}");
    }
    assert!(!message.contains("note:"), "{message}");
    assert!(!message.contains("test result"), "{message}");
}

#[test]
fn libtest_falls_back_to_failed_summary_lines() {
    let output =
        "test a::b ... FAILED\ntest c::d ... ok\ntest result: FAILED. 1 passed; 1 failed\n";
    let failures = libtest_failures(output);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].name, "a::b");
    assert!(failures[0].location.is_none());
}

#[test]
fn nextest_junit_attribute_requires_a_boundary() {
    // `classname` precedes `name`; a substring match would return the
    // classname as the test name.
    let output = r#"<testsuites><testsuite>
    <testcase classname="app::payment" name="payment::rejects_negative" time="0.02">
      <failure message="assertion failed">left: 200, right: 400</failure>
    </testcase>
</testsuite></testsuites>"#;
    let failures = nextest_junit(output);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].name, "payment::rejects_negative");
    assert_eq!(failures[0].message, "assertion failed");
}

#[test]
fn nextest_junit_failures_are_parsed() {
    let output = r#"<?xml version="1.0"?>
<testsuites>
  <testsuite name="app" tests="2">
    <testcase name="payment::ok" classname="app" time="0.01"/>
    <testcase name="payment::rejects_negative" classname="app" time="0.02">
      <failure message="assertion failed">left: 200, right: 400</failure>
    </testcase>
  </testsuite>
</testsuites>"#;
    let failures = nextest_junit(output);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].name, "payment::rejects_negative");
    assert_eq!(failures[0].message, "assertion failed");
}

#[test]
fn linkage_marks_recent_changes() {
    let evidence = Evidence {
        diagnostics: vec![Diagnostic {
            level: "error".to_owned(),
            message: "mismatched types".to_owned(),
            location: Some(Location::new("/abs/ws/src/lib.rs", 12, 5)),
            code: Some("E0308".to_owned()),
        }],
        failures: vec![],
        changed_paths: vec!["src/lib.rs".to_owned()],
    };
    let rendered = evidence.render();
    assert!(
        rendered.contains("relevant change: src/lib.rs"),
        "{rendered}"
    );
}

#[test]
fn render_is_bounded_and_marks_omissions() {
    let evidence = Evidence {
        diagnostics: (0..25)
            .map(|index| Diagnostic {
                level: "error".to_owned(),
                message: format!("problem {index}"),
                location: None,
                code: None,
            })
            .collect(),
        failures: vec![],
        changed_paths: vec![],
    };
    let rendered = evidence.render();
    assert!(
        rendered.contains("... [5 more diagnostics omitted]"),
        "{rendered}"
    );
    assert!(rendered.contains("problem 19"));
    assert!(!rendered.contains("problem 20"));
}

#[test]
fn long_messages_are_clipped() {
    let evidence = Evidence {
        diagnostics: vec![],
        failures: vec![TestFailure {
            name: "t".to_owned(),
            message: "x".repeat(500),
            location: None,
        }],
        changed_paths: vec![],
    };
    let rendered = evidence.render();
    assert!(rendered.contains('…'));
    assert!(rendered.len() < 1000);
}

#[test]
fn paths_match_absolute_and_relative_forms() {
    assert!(paths_match("src/lib.rs", "/ws/src/lib.rs"));
    assert!(paths_match("/ws/src/lib.rs", "src/lib.rs"));
    assert!(paths_match("src/lib.rs", "src/lib.rs"));
    assert!(!paths_match("src/lib.rs", "src/other.rs"));
}

#[test]
fn go_vet_diagnostics_are_parsed() {
    let output = "\
# example.com/m [example.com/m.test]
./main.go:9:2: undefined: z
./pkg/math.go:12: unreachable code
ok  \texample.com/m\t0.5s
some prose line without a location
";
    let diagnostics = go_diagnostics(output);
    assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0],
        Diagnostic {
            level: "error".to_owned(),
            message: "undefined: z".to_owned(),
            location: Some(Location::new("./main.go", 9, 2)),
            code: None,
        }
    );
    // No column: defaults to 1, message may span colons.
    assert_eq!(
        diagnostics[1].location,
        Some(Location::new("./pkg/math.go", 12, 1))
    );
    assert_eq!(diagnostics[1].message, "unreachable code");
}

#[test]
fn go_test_failures_are_parsed_with_locations() {
    let output = "\
--- FAIL: TestSum (0.00s)
    main_test.go:12: sum(2, 2) = 5, want 4
    main_test.go:13: later log line
--- PASS: TestOther (0.00s)
--- FAIL: TestDiv (0.01s)
    div_test.go:4: division by zero
FAIL
FAIL\texample.com/m\t0.012s
";
    let failures = go_failures(output);
    assert_eq!(failures.len(), 2, "{failures:?}");
    assert_eq!(failures[0].name, "TestSum");
    // The first log line supplies location and message; later ones are
    // extra detail, not duplication.
    assert_eq!(
        failures[0].location,
        Some(Location::new("main_test.go", 12, 1))
    );
    assert_eq!(failures[0].message, "sum(2, 2) = 5, want 4");
    assert_eq!(failures[1].name, "TestDiv");
    assert_eq!(failures[1].message, "division by zero");
}

#[test]
fn pytest_sections_supply_message_and_location() {
    let output = "\
FAILURES
_______________________ TestMath.test_sum _______________________

    def test_sum():
>       assert sum(2, 2) == 5
E       assert 4 == 5
E       +  where 4 = sum(2, 2)

tests/test_math.py:8: AssertionError
=========================== short test summary info ============================
";
    let failures = pytest_failures(output);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].name, "TestMath.test_sum");
    assert_eq!(failures[0].message, "assert 4 == 5 +  where 4 = sum(2, 2)");
    assert_eq!(
        failures[0].location,
        Some(Location::new("tests/test_math.py", 8, 1))
    );
}

#[test]
fn pytest_falls_back_to_short_summary_lines() {
    let output = "\
=========================== short test summary info ============================
FAILED tests/test_math.py::test_sum - assert 4 == 5
FAILED tests/test_math.py::test_div - ZeroDivisionError: division by zero
============================== 2 failed in 0.01s ===============================
";
    let failures = pytest_failures(output);
    assert_eq!(failures.len(), 2, "{failures:?}");
    assert_eq!(failures[0].name, "tests/test_math.py::test_sum");
    assert_eq!(failures[0].message, "assert 4 == 5");
    assert_eq!(failures[1].message, "ZeroDivisionError: division by zero");
    assert!(failures[0].location.is_none());
}

#[test]
fn compileall_errors_are_parsed() {
    let output = "\
*** Cannot compile src/broken.py ***
  File \"src/broken.py\", line 3
    def f(:
          ^
SyntaxError: invalid syntax
";
    let diagnostics = python_diagnostics(output);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0],
        Diagnostic {
            level: "error".to_owned(),
            message: "invalid syntax".to_owned(),
            location: Some(Location::new("src/broken.py", 3, 1)),
            code: None,
        }
    );
}

#[test]
fn compileall_groups_without_a_position_lose_the_location() {
    // A closing error line with no File anchor: kept messageless rather
    // than guessed at... but the closing line always follows an anchor
    // in real compileall output; here the anchor has a bad line number,
    // so the location is dropped and the message kept.
    let output = "\
*** Cannot compile src/x.py ***
  File \"src/x.py\", line ?
SyntaxError: invalid syntax
";
    let diagnostics = python_diagnostics(output);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0].message, "invalid syntax");
    assert!(diagnostics[0].location.is_none());
}
