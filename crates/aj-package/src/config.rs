//! `config.yml`, as `docs/specs/PACKAGE_FORMAT.md` defines it.
//!
//! YAML rather than JSON for one reason the specification states plainly: it is
//! the file a problem author edits by hand, and a comment saying why group 3 has
//! a longer limit survives in the package, where the same note in JSON cannot
//! exist at all.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::error::{Error, Result};

pub const FORMAT: &str = "standard-io";
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub format: String,
    pub version: u32,

    pub limits: Limits,

    /// Per language, keyed by the product's language id. Python is slower; this
    /// is where that is said.
    #[serde(default)]
    pub override_limits: BTreeMap<String, PartialLimits>,

    pub groups: Vec<Group>,

    /// Absent means the `.out` files decide.
    #[serde(default)]
    pub checker: Option<Source>,

    /// Used for calibration, never for judging.
    #[serde(default)]
    pub model_solution: Option<Source>,

    #[serde(default)]
    pub calibration: Option<serde_yaml_ng::Value>,

    #[serde(default)]
    pub extra_compilation_files: Vec<String>,
}

/// Milliseconds and **kibibytes**, as `sinolpack` has them — so importing one is
/// a copy rather than a division with a rounding rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    pub time_ms: u64,
    pub memory_kib: u64,
}

