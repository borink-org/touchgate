//! pypi-attestations, which signs the PEP 740 attestations of a release, as
//! `touchgate publish` installs it: from a lock that fixes every file by its
//! hash.

use std::path::PathBuf;
use std::process::Command;

/// pypi-attestations, the library pypa/gh-action-pypi-publish signs with, and
/// everything it runs on, each at one version with the hashes of its files.
/// The job that can get a PyPI token installs exactly these files to sign a
/// release's attestations. uv lists the hash of every file of each version,
/// for every platform and Python, which makes it long: most of it is the
/// wheels of charset-normalizer, pydantic-core, cffi and cryptography.
/// Regenerate it in the repository's root with today's date:
///
/// ```sh
/// echo pypi-attestations==0.0.30 | uv pip compile - --universal \
///   --python-version 3.12 --generate-hashes --no-header --no-annotate \
///   --exclude-newer 2026-10-31T00:00:00Z -o src/python/pypi-attestations.txt
/// ```
const LOCK: &str = include_str!("pypi-attestations.txt");

/// The Python that [`LOCK`] is resolved for.
const PYTHON: &str = "3.12";

/// pypi-attestations, installed from [`LOCK`] into a
/// virtual environment of its own.
pub struct AttestationSigner {
    dir: PathBuf,
}

impl AttestationSigner {
    /// Installs the files [`LOCK`] names, and refuses any
    /// whose hash it does not list.
    pub fn install() -> Result<Self, Vec<String>> {
        let dir =
            std::env::temp_dir().join(format!("touchgate-attestations-{}", std::process::id()));
        let signer = Self { dir };
        let installed = signer.create();
        if installed.is_err() {
            signer.remove();
        }
        installed.map(|()| signer)
    }

    fn create(&self) -> Result<(), Vec<String>> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| vec![format!("{}: {error}", self.dir.display())])?;
        let lock = self.dir.join("requirements.txt");
        std::fs::write(&lock, LOCK)
            .map_err(|error| vec![format!("{}: {error}", lock.display())])?;
        let venv = self.dir.join("venv");
        crate::run(
            Command::new("uv")
                .args(["venv", "--clear", "--no-project", "--python", PYTHON])
                .arg(&venv),
        )?;
        crate::run(
            Command::new("uv")
                .args([
                    "pip",
                    "install",
                    "--require-hashes",
                    "--no-deps",
                    "--python",
                ])
                .arg(&venv)
                .arg("--requirements")
                .arg(&lock),
        )?;
        Ok(())
    }

    /// `pypi-attestations` in the virtual environment.
    pub fn command(&self) -> Command {
        Command::new(self.dir.join("venv/bin/pypi-attestations"))
    }

    pub fn remove(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
