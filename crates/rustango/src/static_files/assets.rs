//! Content-hashed asset names with SRI hashes, and assets embedded in the binary.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Static files compiled into the binary by [`embed_static!`](crate::embed_static).
/// Serve them with [`StaticFiles::embedded`](super::StaticFiles::embedded).
#[derive(Clone, Copy, Debug)]
pub struct EmbeddedDir {
    files: &'static [(&'static str, &'static [u8])],
}

impl EmbeddedDir {
    #[doc(hidden)]
    #[must_use]
    pub const fn __new(files: &'static [(&'static str, &'static [u8])]) -> Self {
        Self { files }
    }

    /// Every file as `(path relative to the embedded dir, bytes)`.
    #[must_use]
    pub fn files(&self) -> &'static [(&'static str, &'static [u8])] {
        self.files
    }

    /// One file's bytes by its relative path, such as `"js/app.js"`.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&'static [u8]> {
        self.files.iter().find(|(p, _)| *p == path).map(|(_, b)| *b)
    }
}

/// One asset in an [`AssetManifest`].
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    /// Relative path with the content hash in the name: `js/app.1a2b3c4d5e6f.js`.
    pub hashed: String,
    /// Subresource Integrity value: `sha384-<base64>`.
    pub integrity: String,
}

/// Content-hashed names and SRI hashes for every file in a static root.
///
/// Mount it with [`StaticFiles::manifest`](super::StaticFiles::manifest) so the
/// hashed names are served (`immutable`), and link to them from templates:
///
/// ```no_run
/// # use rustango::static_files::{static_router, AssetManifest, StaticFiles};
/// # fn app() -> std::io::Result<axum::Router> {
/// let manifest = AssetManifest::from_dir("static")?.prefix("/static");
/// let app = axum::Router::new()
///     .nest("/static", static_router(StaticFiles::new("static").manifest(manifest.clone())));
/// // With Tera (`register_tera`, needs a Tera feature):
/// // manifest.register_tera(&mut tera);
/// // <script src="{{ asset_url(name='app.js') }}"
/// //         integrity="{{ asset_integrity(name='app.js') }}" crossorigin="anonymous"></script>
/// # Ok(app) }
/// ```
///
/// Build it once the files are in place: a file changed after that keeps its
/// old hash, and the browser refuses it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetManifest {
    /// URL prefix the files are mounted at, such as `/static`.
    #[serde(default)]
    prefix: String,
    /// Logical relative path → asset.
    assets: BTreeMap<String, Asset>,
}

impl AssetManifest {
    /// Hash every file under `root`. Names starting with `.` are skipped.
    ///
    /// # Errors
    /// A directory or file that cannot be read.
    pub fn from_dir(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut files = Vec::new();
        collect(root.as_ref(), "", &mut files)?;
        Ok(Self::from_files(
            files.iter().map(|(p, b)| (p.as_str(), b.as_slice())),
        ))
    }

    /// Hash the files of an [`EmbeddedDir`].
    #[must_use]
    pub fn from_embedded(dir: &EmbeddedDir) -> Self {
        Self::from_files(dir.files().iter().copied())
    }

    /// Hash `(relative path, bytes)` pairs.
    #[must_use]
    pub fn from_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Self {
        let assets = files
            .into_iter()
            .map(|(path, bytes)| (path.to_owned(), Asset::of(path, bytes)))
            .collect();
        Self {
            prefix: String::new(),
            assets,
        }
    }

    /// Set the URL prefix the files are mounted at, such as `/static`.
    #[must_use]
    pub fn prefix(mut self, prefix: &str) -> Self {
        self.prefix = prefix.trim_end_matches('/').to_owned();
        self
    }

    /// The asset for a logical path such as `"js/app.js"`.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&Asset> {
        self.assets.get(path.trim_start_matches('/'))
    }

    /// The hashed URL for `path`: `{prefix}/js/app.1a2b3c4d5e6f.js`.
    #[must_use]
    pub fn url(&self, path: &str) -> Option<String> {
        self.get(path)
            .map(|a| format!("{}/{}", self.prefix, a.hashed))
    }

    /// The `integrity` attribute value for `path`.
    #[must_use]
    pub fn integrity(&self, path: &str) -> Option<&str> {
        self.get(path).map(|a| a.integrity.as_str())
    }

    /// Every asset by logical path.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Asset)> {
        self.assets.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Hashed path → logical path, for serving.
    pub(super) fn by_hashed(&self) -> HashMap<String, String> {
        self.assets
            .iter()
            .map(|(k, a)| (a.hashed.clone(), k.clone()))
            .collect()
    }

    /// The manifest as JSON, for a build step or a frontend bundler.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Read a manifest [`Self::to_json`] wrote.
    ///
    /// # Errors
    /// Malformed JSON.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Register `asset_url(name=…)` and `asset_integrity(name=…)` Tera
    /// functions. An unknown name is a render error, not a broken link.
    #[cfg(feature = "_tera")]
    pub fn register_tera(&self, tera: &mut tera::Tera) {
        let urls = std::sync::Arc::new(self.clone());
        let sri = std::sync::Arc::clone(&urls);
        tera.register_function("asset_url", move |args: &HashMap<String, tera::Value>| {
            let name = asset_arg(args)?;
            urls.url(name)
                .map(tera::Value::String)
                .ok_or_else(|| unknown_asset(name))
        });
        tera.register_function(
            "asset_integrity",
            move |args: &HashMap<String, tera::Value>| {
                let name = asset_arg(args)?;
                sri.integrity(name)
                    .map(|s| tera::Value::String(s.to_owned()))
                    .ok_or_else(|| unknown_asset(name))
            },
        );
    }
}

impl Asset {
    fn of(path: &str, bytes: &[u8]) -> Self {
        use base64::Engine as _;
        use sha2::Digest as _;
        let digest = sha2::Sha384::digest(bytes);
        let tag: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
        Self {
            hashed: hashed_name(path, &tag),
            integrity: format!(
                "sha384-{}",
                base64::engine::general_purpose::STANDARD.encode(digest)
            ),
        }
    }
}

/// `js/app.js` + `1a2b` → `js/app.1a2b.js`; `LICENSE` → `LICENSE.1a2b`.
fn hashed_name(path: &str, tag: &str) -> String {
    let (dir, file) = path.rsplit_once('/').map_or(("", path), |(d, f)| (d, f));
    let file = match file.split_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}.{tag}.{ext}"),
        _ => format!("{file}.{tag}"),
    };
    if dir.is_empty() {
        file
    } else {
        format!("{dir}/{file}")
    }
}

fn collect(dir: &Path, rel: &str, out: &mut Vec<(String, Vec<u8>)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') {
            continue;
        }
        let path = if rel.is_empty() {
            name.to_owned()
        } else {
            format!("{rel}/{name}")
        };
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(&entry.path(), &path, out)?;
        } else if kind.is_file() {
            out.push((path, std::fs::read(entry.path())?));
        }
    }
    Ok(())
}

#[cfg(feature = "_tera")]
fn asset_arg(args: &HashMap<String, tera::Value>) -> tera::Result<&str> {
    args.get("name")
        .and_then(tera::Value::as_str)
        .ok_or_else(|| tera::Error::msg("asset_url/asset_integrity need `name=\"…\"`"))
}

#[cfg(feature = "_tera")]
fn unknown_asset(name: &str) -> tera::Error {
    tera::Error::msg(format!("no static asset named `{name}` in the manifest"))
}
