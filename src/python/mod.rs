//! A Python package, as its `pyproject.toml` describes it, the edit that gives
//! it a new version, and its upload to PyPI.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use toml_edit::{DocumentMut, Item, Value};

use crate::version::Version;

mod attestations;
mod backend;

use attestations::AttestationSigner;
use backend::{BuildBackend, build_backend};

/// The index `uv publish` asks which files PyPI has already.
const PYPI_INDEX: &str = "https://pypi.org/simple/";

/// What GitHub Actions sets in every job.
const IN_GITHUB_ACTIONS: &str = "GITHUB_ACTIONS";

/// What GitHub Actions sets in a job allowed `id-token: write`, for asking
/// for the job's OIDC token.
const OIDC_REQUEST_URL: &str = "ACTIONS_ID_TOKEN_REQUEST_URL";

/// The Python package whose `pyproject.toml` is at the repository's root.
pub struct PythonPackage {
    pub name: String,
    pub version: Version,
    manifest: PathBuf,
}

impl PythonPackage {
    /// Reads `[project]` of `root/pyproject.toml`, which must give its
    /// version itself, not leave it to the build backend.
    pub fn load(root: &Path) -> Result<Self, Vec<String>> {
        let manifest = root.join("pyproject.toml");
        let text = crate::read(&manifest)?;
        let document: DocumentMut = text
            .parse()
            .map_err(|error| vec![format!("{}: {error}", manifest.display())])?;
        let error = |message: &str| vec![format!("{}: {message}", manifest.display())];
        let project = document
            .get("project")
            .and_then(Item::as_table_like)
            .ok_or_else(|| error("no `[project]` table"))?;
        let name = project
            .get("name")
            .and_then(Item::as_str)
            .ok_or_else(|| error("no `project.name`"))?;
        let dynamic = project
            .get("dynamic")
            .and_then(Item::as_array)
            .is_some_and(|dynamic| dynamic.iter().any(|item| item.as_str() == Some("version")));
        if dynamic {
            return Err(error(
                "`project.version` is dynamic. touchgate sets the version, so write it in \
                 `[project]`",
            ));
        }
        let version = project
            .get("version")
            .and_then(Item::as_str)
            .ok_or_else(|| error("no `project.version`"))?;
        let version = Version::parse(version).map_err(|message| error(&message))?;
        Ok(Self {
            name: name.to_owned(),
            version,
            manifest,
        })
    }

    /// The directory of `pyproject.toml`.
    pub fn dir(&self) -> &Path {
        self.manifest
            .parent()
            .expect("pyproject.toml is in a directory")
    }

    /// Sets `new` as `project.version`, and moves `uv.lock` along if the
    /// package has one.
    pub fn set_version(&self, new: &Version) -> Result<(), Vec<String>> {
        let text = crate::read(&self.manifest)?;
        let mut document: DocumentMut = text
            .parse()
            .map_err(|error| vec![format!("{}: {error}", self.manifest.display())])?;
        let version = &mut document["project"]["version"];
        let Some(value) = version.as_value_mut() else {
            return Err(vec![format!(
                "{}: no `project.version`",
                self.manifest.display()
            )]);
        };
        let decor = value.decor().clone();
        *value = Value::from(new.to_string());
        *value.decor_mut() = decor;
        std::fs::write(&self.manifest, document.to_string())
            .map_err(|error| vec![format!("{}: {error}", self.manifest.display())])?;

        // `uv lock` records the package's own version, and moves nothing else
        // that still fits.
        if self.dir().join("uv.lock").is_file() {
            crate::run(Command::new("uv").arg("lock").current_dir(self.dir()))?;
        }

        let updated = Self::load(self.dir())?;
        if updated.version != *new {
            return Err(vec![format!(
                "{} is at {} after the edit, not {new}",
                self.name, updated.version
            )]);
        }
        Ok(())
    }

