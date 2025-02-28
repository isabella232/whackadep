//! This module contains code to analyze Rust dependencies.
//!
//! # Stored structures
//!
//! Note that to remain backward compatible, these structures
//! should only be updated to add field, not remove.
//! (As deserialization of past data wouldn't work anymore.)
//! That being said, we might not store data for very long,
//! so this might not matter...
//!

use anyhow::Result;
use futures::{stream, StreamExt};
use guppy_summaries::{PackageStatus, SummarySource};
use rustsec::{report::WarningInfo, Vulnerability, Warning};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use tracing::{error, info};

//
// Modules
//

pub mod cargoaudit;
pub mod cargoguppy;
pub mod cargotree;
pub mod cratesio;
pub mod diff;
pub mod guppy;

use crate::common::dependabot::{self, UpdateMetadata};
use cargoguppy::CargoGuppy;

//
// Structures
//

/// RustAnalysis contains the result of the analysis of a rust workspace
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct RustAnalysis {
    /// Note that we do not use a map because the same dependency can be seen several times.
    /// This is due to different versions being used or/and being used directly and indirectly (transitively).
    dependencies: Vec<DependencyInfo>,

    /// the result of running cargo-audit
    rustsec: RustSec,

    /// A summary of the changes since last analysis
    change_summary: Option<ChangeSummary>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RustSec {
    vulnerabilities: Vec<Vulnerability>,
    warnings: WarningInfo,
}

/// DependencyInfo contains the information obtained from a dependency.
/// Note that some fields might be filled in different stages (e.g. by the priority engine or the risk engine).
#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub struct DependencyInfo {
    /// The name of the dependency.
    name: String,
    /// The current version of the dependency.
    version: Version,
    /// The repository where the dependency is hosted.
    repo: SummarySource,
    /// Is it a dev-dependency?
    dev: bool,
    /// Is it a direct, or a transitive dependency?
    direct: bool,
    /// An optional update available for the dependency.
    update: Option<Update>,
}

/// Update should contain any interesting information (red flags, etc.) about the changes observed in the new version
#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Clone)]
pub struct Update {
    /// All versions
    // TODO: we're missing dates of creation for stats though..
    versions: Vec<Version>,
    /// changelog and commits between current version and last version available
    update_metadata: UpdateMetadata,
    /// build.rs changed
    build_rs: bool,
}

//
// Analysis function
//

impl RustAnalysis {
    /// The main function that will go over the flow:
    /// fetch -> filter -> updatables -> priority -> risk -> store
    pub async fn get_dependencies(
        repo_dir: &Path,
        previous_analysis: Option<&Self>,
        is_diem: bool,
    ) -> Result<Self> {
        // 1. fetch & filter
        info!("1. fetching dependencies...");
        let mut rust_analysis = Self::fetch(repo_dir, is_diem).await?;

        // 2. updatable
        info!("3. checking for updates...");
        rust_analysis.updatable().await?;

        // 3. priority
        info!("4. priority engine running...");
        rust_analysis.priority(repo_dir).await?;

        // 4. risk
        info!("5. risk engine running...");
        rust_analysis.risk().await?;

        // 5. summary of changes since last analysis
        if let Some(old) = previous_analysis {
            let change_summary = ChangeSummary::new(old, &rust_analysis)?;
            rust_analysis.change_summary = Some(change_summary);
        }

        //
        Ok(rust_analysis)
    }

    /// 1. fetch & filter
    /// - filters out internal workspace packages
    /// - might have the same dependency several times but with different version, or as a dev dependency or not (dev), or imported directly or transitively (direct), or with a different repository (repo)
    /// - we filter out duplicates that have the same dependency/version/dev/direct/repo tuple, which happens when the same dependency is imported in different places with different features (in other words, we don't care about features)
    async fn fetch(repo_dir: &Path, is_diem: bool) -> Result<RustAnalysis> {
        // 1. this will produce a json file containing no dev dependencies
        // (only transitive dependencies used in release)
        info!("parsing Cargo.toml with guppy...");
        let manifest_path = repo_dir.join("Cargo.toml");
        let (no_dev_summary, all_summary) = if is_diem {
            CargoGuppy::fetch(repo_dir).await?
        } else {
            guppy::get_guppy_summaries(&manifest_path)?
        };

        info!("filter result...");
        let mut dependencies = Vec::new();

        // merge target + host (build-time) dependencies
        let all_deps = all_summary
            .target_packages
            .iter()
            .chain(all_summary.host_packages.iter());

        for (summary_id, package_info) in all_deps {
            // ignore workspace/internal packages
            if matches!(
                summary_id.source,
                SummarySource::Workspace { .. } | SummarySource::Path { .. }
            ) {
                continue;
            }
            if matches!(
                package_info.status,
                PackageStatus::Initial | PackageStatus::Workspace
            ) {
                continue;
            }

            // dev
            let dev = !no_dev_summary.host_packages.contains_key(summary_id)
                && !no_dev_summary.target_packages.contains_key(summary_id);

            // direct dependency?
            let direct = matches!(package_info.status, PackageStatus::Direct);

            // insert
            dependencies.push(DependencyInfo {
                name: summary_id.name.clone(),
                version: summary_id.version.clone(),
                repo: summary_id.source.clone(),
                update: None,
                dev,
                direct,
            });
        }

        // sort
        info!("sorting dependencies");
        dependencies.sort_by_cached_key(|d| (d.name.clone(), d.version.clone(), d.dev, d.direct));

        // remove duplicates of tuples (name, version, repo, dev, direct)
        info!("removing duplicates");
        dependencies.dedup();

        //
        Ok(Self {
            dependencies,
            rustsec: RustSec::default(),
            change_summary: None,
        })
    }

