use loom_eval_b8::parse_porcelain;

#[test]
fn plain_paths_pass_through() {
    let out = " M src/lib.rs\n?? new.txt\nM  src/lib.rs\n";
    assert_eq!(
        parse_porcelain(out),
        vec!["new.txt".to_owned(), "src/lib.rs".to_owned()]
    );
}

#[test]
fn rename_takes_the_destination() {
    let out = "R  old_name.txt -> new_name.txt\n";
    assert_eq!(parse_porcelain(out), vec!["new_name.txt".to_owned()]);
}

#[test]
fn quoted_paths_are_decoded() {
    // git quotes paths containing `"`, `\`, or control bytes even with
    // core.quotepath=false; octal escapes appear for raw bytes.
    let out = "?? \"a\\tb.txt\"\n M \"quote\\\"d.txt\"\n?? \"caf\\303\\251.txt\"\n";
    assert_eq!(
        parse_porcelain(out),
        vec![
            "a\tb.txt".to_owned(),
            "café.txt".to_owned(),
            "quote\"d.txt".to_owned(),
        ]
    );
}

#[test]
fn quoted_rename_decodes_the_destination() {
    let out = "R  \"old\\tname.txt\" -> \"new\\tname.txt\"\n";
    assert_eq!(parse_porcelain(out), vec!["new\tname.txt".to_owned()]);
}
