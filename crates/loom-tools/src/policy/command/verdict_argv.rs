//! Verdict resolution for `CommandPolicy`: program admission and
//! the full-argv deny check, including exec-wrapper inspection.

use super::{CommandPolicy, PolicyVerdict};

/// Programs whose job is (or can be) to execute another program named
/// in their arguments — the wrapper set the deny list must see
/// through (review F-09). Deliberately conservative: missing an
/// exotic wrapper degrades to argv[0]-only semantics, while listing a
/// program that merely *mentions* others (grep, echo, cat) would
/// poison ordinary use.
const EXEC_WRAPPERS: [&str; 13] = [
    "sh", "bash", "zsh", "dash", "ksh", "env", "nice", "nohup", "timeout", "xargs", "sudo",
    "watch", "find",
];

/// Whether `program` is a known exec wrapper (see [`EXEC_WRAPPERS`]).
fn is_exec_wrapper(program: &str) -> bool {
    EXEC_WRAPPERS.contains(&program)
}

impl CommandPolicy {
    /// The verdict for `program`: a rule admits it, the deny list
    /// refuses it, and everything else falls through to the
    /// unknown-program verdict.
    #[must_use]
    pub fn verdict(&self, program: &str) -> PolicyVerdict {
        if self.rules.iter().any(|rule| rule.program == program) {
            PolicyVerdict::Allow
        } else if self.denied.iter().any(|name| name == program) {
            PolicyVerdict::Deny
        } else {
            self.unknown
        }
    }

    /// The full-argv verdict (review F-09): same as [`Self::verdict`],
    /// except that a known exec wrapper whose arguments name a denied
    /// program is denied too — `[[deny]] program = "curl"` must not be
    /// one `sh -c`, `env`, or `xargs` away from meaningless. Wrapper
    /// status is keyed on the program name; a shell `-c` payload is
    /// additionally split into words so `sh -c "curl --version"`
    /// matches. Non-wrapper programs keep argv[0]-only semantics, so
    /// `grep curl file` with a denied `curl` still runs.
    ///
    /// This closes the *trivial* bypass, not all of them: an
    /// obfuscated payload (`"cu\"rl"`, base64 + `python -c`) still
    /// slips a name-scan. The deny list shapes UX and fail-closed
    /// defaults; the OS sandbox remains the boundary — the two are
    /// documented as layers, not substitutes.
    #[must_use]
    pub fn verdict_for_argv<S: AsRef<str>>(&self, program: &str, args: &[S]) -> PolicyVerdict {
        if self.wrapped_denied_name(program, args).is_some() {
            return PolicyVerdict::Deny;
        }
        self.verdict(program)
    }

    /// The denied program named inside a wrapper's arguments, if any:
    /// the token itself for direct wrappers (`env curl …`,
    /// `xargs -n1 curl`, `find . -exec curl`) and the first denied
    /// word of a shell `-c` payload. `None` for non-wrappers, empty
    /// argument matches, or no deny list.
    #[must_use]
    pub fn wrapped_denied_name<'s, S: AsRef<str>>(
        &'s self,
        program: &str,
        args: &[S],
    ) -> Option<&'s str> {
        if self.denied.is_empty() || !is_exec_wrapper(program) {
            return None;
        }
        let direct = args
            .iter()
            .map(<S as AsRef<str>>::as_ref)
            .filter(|arg| !arg.starts_with('-'))
            .find_map(|arg| {
                self.denied
                    .iter()
                    .find(|name| name.as_str() == arg)
                    .map(String::as_str)
            });
        if direct.is_some() {
            return direct;
        }
        // `sh -c "<script>"`: the script follows `-c` as one argument.
        let references: &[&str] = &args
            .iter()
            .map(<S as AsRef<str>>::as_ref)
            .collect::<Vec<_>>();
        let payload = references
            .windows(2)
            .find_map(|pair| (pair[0] == "-c").then_some(pair[1]))?;
        payload.split_whitespace().find_map(|word| {
            let word = word.trim_matches(|c: char| {
                !(c.is_ascii_alphanumeric()
                    || c.is_ascii_punctuation() && matches!(c, '.' | '-' | '_' | '/'))
            });
            self.denied
                .iter()
                .find(|name| name.as_str() == word)
                .map(String::as_str)
        })
    }
}