    /// 3. Checks for updates in a set of crates
    async fn updatable(&mut self) -> Result<()> {
        // filter out non-crates.io dependencies
        let mut dependencies: Vec<String> = self
            .dependencies
            .iter()
            .filter(|dep| matches!(dep.repo, SummarySource::CratesIo))
            .map(|dep| dep.name.clone())
            .collect();

        // remove duplicates of names (stronger than the dedup in step 2)
        // (assumption: the dependency list is sorted alphabetically)
        dependencies.dedup();

        // fetch versions for each dependency in that list
        let mut iterator = stream::iter(dependencies)
            .map(|dependency| async move {
                // get all versions for that dependency
                (
                    dependency.clone(),
                    cratesio::Crates::get_all_versions(&dependency).await,
                )
            })
            .buffer_unordered(10);

        // extract the result as a hashmap of name -> semver
        let mut dep_to_versions: HashMap<String, Vec<Version>> = HashMap::new();
        while let Some((dependency, crate_)) = iterator.next().await {
            if let Ok(crate_) = crate_ {
                let mut versions: Vec<Version> = crate_
                    .versions
                    .iter()
                    // parse as semver
                    .map(|version| Version::parse(&version.num))
                    .filter_map(Result::ok)
                    // TODO: log the error ^
                    .collect();
                versions.sort();
                dep_to_versions.insert(dependency, versions);
            }
        }

        // update our list of dependencies with that new information
        for dependency in &mut self.dependencies {
            let versions = dep_to_versions.get(dependency.name.as_str());
            if let Some(versions) = versions {
                // get GREAT versions
                // TODO: since the list is sorted, it should be faster to find the matching version and split_at there
                let greater_versions: Vec<Version> = versions
                    .iter()
                    .filter(|&version| version > &dependency.version)
                    .cloned()
                    .collect();

                // any update available?
                if !greater_versions.is_empty() {
                    let update = Update {
                        versions: greater_versions,
                        ..Default::default()
                    };
                    dependency.update = Some(update);
                }
            }
        }

        //
        Ok(())
    }

    /// 4. priority engine
    async fn priority(&mut self, repo_dir: &Path) -> Result<()> {
        // 1. get cargo-audit results
        info!("running cargo-audit");
        let report = cargoaudit::audit(repo_dir).await?;
        self.rustsec.vulnerabilities = report.vulnerabilities.list;
        self.rustsec.warnings = report.warnings;

        // 2. fetch every changelog via dependabot
        if std::env::var("GITHUB_TOKEN").is_err()
            || std::env::var("GITHUB_TOKEN") == Ok("".to_string())
        {
            info!("skipping dependabot run due to GITHUB_TOKEN env var not found");
        } else {
            info!("running dependabot to get changelogs");
            let iterator = stream::iter(&mut self.dependencies)
                .map(|dependency| async move {
                    if let Some(update) = &mut dependency.update {
                        let new_version = match update.versions.last() {
                            Some(version) => version.to_string(),
                            None => {
                                error!(
                                    "couldn't find new version in a dependency update: {:?}",
                                    update
                                );
                                "".to_string()
                            }
                        };
                        let name = dependency.name.clone();
                        let version = dependency.version.to_string();
                        match dependabot::get_update_metadata(
                            "cargo",
                            &name,
                            &version,
                            &new_version,
                        )
                        .await
                        {
                            Ok(update_metadata) => update.update_metadata = update_metadata,
                            Err(e) => {
                                error!("couldn't get changelog for {}: {}", dependency.name, e)
                            }
                        };
                    }
                })
                .buffer_unordered(10);
            iterator.collect::<()>().await;
        }

        //
        Ok(())
    }

