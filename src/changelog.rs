//! The shape of a hand-written `CHANGELOG.md`, and the edits a release makes
//! to it.

use crate::version::{Date, Version};

/// The headings a section may divide its entries under, from Keep a
/// Changelog.
const SUBSECTIONS: [&str; 6] = [
    "Added",
    "Changed",
    "Deprecated",
    "Removed",
    "Fixed",
    "Security",
];

/// A second-level heading of the changelog.
#[derive(Debug, PartialEq)]
pub enum Heading {
    Unreleased,
    Release { version: Version, date: Date },
}

/// A second-level section of the changelog.
#[derive(Debug)]
pub struct Section {
    pub heading: Heading,
    /// The index of the heading line.
    line: usize,
    /// The index one past the last line of the section.
    end: usize,
}

/// A changelog whose shape [`Changelog::parse`] checked.
pub struct Changelog<'a> {
    lines: Vec<&'a str>,
    /// From newest to oldest.
    pub sections: Vec<Section>,
}

impl<'a> Changelog<'a> {
    /// Checks the shape of the changelog.
    ///
    /// The changelog opens with `# Changelog`. Each section is headed
    /// `## Unreleased`, which may only come first, or `## X.Y.Z - YYYY-MM-DD`,
    /// with versions falling and dates not rising down the file. A section may
    /// divide its entries under the `###` headings of Keep a Changelog. No
    /// heading is empty. Each paragraph and list item is one line, so a line of
    /// text that is not a list item follows a blank line or a heading.
    pub fn parse(text: &'a str) -> Result<Self, Vec<String>> {
        let lines: Vec<&str> = text.lines().collect();
        let mut errors = Vec::new();
        let mut sections: Vec<Section> = Vec::new();
        let mut error = |index: usize, message: String| {
            errors.push(format!("CHANGELOG.md:{}: {message}", index + 1));
        };

        if lines.first() != Some(&"# Changelog") {
            error(0, "the first line is not `# Changelog`".to_owned());
        }
        let mut in_code = false;
        for (index, line) in lines.iter().enumerate().skip(1) {
            if line.trim_start().starts_with("```") {
                in_code = !in_code;
                continue;
            }
            if in_code {
                continue;
            }
            let previous = lines[index - 1];
            if let Some(title) = line.strip_prefix("## ") {
                let heading = if title == "Unreleased" {
                    if !sections.is_empty() {
                        error(index, "`## Unreleased` is not the first section".to_owned());
                    }
                    Heading::Unreleased
                } else {
                    match parse_release(title) {
                        Ok(heading) => heading,
                        Err(message) => {
                            error(index, message);
                            continue;
                        }
                    }
                };
                if let (
                    Heading::Release { version, date },
                    Some(Section {
                        heading:
                            Heading::Release {
                                version: newer,
                                date: newer_date,
                            },
                        ..
                    }),
                ) = (&heading, sections.last())
                {
                    if version >= newer {
                        error(index, format!("{version} is listed below {newer}"));
                    }
                    if date > newer_date {
                        error(index, format!("{date} is later than the release above it"));
                    }
                }
                if !has_content(&lines[index + 1..], 2) {
                    error(index, format!("`## {title}` is empty"));
                }
                if let Some(last) = sections.last_mut() {
                    last.end = index;
                }
                sections.push(Section {
                    heading,
                    line: index,
                    end: lines.len(),
                });
            } else if let Some(title) = line.strip_prefix("### ") {
                if sections.is_empty() {
                    error(index, format!("`### {title}` is outside a section"));
                }
                if !SUBSECTIONS.contains(&title) {
                    error(
                        index,
                        format!("`### {title}` is not one of {}", SUBSECTIONS.join(", ")),
                    );
                }
                if !has_content(&lines[index + 1..], 3) {
                    error(index, format!("`### {title}` is empty"));
                }
            } else if line.starts_with('#') {
                error(
                    index,
                    "only `##` and `###` headings may follow the title".to_owned(),
                );
            } else if !line.trim().is_empty()
                && !line.trim_start().starts_with("- ")
                && !previous.trim().is_empty()
                && !previous.starts_with('#')
            {
                error(
                    index,
                    "this continues the line above. Write each paragraph and list item on one line"
                        .to_owned(),
                );
            }
        }
        if errors.is_empty() {
            Ok(Self { lines, sections })
        } else {
            Err(errors)
        }
    }

    /// The newest released version.
    pub fn newest_release(&self) -> Option<(&Version, Date)> {
        self.sections
            .iter()
            .find_map(|section| match &section.heading {
                Heading::Unreleased => None,
                Heading::Release { version, date } => Some((version, *date)),
            })
    }

