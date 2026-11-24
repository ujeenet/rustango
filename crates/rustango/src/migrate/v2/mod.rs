//! Migrations engine v2: the file format, its loader, checksum and state by
//! replay. Nothing here reads a database, the clock, env or the models.
//!
//! Phase 1a: no runner, renderer, ledger or CLI uses this yet.

// Wired to the runner and CLI in later phase 1 branches.
#![cfg_attr(not(test), allow(dead_code))]

pub(crate) mod checksum;
pub(crate) mod format;
pub(crate) mod graph;
pub(crate) mod load;
pub(crate) mod replay;
pub(crate) mod state;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod perf_tests;

/// Why a v2 file or project does not load. Each code is stable: docs and
/// `allow` entries name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum Code {
    /// A replayed op does not match state, or a precondition fails.
    State,
    /// Not a valid v2 file: bad JSON, an unknown or missing key, a bad value.
    File,
    /// Written by a newer release: a higher `format` or op `v`.
    Newer,
    /// A known op this release does not run yet.
    NotEnabled,
    /// The file name or `id` breaks the naming rules.
    Name,
    /// A float or exponent: numbers must be integers.
    NotInteger,
    /// The graph: a missing parent, a duplicate id, a cycle or two leaves.
    Graph,
    /// An engine v1 file in an engine v2 project.
    V1File,
    /// An identifier longer than 63 bytes.
    Identifier,
    /// SQL missing for a dialect the state does not skip.
    Dialect,
    /// NOT NULL on an existing table with no default needs `fill`.
    Fill,
    /// A change v2 does not support (primary key, auto, generated).
    Unsupported,
    /// A type change under an FK (MySQL error 3780 otherwise).
    FkType,
    /// A rename or drop of a column a CHECK names (MySQL error 3959 otherwise).
    CheckColumn,
    /// An FK without an index on its leading columns.
    FkIndex,
    /// A tenant table referencing a registry table.
    CrossTarget,
    /// A checkpoint field misused, or its state hash does not match.
    Checkpoint,
    /// Two app labels in one folder, or one label in two folders.
    AppLabel,
    /// The project is not engine v2, or the `system` folder holds a v2 file.
    Engine,
    /// A folder or file could not be read.
    Io,
}

impl Code {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::State => "MV2-E001",
            Self::File => "MV2-E002",
            Self::Newer => "MV2-E003",
            Self::NotEnabled => "MV2-E004",
            Self::Name => "MV2-E005",
            Self::NotInteger => "MV2-E006",
            Self::Graph => "MV2-E007",
            Self::V1File => "MV2-E008",
            Self::Identifier => "MV2-E009",
            Self::Dialect => "MV2-E010",
            Self::Fill => "MV2-E011",
            Self::FkIndex => "MV2-E012",
            Self::Unsupported => "MV2-E014",
            Self::Checkpoint => "MV2-E015",
            Self::AppLabel => "MV2-E016",
            Self::Engine => "MV2-E017",
            Self::Io => "MV2-E018",
            Self::CrossTarget => "MV2-E030",
            Self::FkType => "MV2-E3780",
            Self::CheckColumn => "MV2-E3959",
        }
    }
}

/// A load or replay error: the code, where (file and JSON pointer), why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {at}: {msg}", code.as_str())]
pub(crate) struct LoadError {
    pub(crate) code: Code,
    pub(crate) at: String,
    pub(crate) msg: String,
}

impl LoadError {
    pub(crate) fn new(code: Code, at: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            code,
            at: at.into(),
            msg: msg.into(),
        }
    }
}

/// The longest identifier every backend takes (PG truncates at 63 bytes).
pub(crate) const MAX_IDENT: usize = 63;
