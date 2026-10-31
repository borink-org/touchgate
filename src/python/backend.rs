//! Which build backend `uv build` gets for a Python package: the copy of
//! `uv_build` built into uv, or one it downloads from PyPI.

use std::str::FromStr;

use toml_edit::{DocumentMut, Item, Value};

use super::file_name_part;

/// What `uv build` builds a package with.
#[derive(Debug, PartialEq)]
pub enum BuildBackend {
    /// The copy of `uv_build` built into the running uv.
    BundledUvBuild,
    /// Another backend, by its `build-backend`, which uv downloads from PyPI.
    Downloaded(String),
}

/// The backend `[build-system]` of `document` gets from a uv at `uv`, or why
/// uv would download `uv_build` from PyPI rather than use its own copy.
///
/// uv uses its own copy when `requires` is a single `uv_build` requirement
/// whose range holds the running uv, or one of a few older versions uv knows
/// to build the same. touchgate asks for the running uv alone, which never
/// passes a `uv_build` uv would download.
pub fn build_backend(
    document: &DocumentMut,
    uv: &pep440_rs::Version,
) -> Result<BuildBackend, String> {
    let Some(build_system) = document.get("build-system").and_then(Item::as_table_like) else {
        // uv then builds with setuptools, as pip does.
        return Ok(BuildBackend::Downloaded(
            "setuptools, for want of a `[build-system]`".to_owned(),
        ));
    };
    let backend = build_system
        .get("build-backend")
        .and_then(Item::as_str)
        .unwrap_or("setuptools.build_meta:__legacy__");
    if backend != "uv_build" {
        return Ok(BuildBackend::Downloaded(backend.to_owned()));
    }
    let requires: Vec<&str> = build_system
        .get("requires")
        .and_then(Item::as_array)
        .map(|requires| requires.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let [requirement] = requires.as_slice() else {
        return Err(format!(
            "uv builds with its own copy of uv_build only if `build-system.requires` is a single \
             `uv_build` requirement, not {requires:?}"
        ));
    };
    let name_end = requirement
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(requirement.len());
    let (name, range) = requirement.split_at(name_end);
    if file_name_part(name) != "uv_build" {
        return Err(format!(
            "uv builds with its own copy of uv_build only if `build-system.requires` is \
             `uv_build`, not `{requirement}`"
        ));
    }
    let range = range.trim();
    if range.contains([';', '[', '@']) {
        return Err(format!(
            "`{requirement}` has extras, markers or a URL. touchgate reads a `uv_build` \
             requirement with a version range alone"
        ));
    }
    let specifiers = pep440_rs::VersionSpecifiers::from_str(range)
        .map_err(|error| format!("`{requirement}`: {error}"))?;
    if specifiers.contains(uv) {
        Ok(BuildBackend::BundledUvBuild)
    } else {
        Err(format!(
            "`{requirement}` leaves out uv {uv}, so `uv build` would download a uv_build from \
             PyPI instead of using its own copy. Widen the range to hold {uv}, or build with a uv \
             inside it"
        ))
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn build_backends() {
        let uv = pep440_rs::Version::from_str("0.13.6").unwrap();
        let backend = |build_system: &str| {
            let document: DocumentMut = format!("[project]\nname = \"frc\"\n{build_system}")
                .parse()
                .unwrap();
            build_backend(&document, &uv)
        };
        for bundled in [
            "[build-system]\nrequires = [\"uv_build >= 0.13.5, < 0.14\"]\nbuild-backend = \"uv_build\"",
            "[build-system]\nrequires = [\"uv-build>=0.13,<0.14\"]\nbuild-backend = \"uv_build\"",
            "[build-system]\nrequires = [\"UV_Build==0.13.6\"]\nbuild-backend = \"uv_build\"",
            "[build-system]\nrequires = [\"uv_build\"]\nbuild-backend = \"uv_build\"",
        ] {
            assert_eq!(
                backend(bundled),
                Ok(BuildBackend::BundledUvBuild),
                "{bundled}"
            );
        }
        assert_eq!(
            backend(
                "[build-system]\nrequires = [\"maturin>=1.9,<2\"]\nbuild-backend = \"maturin\""
            ),
            Ok(BuildBackend::Downloaded("maturin".to_owned()))
        );
        assert!(matches!(backend(""), Ok(BuildBackend::Downloaded(_))));
        for (refused, expected) in [
            (
                "requires = [\"uv_build>=0.12,<0.13\"]",
                "leaves out uv 0.13.6",
            ),
            ("requires = [\"uv_build==0.13.5\"]", "leaves out uv 0.13.6"),
            (
                "requires = [\"uv_build>=0.13,<0.14\", \"wheel\"]",
                "a single",
            ),
            (
                "requires = [\"uv_builder>=0.13\"]",
                "not `uv_builder>=0.13`",
            ),
            (
                "requires = [\"uv_build>=0.13; python_version >= '3.9'\"]",
                "markers",
            ),
            ("requires = [\"uv_build>=banana\"]", "uv_build>=banana"),
        ] {
            let error = backend(&format!(
                "[build-system]\n{refused}\nbuild-backend = \"uv_build\""
            ))
            .unwrap_err();
            assert!(error.contains(expected), "{refused}: {error}");
        }
    }
}
