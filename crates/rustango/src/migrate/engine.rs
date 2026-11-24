//! Which migrations engine a project's files were written for.
//!
//! This release runs engine v1 only. A file written for engine v2 carries
//! a top-level `engine` or `format` key; v1 files never do. Every v1 entry
//! point refuses a folder or project holding one instead of misreading it.

use std::path::{Path, PathBuf};

use super::error::MigrateError;

/// The top-level keys only an engine v2 file has. `embed_migrations!`
/// keeps its own copy (the macro crate cannot depend on this one); the
/// shared cases in `rustango-macros/engine_marker_cases.json` keep
/// the two in step.
pub(crate) const V2_MARKER_KEYS: [&str; 2] = ["engine", "format"];

/// The engine one migration file was written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileEngine {
    V1,
    V2,
}

impl FileEngine {
    /// Classify `raw` by its top-level keys. A text that is not a JSON
    /// object counts as v1, so the v1 parser reports the real error.
    pub(crate) fn of(raw: &str) -> Self {
        // Top-level keys only: values are skipped, not built.
        type TopLevel = std::collections::BTreeMap<String, Option<serde::de::IgnoredAny>>;
        match serde_json::from_str::<TopLevel>(raw) {
            Ok(keys)
                if V2_MARKER_KEYS
                    .iter()
                    .any(|k| matches!(keys.get(*k), Some(Some(_)))) =>
            {
                Self::V2
            }
            _ => Self::V1,
        }
    }
}

/// One `*.json` file of a migrations folder, read and classified.
pub(crate) struct RawFile {
    pub(crate) path: PathBuf,
    pub(crate) text: String,
    pub(crate) engine: FileEngine,
}

/// Every `*.json` file in `dir`, sorted by name; a missing `dir` is empty.
///
/// # Errors
/// [`MigrateError::Io`] when `dir` or a file cannot be read.
pub(crate) fn read_folder(dir: &Path) -> Result<Vec<RawFile>, MigrateError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let text = crate::text_file::read(&path)?;
            let engine = FileEngine::of(&text);
            Ok(RawFile { path, text, engine })
        })
        .collect()
}

/// The engine a project's migrations folders were written for.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProjectEngine {
    /// No engine v2 file; an empty project included.
    V1,
    /// Engine v2 files only.
    V2 {
        /// The v2 files found.
        files: Vec<PathBuf>,
    },
    /// v1 files next to v2 files, in one folder or across folders.
    Mixed {
        /// The v1 files found.
        v1: Vec<PathBuf>,
        /// The v2 files found.
        v2: Vec<PathBuf>,
    },
}

impl ProjectEngine {
    /// Classify the files in `folders`. Pass every folder the project
    /// loads, the `system` one included: discovery skips it.
    ///
    /// # Errors
    /// [`MigrateError::Io`] when a folder or file cannot be read.
    pub fn detect<I, P>(folders: I) -> Result<Self, MigrateError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut files = Vec::new();
        for folder in folders {
            files.extend(read_folder(folder.as_ref())?);
        }
        Ok(Self::of_files(&files))
    }

    pub(crate) fn of_files(files: &[RawFile]) -> Self {
        let (v2, v1): (Vec<&RawFile>, Vec<&RawFile>) =
            files.iter().partition(|f| f.engine == FileEngine::V2);
        let paths = |fs: Vec<&RawFile>| fs.into_iter().map(|f| f.path.clone()).collect();
        match (v1.is_empty(), v2.is_empty()) {
            (_, true) => Self::V1,
            (true, false) => Self::V2 { files: paths(v2) },
            (false, false) => Self::Mixed {
                v1: paths(v1),
                v2: paths(v2),
            },
        }
    }

    /// The project around the migrations dir `dir`: its app folders and its
    /// `system` folder.
    ///
    /// # Errors
    /// As [`Self::detect`].
    pub(crate) fn of_project(dir: &Path) -> Result<Self, MigrateError> {
        let mut folders = super::file::project_migration_dirs(dir);
        folders.push(super::make::SystemChain::dir_for(dir));
        Self::detect(folders)
    }

    /// Refuse anything but [`Self::V1`]: this release runs engine v1 only.
    ///
    /// # Errors
    /// [`MigrateError::EngineV2`] or [`MigrateError::MixedEngine`].
    pub fn require_v1(self) -> Result<(), MigrateError> {
        match self {
            Self::V1 => Ok(()),
            Self::V2 { files } => Err(MigrateError::EngineV2 {
                origin: folders_of(&files),
            }),
            Self::Mixed { v1, v2 } => Err(MigrateError::MixedEngine { v1, v2 }),
        }
    }
}

/// [`ProjectEngine::of_project`], refused unless v1: the check every
/// project-level v1 entry runs first.
///
/// # Errors
/// As [`ProjectEngine::require_v1`].
pub(crate) fn require_v1_project(dir: &Path) -> Result<(), MigrateError> {
    ProjectEngine::of_project(dir)?.require_v1()
}