    /// The text with `## Unreleased` turned into the section of `version`.
    pub fn release(&self, version: &Version, date: Date) -> Result<String, String> {
        let unreleased = self
            .sections
            .first()
            .filter(|section| section.heading == Heading::Unreleased)
            .ok_or("CHANGELOG.md: there is no `## Unreleased` section to release")?;
        if let Some((newest, newest_date)) = self.newest_release() {
            if version <= newest {
                return Err(format!("{version} is not newer than {newest}"));
            }
            if date < newest_date {
                return Err(format!("{date} is before {newest}, released {newest_date}"));
            }
        }
        let heading = format!("## {version} - {date}");
        let mut text = String::new();
        for (index, line) in self.lines.iter().enumerate() {
            text.push_str(if index == unreleased.line {
                &heading
            } else {
                line
            });
            text.push('\n');
        }
        Ok(text)
    }

    /// The body of the section of `version`, without its heading, for the
    /// notes of a release.
    pub fn notes(&self, version: &Version) -> Option<String> {
        let section = self.sections.iter().find(|section| {
            matches!(&section.heading, Heading::Release { version: v, .. } if v == version)
        })?;
        let body = self.lines[section.line + 1..section.end].join("\n");
        Some(body.trim().to_owned() + "\n")
    }
}

/// The non-blank lines under `## Unreleased`, without subsection headings.
/// This reads any text, since the base of a pull request may predate the rules
/// that [`Changelog::parse`] enforces.
pub fn unreleased_entries(text: &str) -> Vec<&str> {
    text.lines()
        .skip_while(|line| *line != "## Unreleased")
        .skip(1)
        .take_while(|line| !line.starts_with("## "))
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .collect()
}

/// Whether text comes before the next heading of `level` or above.
fn has_content(rest: &[&str], level: usize) -> bool {
    for line in rest {
        let hashes = line.len() - line.trim_start_matches('#').len();
        if hashes > 0 && line[hashes..].starts_with(' ') {
            if hashes <= level {
                return false;
            }
            if level == 2 {
                // A subsection counts, since its own heading is checked for
                // content.
                return true;
            }
        } else if !line.trim().is_empty() {
            return true;
        }
    }
    false
}

/// Reads the title of a released section, `X.Y.Z - YYYY-MM-DD`.
fn parse_release(title: &str) -> Result<Heading, String> {
    let (version, date) = title
        .split_once(" - ")
        .ok_or_else(|| format!("`## {title}` is not `## Unreleased` or `## X.Y.Z - YYYY-MM-DD`"))?;
    Ok(Heading::Release {
        version: Version::parse(version)?,
        date: Date::parse(date)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "# Changelog\n\nIntro.\n\n## Unreleased\n\n### Added\n\n- One.\n  - Nested.\n\n\
                         ## 0.0.2 - 2026-09-26\n\nProse.\n\n## 0.0.1 - 2026-09-07\n\n- Two.\n";

    #[test]
    fn rules() {
        assert_eq!(Changelog::parse(VALID).unwrap().sections.len(), 3);
        for (broken, expected) in [
            (
                VALID.replace("- One.\n", "- One\n  wrapped.\n"),
                "continues",
            ),
            (VALID.replace("### Added", "### Misc"), "not one of"),
            (VALID.replace("- One.\n  - Nested.\n", ""), "is empty"),
            (
                VALID.replace("0.0.1 - 2026-09-07", "0.0.3 - 2026-09-07"),
                "listed below",
            ),
            (VALID.replace("2026-09-07", "2026-09-30"), "later than"),
        ] {
            let errors = Changelog::parse(&broken).err().unwrap();
            assert!(
                errors.iter().any(|e| e.contains(expected)),
                "{expected}: {errors:?}"
            );
        }
    }

    #[test]
    fn release_and_notes() {
        let version = Version::parse("0.1.0").unwrap();
        let date = Date::parse("2026-10-04").unwrap();
        let released = Changelog::parse(VALID)
            .unwrap()
            .release(&version, date)
            .unwrap();
        let changelog = Changelog::parse(&released).unwrap();
        assert_eq!(changelog.newest_release(), Some((&version, date)));
        assert_eq!(
            changelog.notes(&version).unwrap(),
            "### Added\n\n- One.\n  - Nested.\n"
        );
        assert!(unreleased_entries(&released).is_empty());
        assert_eq!(unreleased_entries(VALID), ["- One.", "  - Nested."]);
    }
}
