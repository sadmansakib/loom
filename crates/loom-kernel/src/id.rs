//! Session and task identifiers.
//!
//! Both identifiers are newtypes around a UUID v7 — a time-ordered UUID
//! variant. v7 is chosen over v4 for two reasons:
//!
//! 1. **Natural sort order.** The most significant bits encode a Unix
//!    millisecond timestamp, so identifier order matches creation order.
//!    This keeps later persistence (SQLite primary keys) and debug output
//!    sorted without a separate `created_at` column comparison.
//! 2. **Index friendliness.** Sequential-ish keys avoid the B-tree
//!    fragmentation that purely random v4 keys cause in databases.
//!
//! The newtypes prevent accidentally passing a [`TaskId`] where a
//! [`SessionId`] is expected — a mistake the compiler would otherwise
//! silently allow if both were plain `Uuid`s.

use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use uuid::Uuid;

/// Shared v7 check for every construction path ([`FromStr`](std::str::FromStr),
/// [`TryFrom<Uuid>`](TryFrom), and serde via `try_from`).
fn check_v7(uuid: Uuid) -> Result<(), IdParseError> {
    let version = uuid.get_version_num();
    if version == 7 {
        Ok(())
    } else {
        Err(IdParseError::Version(version))
    }
}

/// Error produced when parsing an identifier from text.
///
/// Shared by [`SessionId`] and [`TaskId`]: either the text is not a UUID at
/// all, or it is a UUID of the wrong version — these types require v7 so
/// that identifier order equals creation order.
#[derive(Debug, thiserror::Error)]
pub enum IdParseError {
    /// The text is not a UUID in any accepted format.
    #[error("not a valid UUID")]
    Parse(#[from] uuid::Error),
    /// The UUID parsed fine but is not version 7 (found the given version).
    #[error("not a UUID v7 (version {0})")]
    Version(usize),
}

/// Defines an identifier newtype around [`Uuid`].
///
/// Both identifier kinds share the same shape and behaviour; the macro keeps
/// that behaviour defined exactly once:
///
/// - `new()` mints a fresh UUID v7 (time-ordered).
/// - Serde is `transparent`, so the type serializes as a plain UUID string,
///   with no wrapper object around it. Wire and on-disk formats therefore
///   stay identical to a bare `Uuid`.
/// - `Display` renders the canonical hyphenated form (e.g.
///   `0192e4c1-7f8a-7abc-8def-0123456789ab`), which is also what tracing
///   and the CLI print.
/// - Ordering is derived from the UUID itself: because v7 leads with its
///   timestamp, sorting ids minted by `new()` sorts by creation time.
///   Every input path — `FromStr`, `TryFrom<Uuid>`, and `Deserialize` —
///   rejects non-v7 values so the guarantee holds for ids from outside
///   the process too.
macro_rules! define_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        // Deserializes a bare `Uuid` (same wire form as `transparent`) but
        // routes it through `TryFrom<Uuid>`, so the v7 rule is enforced on
        // every input path: text (`FromStr`), JSON (`Deserialize`), and
        // direct construction (`TryFrom`). Only `From<Self> for Uuid`
        // extracts, and extraction cannot weaken the value.
        #[serde(try_from = "Uuid", into = "Uuid")]
        pub struct $name(Uuid);

        impl $name {
            /// Mints a new identifier with a time-ordered UUID v7.
            ///
            /// There is deliberately no `Default` impl: a default that
            /// mints a fresh random id would let `#[serde(default)]` and
            /// `#[derive(Default)]` silently invent identity (breaking
            /// record linkage) instead of failing loudly.
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Returns the wrapped [`Uuid`].
            ///
            /// The inverse of [`From<Self> for Uuid`](From). Persistence
            /// uses this to write ids to TEXT columns.
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl TryFrom<Uuid> for $name {
            type Error = IdParseError;

            /// Validates the v7 rule on construction from a raw [`Uuid`].
            fn try_from(value: Uuid) -> Result<Self, Self::Error> {
                check_v7(value)?;
                Ok(Self(value))
            }
        }

        impl From<$name> for Uuid {
            /// Extracts the wrapped [`Uuid`] (used by serde `into`).
            fn from(value: $name) -> Self {
                value.0
            }
        }

        /// Parses any UUID format [`Uuid::parse_str`] accepts (hyphenated,
        /// simple, braced, urn), rejecting non-v7 versions with
        /// [`IdParseError::Version`] so the ordering guarantee on the type
        /// cannot be broken by outside input. Persistence uses the
        /// hyphenated form written by `Display`.
        impl std::str::FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let uuid = Uuid::parse_str(s)?;
                check_v7(uuid)?;
                Ok(Self(uuid))
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                // Hyphenated is the canonical UUID form and what users expect
                // to see in logs and on the command line.
                write!(f, "{}", self.0.hyphenated())
            }
        }
    };
}