/// One field, the other, or both — a group may state either alone.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PartialLimits {
    #[serde(default)]
    pub time_ms: Option<u64>,
    #[serde(default)]
    pub memory_kib: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Group {
    pub group: u32,
    pub points: u32,
    /// Group 0 is the examples. The flag has always existed separately, even
    /// though nothing has yet wanted the two properties apart.
    #[serde(default)]
    pub examples: bool,
    /// **Replaces rather than caps.** A group of larger tests legitimately needs
    /// more time than the rest, and saying so per group beats raising the
    /// ceiling for every test in the problem.
    #[serde(default)]
    pub limits: Option<PartialLimits>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Source {
    pub source: String,
    pub language: String,
}

impl Config {
    pub fn parse(yaml: &str) -> Result<Self> {
        Config::parse_as(yaml, FORMAT)
    }

    /// The same shape, under another problem type's discriminator.
    ///
    /// The *shape* — limits, groups, tests — is shared between problem types;
    /// the *name* is not. Splitting them here was the first thing adding a
    /// second type required, and it is a change inside one crate rather than a
    /// second parser.
    pub fn parse_as(yaml: &str, format: &str) -> Result<Self> {
        let config: Config = serde_yaml_ng::from_str(yaml)?;

        // A Runner that does not know the version refuses the package rather
        // than guessing at what changed in it.
        if config.format != format || config.version != VERSION {
            return Err(Error::UnknownFormat {
                format: config.format,
                version: config.version,
            });
        }

        config.validated()
    }

    fn validated(self) -> Result<Self> {
        check(self.limits.time_ms, "limits.timeMs")?;
        check(self.limits.memory_kib, "limits.memoryKib")?;

        if self.groups.is_empty() {
            return Err(Error::invalid("a package with no groups scores nothing"));
        }

        let mut seen = std::collections::BTreeSet::new();
        for group in &self.groups {
            if !seen.insert(group.group) {
                return Err(Error::invalid(format!(
                    "group {} is declared twice, and which one wins is not something \
                     this Runner should decide",
                    group.group
                )));
            }
            if let Some(limits) = group.limits {
                partial(&limits, &format!("groups[{}].limits", group.group))?;
            }
        }

        for (language, limits) in &self.override_limits {
            partial(limits, &format!("overrideLimits.{language}"))?;
        }

        Ok(self)
    }

    /// The limits a test in this group, in this language, actually runs under.
    ///
    /// **Open question — the specification does not settle this.** It gives two
    /// axes, per group and per language, and says each *replaces*, but never
    /// says which wins when both apply. The reading here is **most specific
    /// first**: a group's own limits beat a language override, which beats the
    /// global ones.
    ///
    /// The alternative reading is that a language override should compose with
    /// a group's — a group needing three seconds because its tests are large,
    /// in a language that needs three times as long, arguably wants nine. That
    /// cannot be expressed at all while the override holds absolute values, so
    /// it would be a format change rather than a different resolution here.
    ///
    /// Whichever way it is settled, it is this function and nothing else.
    pub fn effective(&self, group: u32, language: &str) -> Limits {
        let mut limits = self.limits;

        if let Some(over) = self.override_limits.get(language) {
            apply(&mut limits, over);
        }
        if let Some(over) = self
            .groups
            .iter()
            .find(|g| g.group == group)
            .and_then(|g| g.limits)
        {
            apply(&mut limits, &over);
        }

        limits
    }

    pub fn group(&self, group: u32) -> Option<&Group> {
        self.groups.iter().find(|g| g.group == group)
    }

    /// What the package is worth in total — the Runner's own scale, before the
    /// assignment's rescaling. Group 0 contributes zero and is included anyway,
    /// because it is judged like any other.
    pub fn max_score(&self) -> u32 {
        self.groups.iter().map(|g| g.points).sum()
    }
}

fn apply(limits: &mut Limits, over: &PartialLimits) {
    if let Some(time) = over.time_ms {
        limits.time_ms = time;
    }
    if let Some(memory) = over.memory_kib {
        limits.memory_kib = memory;
    }
}

/// Zero is not "inherit" — it is a limit nothing can pass.
fn check(value: u64, what: &str) -> Result<()> {
    if value == 0 {
        Err(Error::invalid(format!(
            "{what} is 0, which no solution can pass; remove the field to inherit"
        )))
    } else {
        Ok(())
    }
}

fn partial(limits: &PartialLimits, what: &str) -> Result<()> {
    if let Some(time) = limits.time_ms {
        check(time, &format!("{what}.timeMs"))?;
    }
    if let Some(memory) = limits.memory_kib {
        check(memory, &format!("{what}.memoryKib"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The specification's own example, so this test fails if the document and
    /// the reader ever part company.
    const FROM_THE_SPECIFICATION: &str = r#"
format: standard-io
version: 1

limits:
  timeMs: 1000
  memoryKib: 262144

overrideLimits:
  python:
    timeMs: 3000
    memoryKib: 524288

groups:
  - group: 0
    points: 0
    examples: true
  - group: 1
    points: 30
  - group: 2
    points: 30
    limits:
      timeMs: 2000
  - group: 3
    points: 40

checker:
  source: checker/checker.cpp
  language: cpp

modelSolution:
  source: solutions/model.cpp
  language: cpp

calibration:
  time:   { factor: 3, add: 0, roundTo: 100 }
  memory: { factor: 1, add: 16384, roundTo: 1024 }

extraCompilationFiles: []
"#;

    #[test]
    fn the_specifications_own_example_reads() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();

        assert_eq!(config.limits.time_ms, 1000);
        assert_eq!(config.limits.memory_kib, 262144);
        assert_eq!(config.groups.len(), 4);
        assert_eq!(config.max_score(), 100);
        assert!(config.group(0).unwrap().examples);
        assert_eq!(config.group(0).unwrap().points, 0);
        assert_eq!(config.checker.as_ref().unwrap().language, "cpp");
        assert_eq!(
            config.model_solution.as_ref().unwrap().source,
            "solutions/model.cpp"
        );
    }

    #[test]
    fn a_group_limit_replaces_only_the_field_it_states() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();

        // Group 2 states time and not memory, so memory stays global.
        let group2 = config.effective(2, "cpp");
        assert_eq!(group2.time_ms, 2000);
        assert_eq!(group2.memory_kib, 262144);

        let group1 = config.effective(1, "cpp");
        assert_eq!(group1.time_ms, 1000);
    }

    #[test]
    fn a_language_override_applies_where_a_group_says_nothing() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();

        let python = config.effective(1, "python");
        assert_eq!(python.time_ms, 3000);
        assert_eq!(python.memory_kib, 524288);
    }

    /// Pins the reading documented on `effective`: most specific first. If this
    /// test is ever changed, the open question it stands for was answered.
    #[test]
    fn a_group_limit_beats_a_language_override() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();

        let python = config.effective(2, "python");
        assert_eq!(python.time_ms, 2000, "the group's own limit wins");
        assert_eq!(
            python.memory_kib, 524288,
            "and the override still fills the rest"
        );
    }

    #[test]
    fn a_format_this_runner_does_not_know_is_refused() {
        let yaml = FROM_THE_SPECIFICATION.replace("version: 1", "version: 2");
        let error = Config::parse(&yaml).unwrap_err();
        assert!(
            matches!(error, Error::UnknownFormat { version: 2, .. }),
            "got {error}"
        );

        let yaml = FROM_THE_SPECIFICATION.replace("format: standard-io", "format: interactive");
        assert!(matches!(
            Config::parse(&yaml).unwrap_err(),
            Error::UnknownFormat { .. }
        ));
    }

    #[test]
    fn a_zero_limit_is_refused_rather_than_read_as_inherit() {
        let yaml = FROM_THE_SPECIFICATION.replace("timeMs: 2000", "timeMs: 0");
        let error = Config::parse(&yaml).unwrap_err();
        assert!(matches!(error, Error::Invalid(_)), "got {error}");
    }

    #[test]
    fn a_group_declared_twice_is_refused() {
        let yaml = FROM_THE_SPECIFICATION.replace(
            "  - group: 3\n    points: 40",
            "  - group: 1\n    points: 40",
        );
        let error = Config::parse(&yaml).unwrap_err();
        assert!(matches!(error, Error::Invalid(_)), "got {error}");
    }

    /// An unknown field is a typo, and a typo in a limit is a limit that
    /// silently did not apply.
    #[test]
    fn a_misspelled_field_is_refused_rather_than_ignored() {
        let yaml = FROM_THE_SPECIFICATION.replace("memoryKib: 262144", "memoryKiB: 262144");
        let error = Config::parse(&yaml).unwrap_err();
        assert!(matches!(error, Error::Malformed(_)), "got {error}");
    }
}
