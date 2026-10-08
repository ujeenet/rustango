//! A database of one test's own, dropped when the guard goes out of
//! scope, also when the test panics (#2222).

use rustango::sql::Pool;

/// A per-test database on a PG or MySQL server; `Drop` removes it.
pub struct ScratchDb {
    name: String,
    admin_url: String,
    url: String,
}

#[allow(dead_code)]
impl ScratchDb {
    /// Creates `<prefix>_<pid>_<nanos>` on the server behind `admin_url`.
    pub async fn create(admin_url: &str, prefix: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("{prefix}_{}_{nanos}", std::process::id());
        let admin = Pool::connect(admin_url).await.expect("connect admin");
        let sql = format!("CREATE DATABASE {}", admin.dialect().quote_ident(&name));
        rustango::sql::raw_execute_pool(&admin, &sql, Vec::new())
            .await
            .expect("create database");
        admin.close().await;
        let (base, _) = admin_url.rsplit_once('/').expect("URL names a database");
        Self {
            url: format!("{base}/{name}"),
            name,
            admin_url: admin_url.to_owned(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for ScratchDb {
    fn drop(&mut self) {
        let (admin_url, name) = (self.admin_url.clone(), self.name.clone());
        // Own thread and runtime: this may run inside a test's runtime or its unwind.
        let dropped = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            rt.block_on(async {
                let admin = Pool::connect(&admin_url).await.map_err(|e| e.to_string())?;
                let dialect = admin.dialect();
                let force = if dialect.name() == "postgres" {
                    " WITH (FORCE)"
                } else {
                    ""
                };
                let sql = format!(
                    "DROP DATABASE IF EXISTS {}{force}",
                    dialect.quote_ident(&name)
                );
                let res = rustango::sql::raw_execute_pool(&admin, &sql, Vec::new()).await;
                admin.close().await;
                res.map(drop).map_err(|e| e.to_string())
            })
        })
        .join();
        if let Err(e) = dropped.unwrap_or_else(|_| Err("drop thread panicked".to_owned())) {
            eprintln!("could not drop scratch database {}: {e}", self.name);
        }
    }
}