define_id! {
    /// Identifies a `Session`: one conversation transcript plus its task
    /// records. (Linked once the `session` module lands.)
    SessionId
}

define_id! {
    /// Identifies one task run inside a session — a single execution of the
    /// agent loop from user input to a terminal state.
    TaskId
}

define_id! {
    /// Identifies a stored artifact body (`artifacts.id`).
    ///
    /// Content identity is the artifact's sha256 column, not this id: two
    /// identical bodies may be stored under different ids, and a transcript
    /// `ArtifactRef` carries this id as the stable row key. v7 keeps stored
    /// artifacts in creation order, like sessions and tasks.
    ArtifactId
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ids_are_unique() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b, "two freshly minted ids must never collide");
    }

    #[test]
    fn v7_ordering_follows_creation_order() {
        // v7 leads with a millisecond timestamp; the uuid crate additionally
        // guarantees monotonicity within the same millisecond via a counter,
        // so creation order and comparison order agree.
        let earlier = TaskId::new();
        let later = TaskId::new();
        assert!(earlier < later);
    }

    #[test]
    fn serde_is_transparent_uuid_string() {
        let id = SessionId::new();

        // Serializes as a bare JSON string — no wrapper object or tuple.
        let json = serde_json::to_string(&id).unwrap();
        let expected = format!("\"{id}\"");
        assert_eq!(json, expected);

        // Round-trips losslessly.
        let back: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn display_is_hyphenated() {
        let id = TaskId::new();
        let rendered = id.to_string();
        assert_eq!(rendered.len(), 36, "canonical hyphenated UUID length");
        assert_eq!(rendered.matches('-').count(), 4);
    }

    #[test]
    fn from_str_round_trips_display_form() {
        let id = SessionId::new();

        // Display -> FromStr is the exact path persistence uses to write and
        // read TEXT columns, so it must round-trip losslessly.
        let parsed: SessionId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn from_str_rejects_garbage() {
        assert!(matches!(
            "not-a-uuid".parse::<TaskId>(),
            Err(IdParseError::Parse(_))
        ));
        assert!(matches!("".parse::<TaskId>(), Err(IdParseError::Parse(_))));
    }

    #[test]
    fn from_str_rejects_non_v7_versions() {
        // v4: random uuid — parses as a UUID but breaks the ordering
        // contract, so it must be rejected with `Version`. Fixed constant
        // rather than `Uuid::new_v4()` to avoid the extra crate feature.
        let v4 = "936DA01F-9ABD-4D9D-80C7-02AF85C822A8";
        assert!(matches!(
            v4.parse::<SessionId>(),
            Err(IdParseError::Version(4))
        ));

        // nil uuid: version 0, same rule.
        assert!(matches!(
            "00000000-0000-0000-0000-000000000000".parse::<SessionId>(),
            Err(IdParseError::Version(0))
        ));
    }

    #[test]
    fn uuid_conversions_are_inverse() {
        let id = TaskId::new();
        assert_eq!(TaskId::try_from(id.as_uuid()).unwrap(), id);
        assert_eq!(Uuid::from(id), id.as_uuid());
        // new() mints v7; the version survives the conversion round trip.
        assert_eq!(id.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn try_from_rejects_non_v7() {
        // Same rule as FromStr, for direct construction from a raw Uuid.
        assert!(matches!(
            TaskId::try_from(Uuid::nil()),
            Err(IdParseError::Version(0))
        ));
        assert!(SessionId::try_from(Uuid::nil()).is_err());
    }

    #[test]
    fn deserialize_enforces_v7_too() {
        // Serde routes through TryFrom<Uuid>, so a JSON v4 string is
        // rejected just like FromStr — no path accepts a non-v7 id. The
        // typed error is flattened into a serde message, so assert on it.
        let json = "\"936da01f-9abd-4d9d-80c7-02af85c822a8\"";
        let err = serde_json::from_str::<SessionId>(json).unwrap_err();
        assert!(err.to_string().contains("not a UUID v7"), "got: {err}");

        // And a v7 id still round-trips as a bare JSON string.
        let id = SessionId::new();
        let back: SessionId = serde_json::from_str(&serde_json::to_string(&id).unwrap()).unwrap();
        assert_eq!(id, back);
    }
}
