// Copyright 2021 AgileBits Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::RazeError;
use crate::settings::RazeSettings;
use crate::util::cargo_bin_path;
use anyhow::{Error, Result};
use cargo_metadata::{Package, PackageId, Version};
use serde::{Deserialize, Serialize};

type UnconsolidatedFeatures = HashMap<PackageId, HashMap<String, HashSet<String>>>;

#[derive(Debug, Clone, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Features {
  pub features: Vec<String>,
  pub targeted_features: Vec<TargetedFeatures>,
}

impl Features {
  pub fn empty() -> Features {
    Features {
      features: Vec::new(),
      targeted_features: vec![],
    }
  }
}

#[derive(Debug, Clone, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TargetedFeatures {
  pub platforms: Vec<String>,
  pub features: Vec<String>,
}

// A function that runs `cargo-tree` to analyze per-platform features.
// This step should not need to be separate from cargo-metadata, but cargo-metadata's 
// output is currently incomplete in this respect.
//
// See: https://github.com/rust-lang/cargo/issues/9863
// and: https://github.com/illicitonion/cargo-metadata-pathologies/tree/main/platform-specific-features
//
pub fn get_per_platform_features(
  cargo_dir: &Path,
  settings: &RazeSettings,
  packages: &Vec<Package>,
) -> Result<HashMap<PackageId, Features>> {
  let mut triples: HashSet<String> = HashSet::new();
  if let Some(target) = settings.target.clone() {
    triples.insert(target);
  }
  if let Some(targets) = settings.targets.clone() {
    triples.extend(targets);
  }

  let mut triple_map = HashMap::new();
  for triple in triples {
    triple_map.insert(
      triple.clone(),
      // TODO: This part is slow, since it runs cargo-tree per-platform.
      run_cargo_tree(cargo_dir, triple.as_str(), packages)?,
    );
  }

  let features: Vec<(PackageId, Features)> = transpose_keys(triple_map)
    .into_iter()
    .map(consolidate_features)
    .collect();
  let mut m = HashMap::new();
  for f in features {
    let (id, features) = f;
    m.insert(id, features);
  }
  Ok(m)
}

// Runs `cargo-tree` with a very specific format argument that makes it easier
// to extract per-platform targets.
fn run_cargo_tree(
  cargo_dir: &Path,
  triple: &str,
  packages: &Vec<Package>,
) -> Result<HashMap<PackageId, HashSet<String>>> {
  // TODO: remove this
  eprintln!("Run cargo-tree for {}.", triple);

  let cargo_bin: PathBuf = cargo_bin_path();
  let mut cargo_tree = Command::new(cargo_bin);
  cargo_tree.current_dir(cargo_dir);
  cargo_tree
    .arg("tree")
    .arg("--prefix=none")
    .arg("--frozen")
    .arg(format!("--target={}", triple))
    .arg("--format={p}|{f}|"); // The format to print output with

  let tree_output = cargo_tree.output()?;
  assert!(tree_output.status.success());

  let text = String::from_utf8(tree_output.stdout)?;
  let mut crates: HashSet<String> = HashSet::new();
  for line in text.lines().filter(|line| {
    // remove dedupe lines     // remove lines with no features
    !(line.ends_with("(*)") || line.ends_with("||") || line.is_empty())
  }) {
    crates.insert(line.to_string());
  }
  let crate_vec: Vec<String> = crates.drain().collect();
  make_package_map(crate_vec, packages)
}

fn make_package_map(crates: Vec<String>, packages: &Vec<Package>) -> Result<HashMap<PackageId, HashSet<String>>> {
  let mut package_map: HashMap<PackageId, HashSet<String>> = HashMap::new();
  for c in &crates {
    let (name, version, features) = process_line(&c)?;
    let id = find_package_id(name, version, packages)?;

    // TODO: this should not be necessary
    match package_map.get_mut(&id) {
      Some(existing_features) => {
        let f = existing_features.union(&features).cloned().collect();
        package_map.insert(id, f);
      }
      None => {
        package_map.insert(id, features);
      }
    }
  }
  Ok(package_map)
}

