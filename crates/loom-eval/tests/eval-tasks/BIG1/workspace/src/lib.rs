//! strung: a single-file toolbox of string utilities.
//!
//! Every function is total — no panics, no unwraps — and documented
//! where the behavior is not obvious from the name.

/// Splits into alphanumeric words, dropping everything between them.
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
}
// ----------------------------------------------------------------------
// 1. Case
// ----------------------------------------------------------------------

/// Capitalizes the first letter of every whitespace-separated word.
pub fn capitalize_words(text: &str) -> String {
    text.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let mut out: String = first.to_uppercase().collect();
                    out.push_str(&chars.as_str().to_lowercase());
                    out
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Uppercases every character.
pub fn shout_case(text: &str) -> String {
    text.to_uppercase()
}

/// Lowercases and collapses every run of non-alphanumerics into one underscore.
pub fn snake_case(text: &str) -> String {
    let mut out = String::new();
    let mut pending = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending && !out.is_empty() {
                out.push('_');
            }
            pending = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending = true;
        }
    }
    out
}

/// Lowercases and collapses every run of non-alphanumerics into one hyphen.
pub fn kebab_case(text: &str) -> String {
    let mut out = String::new();
    let mut pending = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending && !out.is_empty() {
                out.push('-');
            }
            pending = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending = true;
        }
    }
    out
}

/// Joins alphanumeric words: the first lowercased, the rest capitalized.
pub fn camel_case(text: &str) -> String {
    let mut out = String::new();
    for (index, word) in words(text).enumerate() {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            let tail: String = chars.as_str().to_lowercase();
            if index == 0 {
                out.push_str(&first.to_lowercase().to_string());
            } else {
                out.push_str(&first.to_uppercase().to_string());
            }
            out.push_str(&tail);
        }
    }
    out
}

/// Joins alphanumeric words with every word capitalized.
pub fn pascal_case(text: &str) -> String {
    let mut out = String::new();
    for word in words(text) {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.push_str(&first.to_uppercase().to_string());
            out.push_str(&chars.as_str().to_lowercase());
        }
    }
    out
}

/// Flips the case of every alphabetic character.
pub fn swap_case(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_uppercase() {
                ch.to_lowercase().collect::<String>()
            } else {
                ch.to_uppercase().collect::<String>()
            }
        })
        .collect()
}

/// Uppercases the first character and lowercases the rest.
pub fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => {
            let mut out: String = first.to_uppercase().collect();
            out.push_str(&chars.as_str().to_lowercase());
            out
        }
        None => String::new(),
    }
}

/// Lowercases only the first character, leaving the rest untouched.
pub fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => {
            let mut out: String = first.to_lowercase().collect();
            out.push_str(chars.as_str());
            out
        }
        None => String::new(),
    }
}

/// Uppercases only the first character, leaving the rest untouched.
pub fn upper_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => {
            let mut out: String = first.to_uppercase().collect();
            out.push_str(chars.as_str());
            out
        }
        None => String::new(),
    }
}

// ----------------------------------------------------------------------
// 2. Trim & split
// ----------------------------------------------------------------------