    /// Checks that `uv build` would build the package with the copy of
    /// `uv_build` built into the uv on the `PATH`, rather than download one
    /// from PyPI, and says what it builds with. A package with another backend
    /// passes: uv always downloads it.
    ///
    /// A uv pinned in the release workflow then pins the backend as well,
    /// which it no longer does once uv is outside the package's `uv_build`
    /// range.
    pub fn check_build_backend(&self) -> Result<String, Vec<String>> {
        let output = crate::run(Command::new("uv").arg("--version"))?;
        // `uv 0.13.6 (329bf9448 2026-10-28)`
        let uv = output
            .split_whitespace()
            .nth(1)
            .and_then(|version| pep440_rs::Version::from_str(version).ok())
            .ok_or_else(|| vec![format!("uv --version: no version in {:?}", output.trim())])?;
        let text = crate::read(&self.manifest)?;
        let document: DocumentMut = text
            .parse()
            .map_err(|error| vec![format!("{}: {error}", self.manifest.display())])?;
        match build_backend(&document, &uv) {
            Ok(BuildBackend::BundledUvBuild) => Ok(format!(
                "uv {uv} builds {} with its own copy of uv_build",
                self.name
            )),
            Ok(BuildBackend::Downloaded(backend)) => Ok(format!(
                "{} builds with {backend}, which uv downloads from PyPI",
                self.name
            )),
            Err(error) => Err(vec![format!("{}: {error}", self.manifest.display())]),
        }
    }

    /// Uploads the built files in `dist` to PyPI, each with an attestation
    /// that this job made it, and skips those PyPI has already, so that a
    /// release that failed halfway can run again. `dist` must hold this
    /// version's wheels and sdist and nothing else.
    ///
    /// The job runs with `id-token: write`: both pypi-attestations and
    /// `uv publish` take this job's identity from GitHub, and PyPI matches it
    /// to its trusted publisher.
    pub fn publish(&self, dist: &Path) -> Result<(), Vec<String>> {
        // Outside GitHub Actions, pypi-attestations would wait for a browser
        // to sign in instead.
        if std::env::var(IN_GITHUB_ACTIONS).as_deref() != Ok("true") {
            return Err(vec![format!(
                "{IN_GITHUB_ACTIONS} is not `true`. touchgate publishes to PyPI only from a \
                 GitHub Actions job"
            )]);
        }
        if std::env::var_os(OIDC_REQUEST_URL).is_none() {
            return Err(vec![format!(
                "{OIDC_REQUEST_URL} is not set. Give the job that publishes `id-token: write`"
            )]);
        }
        let files = self.release_files(dist)?;
        let signer = AttestationSigner::install()?;
        let signed = crate::run(signer.command().arg("sign").args(&files));
        signer.remove();
        signed?;
        // `uv publish` attaches an attestation only if it is among the paths
        // it is given, beside its file.
        let mut attestations = Vec::new();
        for file in &files {
            let attestation = publish_attestation(file);
            if !attestation.is_file() {
                return Err(vec![format!(
                    "pypi-attestations wrote no {}",
                    attestation.display()
                )]);
            }
            attestations.push(attestation);
        }
        println!("signed {} attestations", attestations.len());
        let status = Command::new("uv")
            .args([
                "publish",
                "--trusted-publishing",
                "always",
                "--check-url",
                PYPI_INDEX,
            ])
            .args(&files)
            .args(&attestations)
            .status()
            .map_err(|error| vec![format!("uv publish: {error}")])?;
        if status.success() {
            Ok(())
        } else {
            Err(vec![format!("uv publish failed: {status}")])
        }
    }

