use anyhow::{anyhow, Result};
use git2::Repository;
use guppy_summaries::SummaryWithMetadata;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

//
//
//

pub struct Rust {
    summaryRelease: Vec<u8>,
    summaryDev: Vec<u8>,
}

impl Rust {
    // use guppy summaries
    pub fn get_release_dependencies(&mut self) -> Result<()> {
        // this will produce a json file containing no dev dependencies
        // (only transitive dependencies used in release)
        let out_dir = tempdir()?;
        let output = Command::new("cargo")
            .args(&["x", "generate-summaries"])
            .arg(&out_dir.path())
            .arg("json")
            .output()?;

        // deserialize the release summary
        let file = File::open(out_dir)?;
        let reader = BufReader::new(file);
        let summary: SummaryWithMetadata = serde_json::from_reader(reader)?;

        // transform it to:
        // - remove workspace/internal packages
        // - remove metadata
        // - remove features

        //
        Ok(())
    }

    pub fn parse_dependencies() {}

    // get dev dependencies by doing cargo select
    // then remove anything that's in release
    pub fn get_dev_dependencies(&mut self) {
        if self.summaryRelease.len() == 0 {
            panic!("must get info on release dependencies first");
        }

        //      cargo guppy select --kind ThirdParty > ../third_party.deps
        //      cargo guppy select --kind DirectThirdParty > ../direct_third_party.deps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_summary() {
        // read the release summary
        let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        d.push("resources/test/summary-release.json");
        let file = File::open(d).unwrap();
        let reader = BufReader::new(file);

        // Read the JSON contents of the file as an instance of `User`.
        let u: SummaryWithMetadata = serde_json::from_reader(reader).unwrap();
        println!("{:?}", u);
    }
}
