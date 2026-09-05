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
    /// Where `<name>.in` is, when the package ships one.
    ///
    /// **Nothing opens this path.** The pipeline rebuilds it from `name` when it
    /// mounts one file into the submission's container, which is what lets that
    /// rule be asserted without a container at all. What this field answers is
    /// *is there one* — and only an interactive problem may say no, because
    /// everywhere else the submission reads it.
    pub input: Option<PathBuf>,
    /// Where `<name>.out` is, when the package ships one.
    ///
    /// Absent only where something else decides the verdict: a checker is handed
    /// the path whether or not the file is behind it, and an interactor knows
    /// the answer by construction.
    pub expected: Option<PathBuf>,
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
    ///
    /// **The census is the union of two sources**, and it is keyed by name. A
    /// name is a test if `<name>.in` exists, or `<name>.out` does, or a group
    /// declares a count that reaches it. Keyed, because the same name arriving
    /// from two of those is one test: pushed twice it would double
    /// [`TestSet::in_group`], which is the divisor for a test's share of the
    /// group's points — every group would quietly score half.
    ///
    /// **Which of the two files a test needs depends on what judges it**, and on
    /// nothing else. With neither a checker nor an interactor the `.out` file is
    /// the whole verdict and the `.in` is what the program reads, so both are
    /// required. A checker replaces the comparison, so `.out` becomes the
    /// author's choice. An interactor replaces the input as well — the
    /// submission is handed no file at all — so both do.
    ///
    /// `output-only@1` shares this reader and is untouched by any of it: it
    /// declares no judge, so it lands in the first case and still needs both.
    pub fn read(root: &Path, config: &Config) -> Result<Self> {
        let directory = root.join("tests");

        // What the configuration names, before anything is looked at.
        let mut named: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for group in &config.groups {
            for index in 0..group.tests.unwrap_or(0) {
                named.insert(format!("{}{}", group.group, (b'a' + index as u8) as char));
            }
        }

        // What is on disk. `(has .in, has .out)` per name.
        let mut found: std::collections::BTreeMap<String, (bool, bool)> =
            std::collections::BTreeMap::new();
        if directory.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(&directory)?
                .flatten()
                .map(|e| e.path())
                .collect();
            entries.sort();

            for path in entries {
                let extension = path.extension().and_then(|e| e.to_str());
                let Some(extension) = extension.filter(|e| *e == "in" || *e == "out") else {
                    continue;
                };
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .ok_or_else(|| Error::invalid("a test file has an unreadable name"))?
                    .to_owned();

                let (group, _) = split(&name)?;
                let Some(declared) = config.group(group) else {
                    return Err(Error::invalid(format!(
                        "test {name} is in group {group}, which config.yml does not declare"
                    )));
                };

                // **A file outside a declared count is refused, not ignored.**
                // An ignored test is one the author believes is running while
                // the divisor leaves it out, and the score says nothing.
                if let Some(count) = declared.tests {
                    if !named.contains(&name) {
                        return Err(Error::invalid(format!(
                            "tests/{name}.{extension} is in group {group}, which declares \
                             {count} tests — {name} is not one of them"
                        )));
                    }
                }

                let entry = found.entry(name).or_default();
                if extension == "in" {
                    entry.0 = true;
                } else {
                    entry.1 = true;
                }
            }
        } else if named.is_empty() {
            return Err(Error::invalid("the package has no tests/ directory"));
        }

        let mut tests = Vec::new();
        for name in named
            .iter()
            .cloned()
            .chain(found.keys().cloned())
            .collect::<std::collections::BTreeSet<String>>()
        {
            let (group, letter) = split(&name)?;
            if config.group(group).is_none() {
                return Err(Error::invalid(format!(
                    "test {name} is in group {group}, which config.yml does not declare"
                )));
            }

            let (has_input, has_expected) = found.get(&name).copied().unwrap_or((false, false));
            let input = has_input.then(|| directory.join(format!("{name}.in")));
            let expected = has_expected.then(|| directory.join(format!("{name}.out")));

            if config.interactor.is_none() {
                if input.is_none() {
                    return Err(Error::invalid(format!(
                        "test {name} has no {name}.in; only an interactive problem judges \
                         without one, because only there is the input something the \
                         submission is not given"
                    )));
                }
                if config.checker.is_none() && expected.is_none() {
                    return Err(Error::invalid(format!(
                        "test {name} has no {name}.out; with no checker and no interactor \
                         the file is what decides the verdict"
                    )));
                }
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
type: "standard-io@1"
limits:
  timeMs: 1000
  memoryBytes: 268435456
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

    /// A judging program, appended to [`CONFIG`].
    fn judged_by(what: &str) -> Config {
        Config::parse(&format!(
            "{CONFIG}{what}:\n  source: judge/judge.cpp\n  language: cpp\n"
        ))
        .unwrap()
    }

    /// **With nothing else to decide, the file is the verdict.**
    #[test]
    fn a_test_without_expected_output_is_refused_when_nothing_else_decides() {
        let root = package(
            "no-out",
            &[
                ("tests/0a.in", "a"),
                ("tests/0a.out", "A"),
                ("tests/1a.in", "a"),
            ],
        );

        let error = TestSet::read(&root, &Config::parse(CONFIG).unwrap()).unwrap_err();
        assert!(matches!(error, Error::Invalid(_)), "got {error}");
    }

    /// **A checker replaces the comparison, so the file it replaced is the
    /// author's choice.** It is still handed the path as `argv[3]`; whether
    /// anything is behind it is between the author and their own checker.
    #[test]
    fn a_checker_makes_the_expected_output_optional() {
        let root = package(
            "checker-no-out",
            &[
                ("tests/0a.in", "a"),
                ("tests/0a.out", "A"),
                ("tests/1a.in", "a"),
            ],
        );

        let set = TestSet::read(&root, &judged_by("checker")).expect("a checker package");
        let one = set.iter().find(|t| t.name == "1a").expect("1a");
        assert!(one.input.is_some(), "the submission still reads its input");
        assert!(
            one.expected.is_none(),
            "and nothing else was invented for it"
        );
    }

    /// **An interactor replaces the input as well**, because the submission is
    /// handed no file at all — so a test may have neither.
    #[test]
    fn an_interactor_makes_both_files_optional() {
        let root = package(
            "interactor-bare",
            &[("tests/0a.in", "a"), ("tests/1a.out", "A")],
        );

        let set = TestSet::read(&root, &judged_by("interactor")).expect("an interactive package");
        let names: Vec<&str> = set.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["0a", "1a"], "either file names a test");

        let zero = set.iter().find(|t| t.name == "0a").unwrap();
        assert!(zero.input.is_some() && zero.expected.is_none());
        let one = set.iter().find(|t| t.name == "1a").unwrap();
        assert!(one.input.is_none() && one.expected.is_some());
    }

    /// **A count is a census of its own**, for the problem where an interactor
    /// is the whole of a test and there is no file to enumerate.
    #[test]
    fn a_declared_count_names_tests_that_have_no_files() {
        let root = package("counted", &[]);
        let config = Config::parse(
            "type: \"standard-io@1\"\n\
             limits:\n  timeMs: 1000\n  memoryBytes: 268435456\n\
             interactor:\n  source: judge/judge.cpp\n  language: cpp\n\
             groups:\n  - group: 1\n    points: 100\n    tests: 3\n",
        )
        .unwrap();

        let set = TestSet::read(&root, &config).expect("a counted package");
        let names: Vec<&str> = set.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["1a", "1b", "1c"]);
        assert_eq!(
            set.in_group(1),
            3,
            "the divisor for a test's share of the points comes from here",
        );
        assert!(set
            .iter()
            .all(|t| t.input.is_none() && t.expected.is_none()));
    }

    /// **A file outside a declared count is refused, not ignored.** An ignored
    /// test is one the author believes is running while the divisor leaves it
    /// out, and nothing anywhere says so.
    #[test]
    fn a_file_naming_a_test_outside_the_count_is_refused() {
        let root = package("counted-stray", &[("tests/1d.in", "a")]);
        let config = Config::parse(
            "type: \"standard-io@1\"\n\
             limits:\n  timeMs: 1000\n  memoryBytes: 268435456\n\
             interactor:\n  source: judge/judge.cpp\n  language: cpp\n\
             groups:\n  - group: 1\n    points: 100\n    tests: 3\n",
        )
        .unwrap();

        let error = TestSet::read(&root, &config).unwrap_err();
        assert!(error.to_string().contains("not one of them"), "got {error}");
    }

    /// **A count and files together, which is the case the count is for.**
    ///
    /// An author seeds some tests from `.in` — a guessing problem's interactor
    /// reads `argv[1]` to learn its secret — and leaves the rest to the
    /// interactor to invent. So a name arrives from the count *and* from disk,
    /// and that is the only way the same name can be built twice.
    ///
    /// It is also the only shape that catches an unkeyed union: with no count,
    /// the map on disk is already unique.
    #[test]
    fn a_counted_group_may_also_ship_files_for_some_of_its_tests() {
        let root = package("counted-and-seeded", &[("tests/1a.in", "a")]);
        let config = Config::parse(
            "type: \"standard-io@1\"\n\
             limits:\n  timeMs: 1000\n  memoryBytes: 268435456\n\
             interactor:\n  source: judge/judge.cpp\n  language: cpp\n\
             groups:\n  - group: 1\n    points: 100\n    tests: 2\n",
        )
        .unwrap();

        let set = TestSet::read(&root, &config).expect("a seeded interactive package");
        let names: Vec<&str> = set.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["1a", "1b"], "1a came from both and is one test");
        assert_eq!(
            set.in_group(1),
            2,
            "counted twice, every test in this group would be worth half its share",
        );

        let seeded = set.iter().find(|t| t.name == "1a").unwrap();
        assert!(seeded.input.is_some(), "the file the author did ship");
        let invented = set.iter().find(|t| t.name == "1b").unwrap();
        assert!(invented.input.is_none() && invented.expected.is_none());
    }

    /// **A name arriving from two sources is one test.** Pushed twice it would
    /// double `in_group`, which is the divisor at `score.rs` — every group would
    /// quietly score half its points, while still reading as all passed.
    #[test]
    fn a_paired_test_is_counted_once_now_that_both_extensions_are_read() {
        let root = package(
            "paired",
            &[
                ("tests/0a.in", "a"),
                ("tests/0a.out", "A"),
                ("tests/1a.in", "a"),
                ("tests/1a.out", "A"),
            ],
        );

        let set = TestSet::read(&root, &Config::parse(CONFIG).unwrap()).expect("a plain package");
        assert_eq!(set.len(), 2);
        assert_eq!(set.in_group(1), 1);
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
