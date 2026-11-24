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
    /// Not a JSON object: no engine can be told, so neither counts it.
    /// Each loader reports the parse error itself.
    Unparsable,
}

impl FileEngine {
    /// Classify `raw` by its top-level keys.
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
            Ok(_) => Self::V1,
            Err(_) => Self::Unparsable,
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
    /// Classify the files in `folders`: the project's app folders. The
    /// `system` folder does not decide the engine (see [`Self::of_project`]).
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

    pub(crate) fn of_files<'a>(files: impl IntoIterator<Item = &'a RawFile>) -> Self {
        let (v2, v1): (Vec<&RawFile>, Vec<&RawFile>) = files
            .into_iter()
            .filter(|f| f.engine != FileEngine::Unparsable)
            .partition(|f| f.engine == FileEngine::V2);
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

    /// The project around the migrations dir `dir`, by its app folders. The
    /// `system` folder is the framework's chain: it stays engine v1 until it
    /// moves to v2, so it never decides the engine and a v2 file there is refused.
    ///
    /// # Errors
    /// As [`Self::detect`], or [`MigrateError::SystemChainV2`].
    pub(crate) fn of_project(dir: &Path) -> Result<Self, MigrateError> {
        refuse_v2_in_system(dir)?;
        Self::detect(super::file::project_migration_dirs(dir))
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

/// Refuse an engine v2 file in the project's `system` folder.
///
/// # Errors
/// [`MigrateError::SystemChainV2`], or [`MigrateError::Io`].
pub(crate) fn refuse_v2_in_system(dir: &Path) -> Result<(), MigrateError> {
    let v2: Vec<PathBuf> = read_folder(&super::make::SystemChain::dir_for(dir))?
        .into_iter()
        .filter(|f| f.engine == FileEngine::V2)
        .map(|f| f.path)
        .collect();
    if v2.is_empty() {
        Ok(())
    } else {
        Err(MigrateError::SystemChainV2 { files: v2 })
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
        assert!(err.to_string().contains("use `manage dbmigrate`"), "{err}");

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

    /// A v2 file in `system` is refused: that chain stays engine v1.
    #[test]
    fn a_v2_file_in_system_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        write(&dir, "0001_a.json", V1);
        write(&tmp.path().join("system/migrations"), "0001_s.json", V2);
        let err = ProjectEngine::of_project(&dir).unwrap_err();
        assert!(matches!(err, MigrateError::SystemChainV2 { .. }), "{err}");
        assert!(err.to_string().contains("0001_s.json"), "{err}");
    }

    /// The `system` folder does not decide the engine: v1 there plus v2 apps is V2.
    #[test]
    fn v1_system_folder_does_not_decide_the_engine() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations");
        write(&dir, "0002_b.json", V2);
        write(&tmp.path().join("blog/migrations"), "0001_c.json", V2);
        write(&tmp.path().join("system/migrations"), "0001_a.json", V1);
        let ProjectEngine::V2 { files } = ProjectEngine::of_project(&dir).unwrap() else {
            panic!("not V2");
        };
        assert_eq!(files.len(), 2, "{files:?}");
        // This release still runs v1 projects only.
        let err = require_v1_project(&dir).unwrap_err();
        assert!(matches!(err, MigrateError::EngineV2 { .. }), "{err}");
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
    fn invalid_json_is_unparsable() {
        assert_eq!(FileEngine::of("not json"), FileEngine::Unparsable);
        assert_eq!(FileEngine::of("[1, 2]"), FileEngine::Unparsable);
        // It decides no engine: a v2 folder with a broken file is still V2.
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "0001_a.json", V2);
        write(tmp.path(), "0002_b.json", "{\"engine\": 2,");
        assert!(matches!(
            ProjectEngine::detect([tmp.path()]).unwrap(),
            ProjectEngine::V2 { .. }
        ));
    }
}
