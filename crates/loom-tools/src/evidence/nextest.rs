//! nextest `JUnit` XML parsing (best effort).

use super::TestFailure;

/// Parses nextest `JUnit` XML into failing tests (best effort).
///
/// Only `<testcase>` blocks containing a `<failure>` element are reported; the
/// `name` attribute and the `message` attribute (or body) supply the fields.
#[must_use]
pub fn nextest_junit(output: &str) -> Vec<TestFailure> {
    let mut failures = Vec::new();
    let mut rest = output;
    while let Some(start) = rest.find("<testcase") {
        let after = &rest[start..];
        let Some(gt) = after.find('>') else { break };
        // A self-closing `<testcase .../>` is a passing case: it carries no
        // body and therefore no `<failure>`.
        if after[..=gt].trim_end().ends_with("/>") {
            rest = &after[gt + 1..];
            continue;
        }
        let Some(end) = after.find("</testcase>") else {
            break;
        };
        let block = &after[..end];
        rest = &after[end + "</testcase>".len()..];
        if !block.contains("<failure") {
            continue;
        }
        let Some(name) = attribute(block, "name") else {
            continue;
        };
        let failure_tag = block.find("<failure").map(|index| &block[index..]);
        let message = failure_tag
            .and_then(|tag| attribute(tag, "message"))
            .or_else(|| failure_tag.and_then(element_body))
            .unwrap_or_default();
        failures.push(TestFailure {
            name,
            message,
            location: None,
        });
    }
    failures
}

/// Reads `attr="value"` from a tag string.
///
/// The attribute name must start at a boundary — the start of the tag or a
/// preceding whitespace, `<`, or `/` — so a request for `name="…"` cannot be
/// satisfied by `classname="…"` (or any other attribute sharing the suffix).
fn attribute(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let mut from = 0;
    while let Some(offset) = tag[from..].find(&needle) {
        let start = from + offset;
        let boundary = start == 0
            || matches!(
                tag.as_bytes()[start - 1],
                b' ' | b'\t' | b'\n' | b'\r' | b'<' | b'/'
            );
        if boundary {
            let rest = &tag[start + needle.len()..];
            let end = rest.find('"')?;
            return Some(rest[..end].to_owned());
        }
        from = start + 1;
    }
    None
}

/// The text between the first `>` and the next `<` in a tag fragment.
fn element_body(tag: &str) -> Option<String> {
    let start = tag.find('>')? + 1;
    let rest = &tag[start..];
    let end = rest.find('<').unwrap_or(rest.len());
    let body = rest[..end].trim();
    (!body.is_empty()).then(|| body.to_owned())
}
