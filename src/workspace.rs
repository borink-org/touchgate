//! The packages of a Cargo workspace that are published, and the edits that
//! give them a new version.
//!
//! touchgate releases every published package of a workspace together, at one
//! version.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use toml_edit::{DocumentMut, Item, TableLike, Value};

use crate::version::Version;

/// A package of the workspace.
pub struct Package {
    pub name: String,
    pub version: Version,
    pub manifest: PathBuf,
    pub published: bool,
}

impl Package {
    /// The directory of the package.
    pub fn dir(&self) -> &Path {
        self.manifest
            .parent()
            .expect("a manifest is in a directory")
    }
}

/// The workspace in the current directory.
pub struct Workspace {
    pub root: PathBuf,
    pub packages: Vec<Package>,
}

impl Workspace {
    /// Reads the workspace with `cargo metadata`.
    pub fn load() -> Result<Self, Vec<String>> {
        let output = crate::run(Command::new("cargo").args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
        ]))?;
        let metadata: serde_json::Value = serde_json::from_str(&output)
            .map_err(|error| vec![format!("cargo metadata: {error}")])?;
        let missing = |field: &str| vec![format!("cargo metadata: no `{field}`")];
        let root = metadata["workspace_root"]
            .as_str()
            .ok_or_else(|| missing("workspace_root"))?;
        let mut packages = Vec::new();
        let mut errors = Vec::new();
        for package in metadata["packages"]
            .as_array()
            .ok_or_else(|| missing("packages"))?
        {
            let (Some(name), Some(version), Some(manifest)) = (
                package["name"].as_str(),
                package["version"].as_str(),
                package["manifest_path"].as_str(),
            ) else {
                return Err(missing("packages[].name, version or manifest_path"));
            };
            match Version::parse(version) {
                Ok(version) => packages.push(Package {
                    name: name.to_owned(),
                    version,
                    manifest: PathBuf::from(manifest),
                    // `publish = false` is an empty list of registries.
                    published: package["publish"].as_array().is_none_or(|r| !r.is_empty()),
                }),
                Err(error) => errors.push(format!("{name}: {error}")),
            }
        }
        if errors.is_empty() {
            Ok(Self {
                root: PathBuf::from(root),
                packages,
            })
        } else {
            Err(errors)
        }
    }

    /// The packages published to a registry.
    pub fn published(&self) -> impl Iterator<Item = &Package> {
        self.packages.iter().filter(|package| package.published)
    }

    /// The version every published package is at.
    pub fn version(&self) -> Result<&Version, Vec<String>> {
        let mut published = self.published();
        let first = published
            .next()
            .ok_or_else(|| vec!["the workspace publishes no package".to_owned()])?;
        let errors: Vec<String> = published
            .filter(|package| package.version != first.version)
            .map(|package| {
                format!(
                    "{} is at {}, but {} is at {}. touchgate releases every published package at one version",
                    package.name, package.version, first.name, first.version
                )
            })
            .collect();
        if errors.is_empty() {
            Ok(&first.version)
        } else {
            Err(errors)
        }
    }

    /// Sets `new` as the version of every published package, and in every
    /// requirement on one that names the current version. Then lets Cargo
    /// bring `Cargo.lock` along.
    pub fn set_version(&self, new: &Version) -> Result<(), Vec<String>> {
        let old = self.version()?.clone();
        let published: HashSet<&str> = self.published().map(|p| p.name.as_str()).collect();
        let root_manifest = self.root.join("Cargo.toml");

        let mut documents = BTreeMap::new();
        for path in std::iter::once(&root_manifest).chain(self.packages.iter().map(|p| &p.manifest))
        {
            if !documents.contains_key(path) {
                let text = crate::read(path)?;
                let document: DocumentMut = text
                    .parse()
                    .map_err(|error| vec![format!("{}: {error}", path.display())])?;
                documents.insert(path.clone(), document);
            }
        }

        let mut inherited = false;
        for package in self.published() {
            let document = documents.get_mut(&package.manifest).expect("read above");
            let version = &mut document["package"]["version"];
            if version.is_str() {
                set_string(version, &new.to_string());
            } else if version.get("workspace").and_then(Item::as_bool) == Some(true) {
                inherited = true;
            } else {
                return Err(vec![format!(
                    "{}: `package.version` is neither a string nor inherited from the workspace",
                    package.manifest.display()
                )]);
            }
        }
        if inherited {
            let document = documents.get_mut(&root_manifest).expect("read above");
            let version = &mut document["workspace"]["package"]["version"];
            if !version.is_str() {
                return Err(vec![format!(
                    "{}: no `workspace.package.version` to inherit",
                    root_manifest.display()
                )]);
            }
            set_string(version, &new.to_string());
        }

        for document in documents.values_mut() {
            let table = document.as_table_mut();
            for tables in dependency_tables(table) {
                bump_requirements(tables, &published, &old, new);
            }
        }

        for (path, document) in &documents {
            std::fs::write(path, document.to_string())
                .map_err(|error| vec![format!("{}: {error}", path.display())])?;
        }
        // Resolving the workspace moves the published packages' entries in
        // `Cargo.lock` to the new version, and upgrades nothing else.
        crate::run(Command::new("cargo").args(["metadata", "--format-version", "1"]))?;

        let updated = Self::load()?;
        let version = updated.version()?;
        if version != new {
            return Err(vec![format!(
                "the published packages are at {version} after the edit, not {new}"
            )]);
        }
        Ok(())
    }
}