    /// 5. risk engine
    async fn risk(&mut self) -> Result<()> {
        // fetch versions for each dependency in that list
        let iterator = stream::iter(&mut self.dependencies)
            .map(|dependency| async move {
                // get all versions for that dependency

                if let Some(update) = &mut dependency.update {
                    let original_dep_name = &dependency.name;
                    let original_dep_version = &dependency.version;
                    let latest_version = match update.versions.last() {
                        Some(version) => version.to_string(),
                        None => {
                            error!(
                                "couldn't find new version in a dependency update: {:?}",
                                update
                            );
                            "".to_string()
                        }
                    };
                    let cargo_crate_original_version =
                        format!("{}=={}", original_dep_name, original_dep_version);
                    let cargo_crate_new_version =
                        format!("{}=={}", original_dep_name, latest_version);

                    match diff::is_diff_in_buildrs(
                        &cargo_crate_original_version,
                        &cargo_crate_new_version,
                    )
                    .await
                    {
                        Ok(update_build_rs) => update.build_rs = update_build_rs,
                        Err(e) => {
                            error!("error checking build.rs diff: {}", e)
                        }
                    };
                }
            })
            .buffer_unordered(10);
        iterator.collect::<()>().await;
        Ok(())
    }
}

//
// Summary of changes between analysis
// ===================================
//
// What matters from a user perspective?
// - new updates available (including changelog/commit)
// - new rustsec available
//

/// Contains changes observed since the last analysis
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct ChangeSummary {
    /// new updates available
    new_updates: Vec<DependencyInfo>,
    /// new RUSTSEC advisories
    new_rustsec: RustSec,
}

impl ChangeSummary {
    /// Creates a change summary by diffing two analysis together
    pub fn new(old: &RustAnalysis, new: &RustAnalysis) -> Result<ChangeSummary> {
        //
        let mut rust_changes = ChangeSummary::default();

        //
        // get new updates available
        //

        // build a hashmap of (name, etc.) -> update
        let mut dep_to_update_version: HashMap<(String, Version), Option<Version>> = HashMap::new();
        for dependency in &old.dependencies {
            let mut update_version = None;
            if let Some(update) = &dependency.update {
                update_version = update.versions.last().cloned();
                if update_version.is_none() {
                    error!(
                        "dependency update didn't have a last version: {:?}",
                        dependency
                    );
                    continue;
                }
            }
            // only insert if not present
            let name = dependency.name.clone();
            let version = dependency.version.clone();
            dep_to_update_version
                .entry((name, version))
                .or_insert(update_version);
        }

        // check for each update, if the hashmap has something
        for dependency in &new.dependencies {
            if let Some(new_update) = &dependency.update {
                let key = (dependency.name.clone(), dependency.version.clone());
                if let Some(Some(version)) = dep_to_update_version.get(&key) {
                    let new_version = match new_update.versions.last() {
                        Some(version) => version,
                        None => {
                            error!(
                                "some dependency update doesn't have a version: {:?}",
                                dependency
                            );
                            continue;
                        }
                    };
                    if new_version > version {
                        // new_er_ update found
                        rust_changes.new_updates.push(dependency.clone());
                    }
                } else {
                    // update found for new dependency or dependency w/o update
                    rust_changes.new_updates.push(dependency.clone());
                }
            }
        }

        //
        // check for new rustsec advisories
        //

        // new vulns
        let new_vulnerabilities: Vec<Vulnerability> = new
            .rustsec
            .vulnerabilities
            .iter()
            // remove what is contained in the previous vulns
            .filter(|v| !old.rustsec.vulnerabilities.contains(&v))
            .cloned()
            .collect();
        rust_changes.new_rustsec.vulnerabilities = new_vulnerabilities;

        // new warnings
        let mut new_warnings: WarningInfo = BTreeMap::new();
        // (there can be different kinds of warnings)
        for (kind, warnings) in &new.rustsec.warnings {
            if let Some(old_warnings) = old.rustsec.warnings.get(kind) {
                let warnings: Vec<Warning> = warnings
                    .iter()
                    // remove warnings for packages that have
                    .filter(|&w| {
                        old_warnings
                            .iter()
                            // TODO: theoretically, we can have a new advisory for the same package...
                            .find(|old_w| old_w.package.name == w.package.name)
                            .is_none()
                    })
                    .cloned()
                    .collect();
                // any new warnings for this kind?
                if !warnings.is_empty() {
                    new_warnings.insert(*kind, warnings);
                }
            }
        }
        rust_changes.new_rustsec.warnings = new_warnings;

        //
        Ok(rust_changes)
    }
}
