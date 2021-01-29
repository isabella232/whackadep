use anyhow::Result;
use mongodb::bson;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::info;

use crate::db::Db;
use crate::git::Repo;
use crate::rust::RustAnalysis;

#[derive(Serialize, Deserialize)]
pub struct Analysis {
    commit: String,
    rust_dependencies: RustAnalysis,
}

/// The flow:
/// 1. initialize repo if not done
/// 2. git pull to get the latest change
/// 3. run metrics to "extract" information about our dependencies
//    this step only works for rust stuff atm
/// 4. check for updates
/// 5. store it in DB
pub async fn analyze(repo_url: &str, repo_path: &Path) -> Result<()> {
    // 1. initialize repo if not done
    info!("getting diem/diem repo");
    let repo = match Repo::new(repo_path) {
        Ok(repo) => repo,
        Err(_) => {
            info!("cloning {} into {}", repo_url, repo_path.to_string_lossy());
            Repo::clone(repo_url, repo_path).await?
        }
    };

    // 3. pull latest changes on the repo
    info!("pulling latest changes");
    repo.update().await?;

    // 4. get metadata
    let commit = repo.head().await.expect("couldn't get HEAD hash");
    info!("current commit: {}", commit);

    // 5. run analysis for different languages
    // at the moment we only have Rust
    let rust_analysis = RustAnalysis::get_dependencies(&repo.repo_folder).await?;

    // 6. store analysis in db
    info!("analysis done, storing in db...");
    let analysis = Analysis {
        commit: commit,
        rust_dependencies: rust_analysis,
    };

    let analysis = bson::to_bson(&analysis).unwrap();
    let document = analysis.as_document().unwrap();
    Db::write(document.to_owned())
}