/// The dependency tables of a manifest: `[dependencies]` and its dev and build
/// kinds, the same under each `[target.*]`, and `[workspace.dependencies]`.
fn dependency_tables(manifest: &mut toml_edit::Table) -> Vec<&mut dyn TableLike> {
    const KINDS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
    let mut tables: Vec<&mut dyn TableLike> = Vec::new();
    for (key, item) in manifest.iter_mut() {
        match key.get() {
            kind if KINDS.contains(&kind) => tables.extend(item.as_table_like_mut()),
            "target" => {
                for (_, target) in item
                    .as_table_like_mut()
                    .into_iter()
                    .flat_map(|t| t.iter_mut())
                {
                    for (kind, table) in target
                        .as_table_like_mut()
                        .into_iter()
                        .flat_map(|t| t.iter_mut())
                    {
                        if KINDS.contains(&kind.get()) {
                            tables.extend(table.as_table_like_mut());
                        }
                    }
                }
            }
            "workspace" => {
                if let Some(workspace) = item.as_table_like_mut()
                    && let Some(dependencies) = workspace.get_mut("dependencies")
                {
                    tables.extend(dependencies.as_table_like_mut());
                }
            }
            _ => {}
        }
    }
    tables
}

/// Moves each requirement on a published package from `old` to `new`, where it
/// names `old` exactly. A wider requirement, such as `0.4`, is left to hold or
/// fail when Cargo resolves the workspace.
fn bump_requirements(
    table: &mut dyn TableLike,
    published: &HashSet<&str>,
    old: &Version,
    new: &Version,
) {
    for (key, dependency) in table.iter_mut() {
        let name = dependency
            .get("package")
            .and_then(Item::as_str)
            .unwrap_or(key.get())
            .to_owned();
        if !published.contains(name.as_str()) {
            continue;
        }
        let requirement = if dependency.is_str() {
            Some(dependency)
        } else {
            dependency
                .as_table_like_mut()
                .and_then(|table| table.get_mut("version"))
        };
        if let Some(requirement) = requirement
            && let Some(bumped) = requirement
                .as_str()
                .and_then(|text| bump_requirement(text, old, new))
        {
            set_string(requirement, &bumped);
        }
    }
}

/// `requirement` with `new` in place of `old`, if it names exactly `old`,
/// bare or after `=`, `^` or `~`.
fn bump_requirement(requirement: &str, old: &Version, new: &Version) -> Option<String> {
    let operator_len = requirement
        .find(|c: char| !matches!(c, '=' | '^' | '~' | ' '))
        .unwrap_or(requirement.len());
    let (operator, version) = requirement.split_at(operator_len);
    (version == old.to_string()).then(|| format!("{operator}{new}"))
}

/// Replaces a string value, keeping the spacing and comments around it.
fn set_string(item: &mut Item, text: &str) {
    if let Some(value) = item.as_value_mut() {
        let decor = value.decor().clone();
        *value = Value::from(text);
        *value.decor_mut() = decor;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirements() {
        let old = Version::parse("0.0.3").unwrap();
        let new = Version::parse("0.0.4").unwrap();
        let mut manifest: DocumentMut = r#"
[dependencies]
a = { path = "../a", version = "0.0.3" } # pinned
b = { version = "=0.0.3", package = "c" }
d = { path = "../d", version = "0.0" }
other = "0.0.3"

[target.'cfg(unix)'.dev-dependencies]
a = "^0.0.3"
"#
        .parse()
        .unwrap();
        let published = HashSet::from(["a", "c", "d"]);
        for table in dependency_tables(manifest.as_table_mut()) {
            bump_requirements(table, &published, &old, &new);
        }
        assert_eq!(
            manifest.to_string(),
            r#"
[dependencies]
a = { path = "../a", version = "0.0.4" } # pinned
b = { version = "=0.0.4", package = "c" }
d = { path = "../d", version = "0.0" }
other = "0.0.3"

[target.'cfg(unix)'.dev-dependencies]
a = "^0.0.4"
"#
        );
    }
}
