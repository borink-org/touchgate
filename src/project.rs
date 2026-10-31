//! What a repository releases and where to, as its `touchgate.json` says.
//!
//! Without a `touchgate.json`, a repository releases the published packages of
//! its Cargo workspace to crates.io. A `touchgate.json` with
//! `"registry": "pypi"` releases the Python package of its `pyproject.toml` to
//! PyPI instead, and `"extension": "maturin"` adds the Cargo workspace that
//! maturin builds the package's Rust extension from:
//!
//! ```json
//! { "registry": "pypi", "extension": "maturin" }
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::python::PythonPackage;
use crate::version::Version;
use crate::workspace::{Members, Workspace};

/// The file that says what a repository releases, at its root.
const CONFIG_FILE: &str = "touchgate.json";

/// Where a release is published.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Registry {
    CratesIo,
    PyPi,
}

impl Registry {
    /// The registry's name, as a sentence names it.
    pub fn name(self) -> &'static str {
        match self {
            Self::CratesIo => "crates.io",
            Self::PyPi => "PyPI",
        }
    }
}

/// What a Python package builds a native extension from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Extension {
    /// A pure Python package.
    None,
    /// A Rust extension built with maturin from the Cargo workspace beside
    /// `pyproject.toml`, whose packages move with the Python package's
    /// version.
    Maturin,
}

/// What `touchgate.json` says.
#[derive(Debug, PartialEq)]
pub enum ReleaseConfig {
    CratesIo,
    PyPi { extension: Extension },
}

impl ReleaseConfig {
    /// Reads `touchgate.json` at `root`, or the default of a repository
    /// without one: its Cargo workspace, to crates.io.
    fn load(root: &Path) -> Result<Self, Vec<String>> {
        let path = root.join(CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).map_err(|error| vec![format!("{CONFIG_FILE}: {error}")]),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::CratesIo),
            Err(error) => Err(vec![format!("{}: {error}", path.display())]),
        }
    }

    fn parse(text: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|error| format!("{error}"))?;
        let object = value.as_object().ok_or("not a JSON object")?;
        for key in object.keys() {
            if !["registry", "extension"].contains(&key.as_str()) {
                return Err(format!("unknown key `{key}`"));
            }
        }
        let string = |key: &str| match object.get(key) {
            None => Ok(None),
            Some(item) => item
                .as_str()
                .map(Some)
                .ok_or_else(|| format!("`{key}` is not a string")),
        };
        match (string("registry")?, string("extension")?) {
            (Some("crates-io"), None) => Ok(Self::CratesIo),
            (Some("crates-io"), Some(_)) => {
                Err("`extension` is for a Python package, with `\"registry\": \"pypi\"`".to_owned())
            }
            (Some("pypi"), extension) => Ok(Self::PyPi {
                extension: match extension {
                    None => Extension::None,
                    Some("maturin") => Extension::Maturin,
                    Some(other) => {
                        return Err(format!("`\"extension\": \"{other}\"` is not \"maturin\""));
                    }
                },
            }),
            (Some(other), _) => Err(format!(
                "`\"registry\": \"{other}\"` is not \"crates-io\" or \"pypi\""
            )),
            (None, _) => Err("no `registry`".to_owned()),
        }
    }
}

/// What the repository in the current directory releases.
pub enum Project {
    /// The published packages of a Cargo workspace, to crates.io.
    Crates(Workspace),
    /// A Python package, to PyPI, with the Cargo workspace of its Rust
    /// extension if it has one.
    Python {
        package: PythonPackage,
        extension: Option<Workspace>,
    },
}

impl Project {
    /// Reads `touchgate.json` at the root of the repository, and what it says
    /// the repository releases.
    pub fn load() -> Result<Self, Vec<String>> {
        let root = repository_root()?;
        match ReleaseConfig::load(&root)? {
            ReleaseConfig::CratesIo => Ok(Self::Crates(Workspace::load(Members::Published)?)),
            ReleaseConfig::PyPi { extension } => Ok(Self::Python {
                package: PythonPackage::load(&root)?,
                extension: match extension {
                    Extension::None => None,
                    Extension::Maturin => Some(Workspace::load(Members::All)?),
                },
            }),
        }
    }