/// Trims surrounding whitespace from every line, keeping the line set.
pub fn trim_lines(text: &str) -> String {
    text.lines()
        .map(|line| line.trim())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Splits on whitespace runs, dropping empties.
pub fn split_words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

/// Splits one CSV line on commas and trims each field.
pub fn split_csv(line: &str) -> Vec<&str> {
    line.split(',').map(|field| field.trim()).collect()
}

/// Keeps the first occurrence of every word, preserving order.
pub fn dedup_words(text: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    text.split_whitespace()
        .filter(|word| seen.insert(*word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Prefixes every line with `width` spaces.
pub fn indent_lines(text: &str, width: usize) -> String {
    let pad = " ".repeat(width);
    text.lines()
        .map(|line| format!("{pad}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strips the longest indentation shared by all non-blank lines.
pub fn outdent_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let shared = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| line.get(shared..).unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pads on the left with `fill` until `width` characters.
pub fn pad_left(text: &str, width: usize, fill: char) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_owned();
    }
    let mut out = String::new();
    for _ in 0..(width - len) {
        out.push(fill);
    }
    out.push_str(text);
    out
}

/// Pads on the right with `fill` until `width` characters.
pub fn pad_right(text: &str, width: usize, fill: char) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_owned();
    }
    let mut out = String::from(text);
    for _ in 0..(width - len) {
        out.push(fill);
    }
    out
}

/// Pads both sides with `fill`; the left side gets the smaller half.
pub fn center_in(text: &str, width: usize, fill: char) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_owned();
    }
    let total = width - len;
    let left = total / 2;
    let right = total - left;
    let mut out = String::new();
    for _ in 0..left {
        out.push(fill);
    }
    out.push_str(text);
    for _ in 0..right {
        out.push(fill);
    }
    out
}

/// Greedy word wrap: packs words onto a line while it fits within `width`.
pub fn wrap_words(text: &str, width: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current = word.to_owned();
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_owned();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines.join("\n")
}

// ----------------------------------------------------------------------
// 3. Inspect
// ----------------------------------------------------------------------

/// Counts ASCII vowels in either case.
pub fn count_vowels(text: &str) -> usize {
    text.matches(|ch: char| "aeiouAEIOU".contains(ch)).count()
}

/// Counts ASCII alphabetic characters that are not vowels.
pub fn count_consonants(text: &str) -> usize {
    text.chars()
        .filter(|ch| ch.is_ascii_alphabetic() && !"aeiouAEIOU".contains(*ch))
        .count()
}

/// Counts whitespace-separated words.
pub fn count_words(text: &str) -> usize {
    text.split_whitespace().count()
}

/// Counts lines the way `str::lines` does: a trailing newline ends, not starts, a line.
pub fn count_lines(text: &str) -> usize {
    text.lines().count()
}

/// Returns the longest word; ties keep the earliest.
pub fn longest_word(text: &str) -> String {
    let mut best = "";
    for word in text.split_whitespace() {
        if word.len() > best.len() {
            best = word;
        }
    }
    best.to_owned()
}

/// Returns the shortest word; ties keep the earliest.
pub fn shortest_word(text: &str) -> String {
    let mut best: Option<&str> = None;
    for word in text.split_whitespace() {
        if best.is_none_or(|current| word.len() < current.len()) {
            best = Some(word);
        }
    }
    best.unwrap_or("").to_owned()
}

/// Palindrome check over letters and digits only, case-insensitive.
pub fn is_palindrome(text: &str) -> bool {
    let filtered: Vec<char> = text
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    let mut left = 0;
    let mut right = filtered.len().saturating_sub(1);
    while left < right {
        if filtered[left] != filtered[right] {
            return false;
        }
        left += 1;
        right -= 1;
    }
    true
}

/// True when every ASCII lowercase letter appears at least once.
pub fn is_pangram(text: &str) -> bool {
    let lowered = text.to_lowercase();
    "abcdefghijklmnopqrstuvwxyz"
        .chars()
        .all(|letter| lowered.contains(letter))
}

/// True when any whitespace-separated word occurs twice.
pub fn has_repeated_words(text: &str) -> bool {
    let mut seen = std::collections::HashSet::new();
    text.split_whitespace().any(|word| !seen.insert(word))
}

/// Sums every ASCII digit in the text.
pub fn digit_sum(text: &str) -> u32 {
    text.chars()
        .filter(|ch| ch.is_ascii_digit())
        .map(|ch| (ch as u32) - ('0' as u32))
        .sum()
}

// ----------------------------------------------------------------------
// 4. Transform
// ----------------------------------------------------------------------

/// Reverses word order, not the words.
pub fn reverse_words(text: &str) -> String {
    text.split_whitespace()
        .rev()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reverses the characters of the whole text.
pub fn reverse_chars(text: &str) -> String {
    text.chars().rev().collect()
}

/// Rotates the word list left by `steps`, wrapping around.
pub fn rotate_words(text: &str, steps: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }
    let by = steps % words.len();
    words
        .iter()
        .cycle()
        .skip(by)
        .take(words.len())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Replaces whole whitespace-separated words equal to `from`.
pub fn replace_word(text: &str, from: &str, to: &str) -> String {
    text.split_whitespace()
        .map(|word| if word == from { to } else { word })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Builds a URL slug: lowercase, words separated by single hyphens.
pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut pending = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending && !out.is_empty() {
                out.push('_');
            }
            pending = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending = true;
        }
    }
    out
}

