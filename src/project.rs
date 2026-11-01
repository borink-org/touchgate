//! What a repository releases and where to, as its manifests show.
//!
//! A repository with a `pyproject.toml` at its root that has a `[project]`
//! table releases that Python package to PyPI. If maturin builds it, the Cargo
//! workspace of its Rust extension moves with it. Any other repository
//! releases the published packages of its Cargo workspace to crates.io.

use std::path::{Path, PathBuf};
use std::process::Command;

use toml_edit::{DocumentMut, Item};

use crate::python::PythonPackage;
use crate::version::Version;
use crate::workspace::{Members, Workspace};

/// Where a release is published.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Registry {
    CratesIo,
    PyPi,
}

impl Registry {
    /// Returns the name of the registry, as a sentence writes it.
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

/// What kind of release a repository makes.
#[derive(Debug, PartialEq)]
enum ReleaseKind {
    CratesIo,
    PyPi { extension: Extension },
}

impl ReleaseKind {
    /// Reads the kind from the `pyproject.toml` at `root`, if there is one.
    fn detect(root: &Path) -> Result<Self, Vec<String>> {
        let path = root.join("pyproject.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_pyproject(&text)
                .map_err(|error| vec![format!("{}: {error}", path.display())]),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::CratesIo),
            Err(error) => Err(vec![format!("{}: {error}", path.display())]),
        }
    }

    /// Reads the kind of release from the text of a `pyproject.toml`. A
    /// `pyproject.toml` without `[project]` only configures tools, as in a Rust
    /// repository, and makes no Python release.
    fn from_pyproject(text: &str) -> Result<Self, String> {
        let document: DocumentMut = text.parse().map_err(|error| format!("{error}"))?;
        if document
            .get("project")
            .and_then(Item::as_table_like)
            .is_none()
        {
            return Ok(Self::CratesIo);
        }
        let backend = document
            .get("build-system")
            .and_then(|build_system| build_system.get("build-backend"))
            .and_then(Item::as_str);
        Ok(Self::PyPi {
            extension: match backend {
                Some("maturin") => Extension::Maturin,
                _ => Extension::None,
            },
        })
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
    /// Reads what the repository releases, as [`ReleaseKind::detect`] tells.
    pub fn load() -> Result<Self, Vec<String>> {
        let root = repository_root()?;
        match ReleaseKind::detect(&root)? {
            ReleaseKind::CratesIo => Ok(Self::Crates(Workspace::load(Members::Published)?)),
            ReleaseKind::PyPi { extension } => Ok(Self::Python {
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

    /// Returns the directory of the changelog: the root of the Cargo workspace,
    /// or the directory of `pyproject.toml`.
    pub fn root(&self) -> &Path {
        match self {
            Self::Crates(workspace) => &workspace.root,
            Self::Python { package, .. } => package.dir(),
        }
    }

    /// Returns the version that the project is at. The Rust extension of a
    /// Python package must be at the same version.
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

    /// Returns the names of what is released, each with the directory it
    /// covers. A change under one of these directories needs a changelog entry.
    /// A Python package covers its whole directory.
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

/// Returns the top directory of the git repository the current directory is in.
fn repository_root() -> Result<PathBuf, Vec<String>> {
    let top = crate::run(Command::new("git").args(["rev-parse", "--show-toplevel"]))?;
    Ok(PathBuf::from(top.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_kinds() {
        let python = "[project]\nname = \"frc\"\nversion = \"0.1.2\"\n";
        assert_eq!(
            ReleaseKind::from_pyproject(python),
            Ok(ReleaseKind::PyPi {
                extension: Extension::None
            })
        );
        assert_eq!(
            ReleaseKind::from_pyproject(&format!(
                "{python}[build-system]\nrequires = [\"maturin>=1.16,<2.0\"]\nbuild-backend = \"maturin\"\n"
            )),
            Ok(ReleaseKind::PyPi {
                extension: Extension::Maturin
            })
        );
        assert_eq!(
            ReleaseKind::from_pyproject("[tool.ruff]\nline-length = 100\n"),
            Ok(ReleaseKind::CratesIo)
        );
        assert!(ReleaseKind::from_pyproject("[project").is_err());
    }
}
