//! What a repository releases and where to, as its manifests show.
//!
//! A repository with a `pyproject.toml` at its root that has a `[project]`
//! table releases that Python package to PyPI. If maturin builds it, the Cargo
//! workspace of its Rust extension moves with it. A root `pyproject.toml`
//! without `[project]` that lists the members of a uv workspace releases every
//! member to PyPI, and the published packages of the Cargo workspace at the
//! root, if there is one, to crates.io, all at one version. Any other
//! repository releases the published packages of its Cargo workspace to
//! crates.io.

use std::path::{Path, PathBuf};
use std::process::Command;

use toml_edit::{DocumentMut, Item};

use crate::python::{self, PythonPackage};
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

    /// Returns the registry that `touchgate publish --to` names.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "crates.io" => Ok(Self::CratesIo),
            "pypi" => Ok(Self::PyPi),
            _ => Err(format!("`{text}` is not `crates.io` or `pypi`")),
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
    PyPi {
        extension: Extension,
    },
    /// The members of a uv workspace, as `tool.uv.workspace.members` lists
    /// them, with the Cargo workspace at the root if there is one.
    UvWorkspace {
        members: Vec<String>,
    },
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
    /// `pyproject.toml` without `[project]` or workspace members only
    /// configures tools, as in a Rust repository, and makes no Python release.
    fn from_pyproject(text: &str) -> Result<Self, String> {
        let document: DocumentMut = text.parse().map_err(|error| format!("{error}"))?;
        if document
            .get("project")
            .and_then(Item::as_table_like)
            .is_none()
        {
            let members: Vec<String> = document
                .get("tool")
                .and_then(|tool| tool.get("uv"))
                .and_then(|uv| uv.get("workspace"))
                .and_then(|workspace| workspace.get("members"))
                .and_then(Item::as_array)
                .into_iter()
                .flatten()
                .filter_map(|member| member.as_str().map(str::to_owned))
                .collect();
            return Ok(if members.is_empty() {
                Self::CratesIo
            } else {
                Self::UvWorkspace { members }
            });
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
    /// The Python packages of a uv workspace to PyPI, and the published
    /// packages of the Cargo workspace beside them, if there is one, to
    /// crates.io. Every package of both moves to the release's version.
    UvWorkspace {
        root: PathBuf,
        /// In the order the workspace lists them, which is the order they are
        /// uploaded in.
        packages: Vec<PythonPackage>,
        crates: Option<Workspace>,
    },
}

impl Project {
    /// Reads what the repository releases, as [`ReleaseKind::detect`] tells.
    pub fn load() -> Result<Self, Vec<String>> {
        Self::load_at(&repository_root()?)
    }

    /// Reads the project at `root`, the top directory of a git checkout, as
    /// [`Project::load`] reads the one of the current directory.
    pub fn load_at(root: &Path) -> Result<Self, Vec<String>> {
        let root = root.to_path_buf();
        match ReleaseKind::detect(&root)? {
            ReleaseKind::CratesIo => Ok(Self::Crates(Workspace::load(&root, Members::Published)?)),
            ReleaseKind::PyPi { extension } => Ok(Self::Python {
                package: PythonPackage::load(&root)?,
                extension: match extension {
                    Extension::None => None,
                    Extension::Maturin => Some(Workspace::load(&root, Members::All)?),
                },
            }),
            ReleaseKind::UvWorkspace { members } => Ok(Self::UvWorkspace {
                packages: members
                    .iter()
                    .map(|member| load_member(&root, member))
                    .collect::<Result<_, _>>()?,
                crates: root
                    .join("Cargo.toml")
                    .is_file()
                    .then(|| Workspace::load(&root, Members::All))
                    .transpose()?,
                root,
            }),
        }
    }

    /// Returns the registries the project publishes to.
    pub fn registries(&self) -> Vec<Registry> {
        let mut registries = Vec::new();
        if self.published_crates().is_some() {
            registries.push(Registry::CratesIo);
        }
        if !self.python_packages().is_empty() {
            registries.push(Registry::PyPi);
        }
        registries
    }

    /// Returns the Python packages that the project publishes to PyPI.
    pub fn python_packages(&self) -> &[PythonPackage] {
        match self {
            Self::Crates(_) => &[],
            Self::Python { package, .. } => std::slice::from_ref(package),
            Self::UvWorkspace { packages, .. } => packages,
        }
    }

    /// Returns the Cargo workspace whose published packages the project
    /// publishes to crates.io, if it publishes any.
    pub fn published_crates(&self) -> Option<&Workspace> {
        match self {
            Self::Crates(workspace) => Some(workspace),
            Self::Python { .. } => None,
            Self::UvWorkspace { crates, .. } => crates
                .as_ref()
                .filter(|crates| crates.published().next().is_some()),
        }
    }

    /// Returns the directory of the changelog: the root of the Cargo workspace,
    /// or the directory of the root `pyproject.toml`.
    pub fn root(&self) -> &Path {
        match self {
            Self::Crates(workspace) => &workspace.root,
            Self::Python { package, .. } => package.dir(),
            Self::UvWorkspace { root, .. } => root,
        }
    }

    /// Returns the version that the project is at. Everything it releases,
    /// and every package of a Rust extension, must be at the same version.
    pub fn version(&self) -> Result<Version, Vec<String>> {
        let (version, crates) = match self {
            Self::Crates(workspace) => return workspace.version().cloned(),
            Self::Python { package, extension } => (&package.version, extension.as_ref()),
            Self::UvWorkspace {
                packages, crates, ..
            } => {
                let first = &packages[0];
                if let Some(other) = packages.iter().find(|p| p.version != first.version) {
                    return Err(vec![format!(
                        "{} is at {}, but {} is at {}. touchgate releases them at one version",
                        other.name, other.version, first.name, first.version
                    )]);
                }
                (&first.version, crates.as_ref())
            }
        };
        if let Some(workspace) = crates {
            let crates = workspace.version()?;
            if crates != version {
                return Err(vec![format!(
                    "the Python packages are at {version}, but the crates are at {crates}. \
                     touchgate releases them at one version"
                )]);
            }
        }
        if version.is_pre_release() {
            return Err(vec![format!(
                "the project is at the pre-release {version}, which touchgate does not release \
                 to PyPI: PyPI writes pre-releases another way than Cargo"
            )]);
        }
        Ok(version.clone())
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
            Self::UvWorkspace {
                root,
                packages,
                crates,
            } => {
                if let Some(workspace) = crates {
                    workspace.set_version(new)?;
                }
                let names: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
                for package in packages {
                    package.write_version(new, &names)?;
                }
                python::lock(root)
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
            Self::UvWorkspace {
                packages, crates, ..
            } => packages
                .iter()
                .map(|package| (package.name.as_str(), package.dir()))
                .chain(
                    crates
                        .iter()
                        .flat_map(Workspace::released)
                        .map(|package| (package.name.as_str(), package.dir())),
                )
                .collect(),
        }
    }
}

/// Loads the uv workspace member `member`, a directory relative to `root`.
fn load_member(root: &Path, member: &str) -> Result<PythonPackage, Vec<String>> {
    if member.contains(['*', '?', '[']) {
        return Err(vec![format!(
            "pyproject.toml: touchgate reads each uv workspace member as a directory, not the \
             pattern `{member}`"
        )]);
    }
    PythonPackage::load(&root.join(member))
}

/// Returns the top directory of the git repository the current directory is in.
pub fn repository_root() -> Result<PathBuf, Vec<String>> {
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
        assert_eq!(
            ReleaseKind::from_pyproject(
                "[tool.uv.workspace]\nmembers = [\"python/frc-core\", \"python/frc\"]\n"
            ),
            Ok(ReleaseKind::UvWorkspace {
                members: vec!["python/frc-core".to_owned(), "python/frc".to_owned()]
            })
        );
        // A package with a workspace releases itself, as before.
        assert_eq!(
            ReleaseKind::from_pyproject(&format!(
                "{python}[tool.uv.workspace]\nmembers = [\"packages/*\"]\n"
            )),
            Ok(ReleaseKind::PyPi {
                extension: Extension::None
            })
        );
        assert!(ReleaseKind::from_pyproject("[project").is_err());
    }
}