/// Keeps at most `max_words` words; fewer words pass through unchanged.
pub fn truncate_words(text: &str, max_words: usize) -> String {
    text.split_whitespace()
        .take(max_words)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Repeats every word `times` times in place.
pub fn repeat_each_word(text: &str, times: usize) -> String {
    text.split_whitespace()
        .map(|word| word.repeat(times))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Alternates characters from two texts until both are exhausted.
pub fn interleave(left: &str, right: &str) -> String {
    let mut first = left.chars();
    let mut second = right.chars();
    let mut out = String::new();
    loop {
        match (first.next(), second.next()) {
            (None, None) => break,
            (one, two) => {
                if let Some(ch) = one {
                    out.push(ch);
                }
                if let Some(ch) = two {
                    out.push(ch);
                }
            }
        }
    }
    out
}

/// Replaces every ASCII digit with `*`.
pub fn mask_digits(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_ascii_digit() { '*' } else { ch })
        .collect()
}

/// Replaces every ASCII vowel, either case, with `*`.
pub fn mask_vowels(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if "aeiouAEIOU".contains(ch) {
                '*'
            } else {
                ch
            }
        })
        .collect()
}

// ----------------------------------------------------------------------
// 5. Parse & format
// ----------------------------------------------------------------------

/// Parses a trimmed integer, falling back to `default`.
pub fn parse_int_or(text: &str, default: i64) -> i64 {
    text.trim().parse().unwrap_or(default)
}

/// Interprets yes/true/on/1 (any case) as true; everything else is false.
pub fn parse_bool_word(word: &str) -> bool {
    matches!(
        word.trim().to_lowercase().as_str(),
        "true" | "yes" | "on" | "1"
    )
}

/// Reads one `key=value` pair out of a comma-separated list.
pub fn parse_pair<'a>(list: &'a str, key: &str) -> Option<&'a str> {
    list.split(',').find_map(|entry| {
        let (candidate, value) = entry.split_once('=')?;
        (candidate.trim() == key).then(|| value.trim())
    })
}

/// Renders a byte count with one decimal and a binary-unit suffix.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Renders whole seconds as `Xh Ym Zs`.
pub fn format_duration(total_seconds: u64) -> String {
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{hours}h {minutes}m {seconds}s")
}

/// Builds the classic Roman numeral for 1..=3999.
pub fn roman_numeral(number: u16) -> String {
    const PAIRS: [(u16, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut remaining = number;
    let mut out = String::new();
    for (value, glyph) in PAIRS {
        while remaining >= value {
            out.push_str(glyph);
            remaining -= value;
        }
    }
    out
}

/// Names a single ASCII digit; other characters yield `None`.
pub fn spell_digit(ch: char) -> Option<&'static str> {
    match ch {
        '0' => Some("zero"),
        '1' => Some("one"),
        '2' => Some("two"),
        '3' => Some("three"),
        '4' => Some("four"),
        '5' => Some("five"),
        '6' => Some("six"),
        '7' => Some("seven"),
        '8' => Some("eight"),
        '9' => Some("nine"),
        _ => None,
    }
}

/// Rewrites real newlines as the two-character escape sequence backslash-n.
pub fn escape_newlines(text: &str) -> String {
    text.replace('\n', "\\n")
}

/// Rewrites backslash-n escape sequences as real newlines.
pub fn unescape_newlines(text: &str) -> String {
    text.replace("\\n", "\n")
}

/// Renders a u32 as eight uppercase hex digits.
pub fn hex_encode(value: u32) -> String {
    format!("{value:08X}")
}

/// Parses hex with an optional 0x prefix.
pub fn parse_hex(text: &str) -> Option<u32> {
    let trimmed = text.trim();
    let digits = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    u32::from_str_radix(digits, 16).ok()
}

/// Suffixes a number: 1st, 2nd, 3rd, 4th, 11th, 112th, ...
pub fn ordinal(number: u32) -> String {
    let suffix = match (number % 100, number % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{number}{suffix}")
}
