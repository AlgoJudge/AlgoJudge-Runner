//! The tests, and which group each belongs to.
//!
//! A test is named `{group}{letter}`: `2a` is the first test of group 2. Groups
//! are integers from 0 and letters run `a`, `b`, …; a group of one test is still
//! `1a`. The file name carries no problem short name — `sinolpack` writes
//! `squ1a.in`, and tying every file name to a name that can be changed means
//! renaming a problem rewrites its package.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Test {
    /// `1a`, `2c`. What the result document calls `no`.
    pub name: String,
    pub group: u32,
    /// `a`, `b`, … — kept for ordering rather than for display.
    pub letter: String,
    pub input: PathBuf,
    pub expected: PathBuf,
}

#[derive(Debug, Clone)]
pub struct TestSet {
    tests: Vec<Test>,
}

impl TestSet {
    /// Reads `tests/` under an unpacked package.
    ///
    /// Every test is checked against the configuration as it is read: a test in
    /// a group the config does not declare is a package error, not a test worth
    /// zero. Silently scoring it would make a mistyped group number look like a
    /// failing solution.
    pub fn read(root: &Path, config: &Config) -> Result<Self> {
        let directory = root.join("tests");
        if !directory.is_dir() {
            return Err(Error::invalid("the package has no tests/ directory"));
        }

        let mut tests = Vec::new();
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&directory)?
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort();

        for input in entries {
            if input.extension().and_then(|e| e.to_str()) != Some("in") {
                continue;
            }
            let name = input
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| Error::invalid("a test file has an unreadable name"))?
                .to_owned();

            let (group, letter) = split(&name)?;

            if config.group(group).is_none() {
                return Err(Error::invalid(format!(
                    "test {name} is in group {group}, which config.yml does not declare"
                )));
            }

            let expected = directory.join(format!("{name}.out"));
            if !expected.is_file() {
                return Err(Error::invalid(format!(
                    "test {name} has no {name}.out; a test with no expected output \
                     cannot be judged, and a checker replaces the comparison, not the file"
                )));
            }

            tests.push(Test {
                name,
                group,
                letter,
                input,
                expected,
            });
        }

        if tests.is_empty() {
            return Err(Error::invalid("the package has no tests"));
        }

        // Group first, then letter, so a result document reads in the order a
        // person expects and two Runners produce the same order.
        tests.sort_by(|a, b| a.group.cmp(&b.group).then_with(|| a.letter.cmp(&b.letter)));

        // A group that is declared and has no tests would silently award its
        // points, since "every test passed" is vacuously true of none.
        for group in &config.groups {
            if !tests.iter().any(|t| t.group == group.group) {
                return Err(Error::invalid(format!(
                    "group {} is declared with {} points and has no tests; every test \
                     in it passing would be vacuously true",
                    group.group, group.points
                )));
            }
        }

        Ok(Self { tests })
    }

    pub fn iter(&self) -> impl Iterator<Item = &Test> {
        self.tests.iter()
    }

    pub fn len(&self) -> usize {
        self.tests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tests.is_empty()
    }

    /// How many tests are in a group — the divisor for a test's share of the
    /// group's points.
    pub fn in_group(&self, group: u32) -> usize {
        self.tests.iter().filter(|t| t.group == group).count()
    }
}

/// `2a` → `(2, "a")`.
fn split(name: &str) -> Result<(u32, String)> {
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    let letter: String = name.chars().skip(digits.len()).collect();

    if digits.is_empty() || letter.is_empty() || !letter.chars().all(|c| c.is_ascii_lowercase()) {
        return Err(Error::invalid(format!(
            "{name} is not a test name; they are a group number then lower-case \
             letters, as in 1a or 12ab"
        )));
    }

    let group = digits
        .parse()
        .map_err(|_| Error::invalid(format!("{name} has a group number that will not fit")))?;

    Ok((group, letter))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
format: standard-io
version: 1
limits:
  timeMs: 1000
  memoryKib: 262144
groups:
  - group: 0
    points: 0
    examples: true
  - group: 1
    points: 100
"#;

    fn package(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let mut root = std::env::temp_dir();
        root.push(format!("aj-tests-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("tests")).unwrap();
        for (path, body) in files {
            std::fs::write(root.join(path), body).unwrap();
        }
        root
    }

    #[test]
    fn tests_are_read_in_group_then_letter_order() {
        let root = package(
            "order",
            &[
                ("tests/1b.in", "b"),
                ("tests/1b.out", "B"),
                ("tests/0a.in", "a"),
                ("tests/0a.out", "A"),
                ("tests/1a.in", "a"),
                ("tests/1a.out", "A"),
            ],
        );
        let config = Config::parse(CONFIG).unwrap();

        let set = TestSet::read(&root, &config).unwrap();

        let names: Vec<&str> = set.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["0a", "1a", "1b"]);
        assert_eq!(set.in_group(1), 2);
    }

    #[test]
    fn a_test_without_expected_output_is_refused() {
        let root = package(
            "no-out",
            &[
                ("tests/0a.in", "a"),
                ("tests/0a.out", "A"),
                ("tests/1a.in", "a"),
            ],
        );
        let config = Config::parse(CONFIG).unwrap();

        let error = TestSet::read(&root, &config).unwrap_err();
        assert!(matches!(error, Error::Invalid(_)), "got {error}");
    }

    #[test]
    fn a_test_in_an_undeclared_group_is_refused() {
        let root = package(
            "stray",
            &[
                ("tests/0a.in", "a"),
                ("tests/0a.out", "A"),
                ("tests/1a.in", "a"),
                ("tests/1a.out", "A"),
                ("tests/7a.in", "a"),
                ("tests/7a.out", "A"),
            ],
        );
        let config = Config::parse(CONFIG).unwrap();

        let error = TestSet::read(&root, &config).unwrap_err();
        assert!(matches!(error, Error::Invalid(_)), "got {error}");
    }

    /// An empty group would award its points for free, because "every test in
    /// it passed" is vacuously true of no tests at all.
    #[test]
    fn a_declared_group_with_no_tests_is_refused() {
        let root = package(
            "empty-group",
            &[("tests/0a.in", "a"), ("tests/0a.out", "A")],
        );
        let config = Config::parse(CONFIG).unwrap();

        let error = TestSet::read(&root, &config).unwrap_err();
        assert!(matches!(error, Error::Invalid(_)), "got {error}");
    }

    #[test]
    fn a_name_that_is_not_group_then_letters_is_refused() {
        for bad in ["a1", "1", "a", "1A", "1a2"] {
            assert!(split(bad).is_err(), "{bad} should not parse");
        }
        assert_eq!(split("12ab").unwrap(), (12, "ab".to_owned()));
    }
}