    pub fn registry(&self) -> Registry {
        match self {
            Self::Crates(_) => Registry::CratesIo,
            Self::Python { .. } => Registry::PyPi,
        }
    }

    /// The directory of the changelog: the root of the Cargo workspace, or
    /// the directory of `pyproject.toml`.
    pub fn root(&self) -> &Path {
        match self {
            Self::Crates(workspace) => &workspace.root,
            Self::Python { package, .. } => package.dir(),
        }
    }

    /// The version the project is at. A Python package's Rust extension must
    /// be at the same version.
    pub fn version(&self) -> Result<Version, Vec<String>> {
        match self {
            Self::Crates(workspace) => workspace.version().cloned(),
            Self::Python { package, extension } => {
                if let Some(workspace) = extension {
                    let crates = workspace.version()?;
                    if *crates != package.version {
                        return Err(vec![format!(
                            "{} is at {}, but its Rust extension's crates are at {crates}. \
                             touchgate releases them at one version",
                            package.name, package.version
                        )]);
                    }
                }
                if package.version.is_pre_release() {
                    return Err(vec![format!(
                        "{} is at the pre-release {}, which touchgate does not release to PyPI: \
                         PyPI writes pre-releases another way than Cargo",
                        package.name, package.version
                    )]);
                }
                Ok(package.version.clone())
            }
        }
    }

    /// Sets `new` as the version of what the project releases.
    pub fn set_version(&self, new: &Version) -> Result<(), Vec<String>> {
        match self {
            Self::Crates(workspace) => workspace.set_version(new),
            Self::Python { package, extension } => {
                if let Some(workspace) = extension {
                    workspace.set_version(new)?;
                }
                package.set_version(new)
            }
        }
    }

    /// The names of what is released, with the directory each covers: a
    /// change under one needs a changelog entry. A Python package covers its
    /// whole directory.
    pub fn released_dirs(&self) -> Vec<(&str, &Path)> {
        match self {
            Self::Crates(workspace) => workspace
                .released()
                .map(|package| (package.name.as_str(), package.dir()))
                .collect(),
            Self::Python { package, .. } => vec![(package.name.as_str(), package.dir())],
        }
    }
}

/// The top directory of the git repository the current directory is in.
fn repository_root() -> Result<PathBuf, Vec<String>> {
    let top = crate::run(Command::new("git").args(["rev-parse", "--show-toplevel"]))?;
    Ok(PathBuf::from(top.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs() {
        assert_eq!(
            ReleaseConfig::parse(r#"{ "registry": "crates-io" }"#),
            Ok(ReleaseConfig::CratesIo)
        );
        assert_eq!(
            ReleaseConfig::parse(r#"{ "registry": "pypi" }"#),
            Ok(ReleaseConfig::PyPi {
                extension: Extension::None
            })
        );
        assert_eq!(
            ReleaseConfig::parse(r#"{ "registry": "pypi", "extension": "maturin" }"#),
            Ok(ReleaseConfig::PyPi {
                extension: Extension::Maturin
            })
        );
        for (broken, expected) in [
            ("{}", "no `registry`"),
            ("[]", "not a JSON object"),
            ("{", "EOF"),
            (
                r#"{ "registry": "npm" }"#,
                r#"is not "crates-io" or "pypi""#,
            ),
            (
                r#"{ "registry": "pypi", "extension": "pyo3" }"#,
                r#"is not "maturin""#,
            ),
            (
                r#"{ "registry": "crates-io", "extension": "maturin" }"#,
                "is for a Python package",
            ),
            (r#"{ "registry": "pypi", "tag": "v" }"#, "unknown key `tag`"),
            (r#"{ "registry": 1 }"#, "is not a string"),
        ] {
            let error = ReleaseConfig::parse(broken).unwrap_err();
            assert!(error.contains(expected), "{broken:?}: {error}");
        }
    }
}