    /// The files in `dist`, sorted, each a wheel or the sdist of this version.
    fn release_files(&self, dist: &Path) -> Result<Vec<PathBuf>, Vec<String>> {
        let entries = std::fs::read_dir(dist)
            .map_err(|error| vec![format!("{}: {error}", dist.display())])?;
        let mut files = Vec::new();
        let mut errors = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| vec![format!("{}: {error}", dist.display())])?;
            let file_name = entry.file_name();
            match file_name.to_str() {
                // `uv build` writes one, so that git leaves `dist` alone.
                Some(".gitignore") => {}
                Some(name) if self.is_release_file(name) => files.push(entry.path()),
                _ => errors.push(format!(
                    "{}: {} is not a wheel or the sdist of {} {}",
                    dist.display(),
                    file_name.to_string_lossy(),
                    self.name,
                    self.version
                )),
            }
        }
        if files.is_empty() && errors.is_empty() {
            errors.push(format!("{}: there is nothing to publish", dist.display()));
        }
        if errors.is_empty() {
            files.sort();
            Ok(files)
        } else {
            Err(errors)
        }
    }

    /// Whether `file` is named as a wheel or the sdist of this version.
    fn is_release_file(&self, file: &str) -> bool {
        let prefix = format!("{}-{}", file_name_part(&self.name), self.version);
        match file.strip_prefix(&prefix) {
            Some(".tar.gz") => true,
            Some(rest) => rest.starts_with('-') && rest.ends_with(".whl"),
            None => false,
        }
    }
}

/// Where `pypi-attestations sign` writes the attestation of `file`, the path
/// `uv publish` pairs with it: `frc-0.1.2.tar.gz.publish.attestation`.
fn publish_attestation(file: &Path) -> PathBuf {
    let mut path = file.as_os_str().to_owned();
    path.push(".publish.attestation");
    PathBuf::from(path)
}

/// `name` as the file names of its wheels and sdists write it: lower case,
/// with each run of `-`, `_` and `.` turned into one `_`.
fn file_name_part(name: &str) -> String {
    let mut part = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !part.ends_with('_') {
                part.push('_');
            }
        } else {
            part.push(c.to_ascii_lowercase());
        }
    }
    part
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str, version: &str) -> PythonPackage {
        PythonPackage {
            name: name.to_owned(),
            version: Version::parse(version).unwrap(),
            manifest: PathBuf::from("pyproject.toml"),
        }
    }

    #[test]
    fn attestation_paths() {
        assert_eq!(
            publish_attestation(Path::new("dist/frc-0.1.2.tar.gz")),
            PathBuf::from("dist/frc-0.1.2.tar.gz.publish.attestation")
        );
    }

    #[test]
    fn release_file_names() {
        let frc = package("frc", "0.1.2");
        for file in [
            "frc-0.1.2.tar.gz",
            "frc-0.1.2-py3-none-any.whl",
            "frc-0.1.2-cp312-cp312-manylinux_2_17_x86_64.whl",
        ] {
            assert!(frc.is_release_file(file), "{file}");
        }
        for file in [
            "frc-0.1.1.tar.gz",
            "frc-0.1.20-py3-none-any.whl",
            "frc-0.1.2-py3-none-any.whl.publish.attestation",
            "frc-0.1.2.zip",
            "frcx-0.1.2.tar.gz",
        ] {
            assert!(!frc.is_release_file(file), "{file}");
        }
        assert!(package("Py.Some--Name", "1.0.0").is_release_file("py_some_name-1.0.0.tar.gz"));
    }

    #[test]
    fn reads_and_sets_the_version() {
        let dir = std::env::temp_dir().join(format!("touchgate-python-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("pyproject.toml");
        std::fs::write(
            &manifest,
            "[project]\nname = \"frc\"\nversion = \"0.1.2\" # the release\n\n[tool.uv]\n",
        )
        .unwrap();
        let package = PythonPackage::load(&dir).unwrap();
        assert_eq!(package.version, Version::parse("0.1.2").unwrap());
        package
            .set_version(&Version::parse("0.2.0").unwrap())
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&manifest).unwrap(),
            "[project]\nname = \"frc\"\nversion = \"0.2.0\" # the release\n\n[tool.uv]\n"
        );

        std::fs::write(
            &manifest,
            "[project]\nname = \"frc\"\ndynamic = [\"version\"]\n",
        )
        .unwrap();
        let errors = PythonPackage::load(&dir).err().unwrap();
        assert!(errors[0].contains("is dynamic"), "{errors:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
