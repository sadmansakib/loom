use std::path::Path;

use loom_kernel::tool::ToolError;
use schemars::JsonSchema;
use serde::Deserialize;

/// The build system a verification runs for. Variant meanings live in
/// the command table and the `ecosystem` field docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Ecosystem {
    /// Cargo workspace or single crate (`cargo test`).
    Cargo,
    /// Go module (`go test ./...`).
    Go,
    /// Python project (`python -m pytest`).
    Python,
    /// Node project (`npm test`).
    Npm,
    /// No recognized marker, but a `Makefile` (`make test`).
    Make,
}

/// Marker files that identify an ecosystem, in detection-priority order:
/// the first marker present wins, so a mixed workspace stays
/// deterministic. Mirrors the facts scanner's `BUILD_SYSTEMS` names.
const MARKERS: &[(&str, Ecosystem)] = &[
    ("Cargo.toml", Ecosystem::Cargo),
    ("package.json", Ecosystem::Npm),
    ("pyproject.toml", Ecosystem::Python),
    ("setup.py", Ecosystem::Python),
    ("go.mod", Ecosystem::Go),
    ("Makefile", Ecosystem::Make),
    ("makefile", Ecosystem::Make),
];

/// Detects the workspace's ecosystem from root marker files.
#[must_use]
pub fn detect_ecosystem(root: &Path) -> Option<Ecosystem> {
    MARKERS
        .iter()
        .find(|(marker, _)| root.join(marker).is_file())
        .map(|(_, ecosystem)| *ecosystem)
}

impl Ecosystem {
    /// The program and arguments this scope runs. The program always
    /// comes from this table — never from model input.
    pub(crate) fn command(self, scope: Scope) -> Result<(&'static str, Vec<String>), ToolError> {
        fn owned(args: &[&str]) -> Vec<String> {
            args.iter().map(|arg| (*arg).to_owned()).collect()
        }
        match (self, scope) {
            (Self::Cargo, Scope::Check) => {
                Ok(("cargo", owned(&["check", "--message-format=json"])))
            }
            (Self::Cargo, Scope::Test) => Ok(("cargo", owned(&["test", "--message-format=json"]))),
            (Self::Go, Scope::Check) => Ok(("go", owned(&["vet", "./..."]))),
            (Self::Go, Scope::Test) => Ok(("go", owned(&["test", "./..."]))),
            (Self::Python, Scope::Check) => {
                Ok(("python3", owned(&["-m", "compileall", "-q", "."])))
            }
            (Self::Python, Scope::Test) => Ok(("python3", owned(&["-m", "pytest"]))),
            (Self::Npm, Scope::Test) => Ok(("npm", owned(&["test"]))),
            (Self::Make, scope) => Ok(("make", owned(&[scope.label()]))),
            (Self::Npm, Scope::Check) => Err(ToolError::InvalidInput(String::from(
                "npm has no build-agnostic check; use the test scope",
            ))),
        }
    }
}

/// How much verification to run.
///
/// Shared with the mutating tools' fused `verify` argument (action
/// fusion), so one enum owns the vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Scope {
    /// Compile only.
    Check,
    /// Compile and run tests.
    #[default]
    Test,
}

impl Scope {
    /// The scope's wire name, used where an ecosystem takes it as its
    /// own argument (Make targets).
    fn label(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Test => "test",
        }
    }
}