/// At boot, a database switched to engine v2 is ahead of this binary: warn
/// and serve without migrating. `true` when `e` is that case.
#[cfg_attr(not(any(feature = "manage", feature = "tenancy")), allow(dead_code))]
pub(crate) fn boot_skips_switched_db(e: &MigrateError) -> bool {
    let MigrateError::SwitchedToV2 { table } = e else {
        return false;
    };
    tracing::warn!(
        target: "rustango::migrate",
        table = %table,
        "{e}; serving without migrating"
    );
    true
}

/// The distinct folders of `files`, for an error message.
fn folders_of(files: &[PathBuf]) -> String {
    let mut dirs: Vec<String> = files
        .iter()
        .map(|f| f.parent().unwrap_or(f).display().to_string())
        .collect();
    dirs.dedup();
    dirs.join(", ")
}

/// `paths` for an error message: the first few, then a count.
pub(crate) fn path_list(paths: &[PathBuf]) -> String {
    const SHOWN: usize = 5;
    let mut out: Vec<String> = paths
        .iter()
        .take(SHOWN)
        .map(|p| p.display().to_string())
        .collect();
    if paths.len() > SHOWN {
        out.push(format!("and {} more", paths.len() - SHOWN));
    }
    out.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cases `embed_migrations!` is tested on too.
    const CASES: &str = include_str!("../../../rustango-macros/engine_marker_cases.json");

    #[test]
    fn file_engine_matches_the_shared_marker_cases() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(CASES).unwrap();
        assert!(cases.len() >= 5, "{cases:?}");
        for case in cases {
            let raw = serde_json::to_string(&case["file"]).unwrap();
            let want = if case["v2"].as_bool().unwrap() {
                FileEngine::V2
            } else {
                FileEngine::V1
            };
            assert_eq!(FileEngine::of(&raw), want, "{}", case["case"]);
        }
    }

    const V1: &str =
        r#"{"name": "0001_a", "created_at": "", "snapshot": {"tables": []}, "forward": []}"#;
    const V2: &str = r#"{"engine": 2, "format": 1, "id": "0002_b", "operations": []}"#;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    /// The per-folder guard: every v1 reader ends in `list_dir`, `load` or `parse`.
    #[test]
    fn list_dir_refuses_a_v2_file() {
        let tmp = tempfile::tempdir().unwrap();
        let only = tmp.path().join("only");
        write(&only, "0002_b.json", V2);
        let err = super::super::file::list_dir(&only).unwrap_err();
        assert!(matches!(err, MigrateError::EngineV2 { .. }), "{err}");
        assert!(err.to_string().contains("only"), "{err}");
        let err = super::super::file::load(&only.join("0002_b.json")).unwrap_err();
        assert!(err.to_string().contains("0002_b.json"), "{err}");
        let err = super::super::file::parse(V2).unwrap_err();
        assert!(matches!(err, MigrateError::EngineV2 { .. }), "{err}");

        let mixed = tmp.path().join("mixed");
        write(&mixed, "0001_a.json", V1);
        write(&mixed, "0002_b.json", V2);
        let err = super::super::file::list_dir(&mixed).unwrap_err();
        assert!(matches!(err, MigrateError::MixedEngine { .. }), "{err}");
        assert!(err.to_string().contains("0001_a.json"), "{err}");

        let v1 = tmp.path().join("v1");
        write(&v1, "0001_a.json", V1);
        assert_eq!(super::super::file::list_dir(&v1).unwrap().len(), 1);
    }

    /// `detect` reads the `system` folder, which discovery skips.
    #[test]
    fn detect_includes_system_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        std::fs::create_dir_all(&dir).unwrap();
        write(&tmp.path().join("system/migrations"), "0001_s.json", V2);
        assert!(matches!(
            ProjectEngine::of_project(&dir).unwrap(),
            ProjectEngine::V2 { .. }
        ));
    }

    /// A v1 `system` folder next to v2 app folders is Mixed, not V2.
    #[test]
    fn v1_system_folder_next_to_v2_app_folders_is_mixed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        write(&dir, "0002_b.json", V2);
        write(&tmp.path().join("blog/migrations"), "0001_c.json", V2);
        write(&tmp.path().join("system/migrations"), "0001_a.json", V1);
        let ProjectEngine::Mixed { v1, v2 } = ProjectEngine::of_project(&dir).unwrap() else {
            panic!("not mixed");
        };
        assert_eq!(v1, [tmp.path().join("system/migrations/0001_a.json")]);
        assert_eq!(v2.len(), 2, "{v2:?}");
        let err = require_v1_project(&dir).unwrap_err();
        assert!(matches!(err, MigrateError::MixedEngine { .. }), "{err}");
        // The v1 folder alone passes the per-folder guard.
        assert!(super::super::file::list_dir(&tmp.path().join("system/migrations")).is_ok());
    }

    #[test]
    fn an_empty_project_is_v1() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            ProjectEngine::of_project(&tmp.path().join("migrations")).unwrap(),
            ProjectEngine::V1
        );
    }

    #[test]
    fn invalid_json_counts_as_v1() {
        assert_eq!(FileEngine::of("not json"), FileEngine::V1);
        assert_eq!(FileEngine::of("[1, 2]"), FileEngine::V1);
    }
}
