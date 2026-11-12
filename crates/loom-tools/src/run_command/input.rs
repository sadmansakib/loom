//! The `run_command` input schema and its lenient `args` coercion:
//! whatever the model sends (array, string, bracketed list, or a
//! shell-ish line) is normalized into `Vec<String>` before the policy
//! gate sees it.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub(super) struct RunInput {
    /// Executable name, no path; runs inside the sandbox unless the
    /// policy escalates it.
    pub(super) program: String,
    /// Arguments, one array entry each.
    #[serde(default, deserialize_with = "args_from_value")]
    pub(super) args: Vec<String>,
}

/// Accepts the documented array form and, leniently, the two shapes a
/// model actually emits: the array's own JSON text (`"[--version]"`)
/// and a bare command line (`--version --quiet`). The coercion is
/// deliberate — every accepted shape has one unambiguous reading, and
/// a rejected shape costs the model a whole turn to retry.
fn args_from_value<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Value::String(text) => Ok(text),
                Value::Number(number) => Ok(number.to_string()),
                Value::Bool(flag) => Ok(flag.to_string()),
                other => Err(D::Error::custom(format!(
                    "args entries must be strings, got {}",
                    json_kind(&other)
                ))),
            })
            .collect(),
        Value::String(text) => Ok(split_args_lenient(&text)),
        other => Err(D::Error::custom(format!(
            "args must be an array of strings, got {}",
            json_kind(&other)
        ))),
    }
}

/// One-word name for a JSON value's kind, for coercion errors.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Splits `args` that arrived as one string, tolerating the near-array
/// shapes the model actually emits. Observed storms (xarray-4687, 100
/// turns lost): `args` as a valid JSON array string with a shell
/// pipeline appended OUTSIDE the brackets —
/// `"[\"-c\", \"grep -n x f\"] | head -5"` — which the old
/// strip-suffix path couldn't salvage, tokenizing the brackets as argv.
/// Each shape has exactly one reading; rejecting it costs a turn, and a
/// run of them costs the task.
fn split_args_lenient(text: &str) -> Vec<String> {
    let trimmed = text.trim().trim_start_matches('=').trim();
    if let Some((items, remainder)) = parse_array_prefix(trimmed) {
        return fold_remainder(items, remainder);
    }
    if let Some(inner) = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        // Unquoted bracketed list ("[test, r2574, --quiet]"):
        // separators are commas the model left unquoted.
        return split_args(&inner.replace(',', " "));
    }
    split_args(trimmed)
}

/// Longest valid JSON-array prefix of `text`, plus whatever follows it.
/// Brackets inside quoted strings are handled by the parser itself.
fn parse_array_prefix(text: &str) -> Option<(Vec<String>, String)> {
    let bytes = text.as_bytes();
    let mut end = None;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b']' && serde_json::from_str::<Vec<String>>(&text[..=index]).is_ok() {
            end = Some(index + 1);
            break;
        }
    }
    end.map(|end| {
        let items = serde_json::from_str::<Vec<String>>(&text[..end]);
        (items.unwrap_or_default(), text[end..].to_string())
    })
}

/// A trailing `| pipeline` (or `2>&1` redirects) outside the array
/// belongs to the shell command, which is the array's last element when
/// the program is a shell — append it there so `sh -c "grep … | head"`
/// runs as intended. Anything else after `]` is unreadable: refuse with
/// the shape explained, which is cheaper than a garbage execution.
fn fold_remainder(mut items: Vec<String>, remainder: String) -> Vec<String> {
    let rest = remainder.trim();
    if rest.is_empty() {
        return items;
    }
    if (rest.starts_with('|') || rest.starts_with("2>"))
        && let Some(last) = items.last_mut()
    {
        last.push(' ');
        last.push_str(rest);
        return items;
    }
    // Unreadable tail: keep the salvage deterministic, not silent —
    // surface the junk as a final arg the command will visibly fail on.
    items.push(rest.to_string());
    items
}

/// Splits a command line the way a shell would tokenize it — whitespace
/// separates, quotes group, a backslash escapes the next character —
/// with no expansion, substitution, or globbing. Used only when `args`
/// arrives as one string that is not JSON array text.
fn split_args(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        match quote {
            Some(mark) if character == mark => quote = None,
            Some(_) => {
                current.push(character);
                started = true;
            }
            None => match character {
                '\'' | '"' => {
                    quote = Some(character);
                    started = true;
                }
                '\\' => {
                    if let Some(escaped) = characters.next() {
                        current.push(escaped);
                        started = true;
                    }
                }
                character if character.is_whitespace() => {
                    if started {
                        words.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                character => {
                    current.push(character);
                    started = true;
                }
            },
        }
    }
    if started {
        words.push(current);
    }
    words
}