// Process the output of cargo-tree to discover features that are only targeted
// per-platform. The input format is specified in `run_cargo_tree`.
//
// This function does some basic text processing, and ignores
// bogus and/or repetitive lines that cargo tree inserts.
fn process_line(s: &String) -> Result<(String, Version, HashSet<String>)> {
  match (s.find(" "), s.find("|")) {
    (Some(space), Some(pipe)) => {
      let (package, features) = s.split_at(pipe);
      let features_trimmed = features.replace("|", "");
      let feature_set: HashSet<String> = features_trimmed
        .split(",")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
      let (name, mut version_str) = package.split_at(space);
      version_str = version_str.trim_start_matches(|c| c == ' ' || c == 'v');
      let version_end = match version_str.find(" ") {
        Some(index) => index,
        None => version_str.chars().count(),
      };
      let version = Version::parse(&version_str[..version_end])?;
      Ok((name.trim().to_string(), version, feature_set))
    }
    _ => Err(Error::new(RazeError::Generic(
      "Failed to process cargo tree line.".into(),
    ))),
  }
}

fn find_package_id(name: String, version: Version, packages: &Vec<Package>) -> Result<PackageId> {
  packages
    .iter()
    .find(|package| package.name == name && package.version == version)
    .map(|package| package.id.clone())
    .ok_or(Error::new(RazeError::Generic(
      "Failed to find package.".into(),
    )))
}

// TODO: this needs to be redone with a BTree and made generic for build targets
fn transpose_keys(
  triples: HashMap<String, HashMap<PackageId, HashSet<String>>>,
) -> UnconsolidatedFeatures {
  let mut package_map: HashMap<PackageId, HashMap<String, HashSet<String>>> = HashMap::new();
  for (triple, packages) in triples {
    for (pkg, features) in packages {
      match package_map.get_mut(&pkg) {
        Some(triple_map) => {
          triple_map.insert(triple.clone(), features);
        },
        None => {
          let mut m = HashMap::new();
          m.insert(triple.clone(), features);
          package_map.insert(pkg.clone(), m);
        }
      }
    }
  }
  package_map
}

// TODO: this needs to be redone with a BTree and made generic for build targets
fn consolidate_features(pkg: (PackageId, HashMap<String, HashSet<String>>)) -> (PackageId, Features) {
  let (id, features) = pkg;

  // Find the features common to all targets
  let sets: Vec<&HashSet<String>> = features.values().collect();
  let common_features = sets.iter().skip(1).fold(sets[0].clone(), |acc, hs| {
    acc.intersection(hs).cloned().collect()
  });

  // Partition the platform features
  let mut platform_map: HashMap<String, Vec<String>> = HashMap::new();
  for (platform, pfs) in features {
    for feature in pfs {
      if !common_features.contains(&feature) {
        match platform_map.get_mut(&feature) {
          Some(platforms) => {
            platforms.push(platform.clone());
          }
          None => {
            platform_map.insert(feature, vec![platform.clone()]);
          }
        }
      }
    }
  }

  let mut platforms_to_features: HashMap<Vec<String>, Vec<String>> = HashMap::new();
  for (feature, platforms) in platform_map {
    let mut key = platforms.clone();
    key.sort();
    match platforms_to_features.get_mut(&key) {
      Some(features) => {
        features.push(feature);
        features.sort();
      }
      None => {
        platforms_to_features.insert(key, vec![feature]);
      }
    }
  }

  let mut common_vec: Vec<String> = common_features.iter().map(|s| s.clone()).collect();
  common_vec.sort();

  let mut targeted_features: Vec<TargetedFeatures> = platforms_to_features
  .iter()
  .map(|ptf| {
    let (platforms, features) = ptf;
    TargetedFeatures {
      platforms: platforms.to_vec(),
      features: features.to_vec(),
    }
  })
  .collect();

  // Sort to keep the output stable
  targeted_features.sort_by(|a, b| {
    if a.platforms.len() != b.platforms.len() {
      a.platforms.len().cmp(&b.platforms.len())
    } else if a.platforms.len() > 0 {
      a.platforms[0].cmp(&b.platforms[0])
    } else {
      Ordering::Equal
    }
  });
  targeted_features.reverse();

  (
    id,
    Features {
      features: common_vec,
      targeted_features,
    }
  )
}
 