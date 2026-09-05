//! `config.yml`, as `docs/specs/PACKAGE_FORMAT.md` defines it.
//!
//! YAML rather than JSON for one reason the specification states plainly: it is
//! the file a problem author edits by hand, and a comment saying why group 3 has
//! a longer limit survives in the package, where the same note in JSON cannot
//! exist at all.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const FORMAT: &str = "standard-io";
pub const VERSION: u32 = 1;

/// The type this package is for, as **one string** — `standard-io@1`.
///
/// The envelope was decided as one string on 2026-08-02 and this file wrote it
/// as two fields until 2026-08-22. Four spellings of one idea existed in the
/// product; a convention with four spellings is not a convention.
pub fn envelope(format: &str) -> String {
    format!("{format}@{VERSION}")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// `standard-io@1`. **One string**, as every type discriminator in the
    /// product is (decided 2026-08-02, applied here 2026-08-22).
    ///
    /// Written by everything that builds a package. Absent only in a package
    /// built before that date, which still reads: see `format` and `version`
    /// below, which are the old spelling and are accepted rather than
    /// demanded — a package the Runner cannot parse is an infrastructure
    /// failure on every submission to it, not a message anybody can act on.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,

    /// **The old spelling, read and never written.** A package carrying these
    /// and no `type` is one built before 2026-08-22.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,

    pub limits: Limits,

    /// Per language, keyed by a **family** (`c`, `cpp`, `python`) or by one
    /// toolchain (`pypy3`). Python is slower; this is where that is said.
    ///
    /// A family covers every toolchain in it, which is what an author almost
    /// always means — writing sixteen entries to say "C++ gets the same" is not
    /// a format anybody would use. See `for_language`.
    #[serde(default)]
    pub override_limits: BTreeMap<String, PartialLimits>,

    /// Which toolchains may be submitted. **Empty means the package does not
    /// say**, and an assignment that says nothing allows everything this Runner
    /// can build.
    ///
    /// Ids as `language.rs` names them, so `cpp17-gcc` rather than `cpp17` —
    /// and the family shorthands `cpp` and `python` resolve too, because that is
    /// what every package written before the catalogue uses.
    ///
    /// A limit on *what may be sent* rather than on how it runs, which is why it
    /// is a list here rather than an entry in `overrideLimits`.
    #[serde(default)]
    pub languages: Vec<String>,

    pub groups: Vec<Group>,

    /// Absent means the `.out` files decide.
    #[serde(default)]
    pub checker: Option<Source>,

    /// The problem is interactive, and this program is the other side of it.
    ///
    /// **A field and not a second package type**, decided 2026-09-05. The format
    /// said interaction was out of scope, and that was a boundary drawn around a
    /// mechanism it did not have: judging held a submission's whole output and
    /// only then compared it, so nothing could answer a program while it ran.
    /// It can now — a judged run is wired to whatever is checking it by pipes,
    /// and an interactor is that same wiring with a second pipe pointing back.
    /// A package type of its own for one field would give an author two formats
    /// for one idea.
    ///
    /// It stands where `checker` stands, and it is refused beside one: the two
    /// decide the same question and a package that declares both has not said
    /// which of them judges.
    ///
    /// **`deny_unknown_fields` is what makes this safe to add.** A Runner too
    /// old to know the field refuses the package outright rather than judging an
    /// interactive problem as a batch one — which would fail every submission
    /// and look like the participants' doing.
    #[serde(default)]
    pub interactor: Option<Source>,

    /// Used for calibration, never for judging. The shorthand for one language.
    #[serde(default)]
    pub model_solution: Option<Source>,

    /// One per language. The same field as `model_solution`, written for more
    /// than one; a package states one of the two.
    #[serde(default)]
    pub model_solutions: Vec<Source>,

    #[serde(default)]
    pub calibration: Option<Calibration>,

    #[serde(default)]
    pub extra_compilation_files: Vec<String>,
}

/// Milliseconds and **kibibytes**, as `sinolpack` has them — so importing one is
/// a copy rather than a division with a rounding rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    pub time_ms: u64,
    pub memory_bytes: u64,
}

/// One field, the other, or both — a group may state either alone.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PartialLimits {
    #[serde(default)]
    pub time_ms: Option<u64>,
    #[serde(default)]
    pub memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

/// How a measurement of a model solution becomes a limit, and what was measured.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Calibration {
    #[serde(default)]
    pub time: Option<Rule>,
    #[serde(default)]
    pub memory: Option<Rule>,
    /// Written by a trial run: one row per group, and per language where more
    /// than one model solution was measured.
    #[serde(default)]
    pub measured: Vec<Measurement>,
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub runner: Option<String>,
}

/// `measured × factor + add`, rounded **up** to `round_to`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rule {
    #[serde(default = "one")]
    pub factor: f64,
    #[serde(default)]
    pub add: f64,
    #[serde(default)]
    pub round_to: f64,
}

fn one() -> f64 {
    1.0
}

impl Calibration {
    /// Time ×3 rounded to 100 ms, as the format states the default.
    pub fn time_rule(&self) -> Rule {
        self.time.unwrap_or(Rule {
            factor: 3.0,
            add: 0.0,
            round_to: 100.0,
        })
    }

    /// Memory **+16 MiB** rounded to 1 MiB. Headroom rather than a multiple,
    /// because what a weaker solution needs is room, not a proportion.
    pub fn memory_rule(&self) -> Rule {
        self.memory.unwrap_or(Rule {
            factor: 1.0,
            add: 16.0 * 1024.0 * 1024.0,
            round_to: 1024.0 * 1024.0,
        })
    }
}

impl Rule {
    /// Rounded **up**, because a limit landing below the measurement it came
    /// from fails the solution it was derived from.
    pub fn applied(&self, measured: f64) -> f64 {
        let scaled = measured * self.factor + self.add;
        if self.round_to > 0.0 {
            (scaled / self.round_to).ceil() * self.round_to
        } else {
            scaled.ceil()
        }
    }
}

/// What one model solution did on one group.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Measurement {
    pub group: u32,
    /// Absent means the package's own limits, for every language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub time_ms: u64,
    /// **Absent is not zero.** A Runner that cannot measure peak memory
    /// honestly reports nothing rather than a number that would be shown to a
    /// participant beside their verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
        let mut config: Config = serde_yaml_ng::from_str(yaml)?;

        // **One string, or the two fields that used to say the same thing.**
        // A package built before 2026-08-22 carries `format` and `version`;
        // reading both spellings is what keeps such a package judgeable, and
        // only one of them is ever written.
        let declared = match (&config.kind, &config.format, config.version) {
            (Some(kind), _, _) => kind.clone(),
            (None, Some(old), Some(version)) => format!("{old}@{version}"),
            _ => {
                return Err(Error::invalid(
                    "the package does not say what it is; `type: \"name@version\"` is required",
                ));
            }
        };

        // A Runner that does not know the version refuses the package rather
        // than guessing at what changed in it.
        if declared != envelope(format) {
            return Err(Error::UnknownFormat {
                format: declared,
                version: VERSION,
            });
        }

        // Normalised on the way in, so nothing downstream has to know which
        // spelling arrived — and so a merged or re-serialised document carries
        // the new one.
        config.kind = Some(declared);
        config.format = None;
        config.version = None;

        config.validated()
    }

    fn validated(self) -> Result<Self> {
        check(self.limits.time_ms, "limits.timeMs")?;
        check(self.limits.memory_bytes, "limits.memoryBytes")?;

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

        // **The paths this document declares are package paths.** They are read
        // with `package.join(source)` — in `pipeline.rs` for the checker, in
        // `calibrate.rs` for a model solution — and `join` on an absolute path
        // discards everything to its left. Unvalidated, `source:
        // /var/lib/algojudge-runner/identity.key` is a file the Runner reads
        // and then quotes back: the build fails, and the compiler's diagnostics
        // carry the offending lines into the failure reason and the uploaded
        // `log`.
        //
        // `archive.rs` has refused these names in an archive entry from the
        // beginning, so this is that rule one layer up and nothing new: a name
        // it rejects could not have been extracted into the package either. It
        // holds for an overlay too, since `overlaid` ends here — and an
        // activity's `config` can name a checker.
        for declared in self
            .checker
            .iter()
            .chain(self.interactor.iter())
            .chain(self.models())
        {
            crate::archive::safe_path(
                &declared.source,
                crate::archive::Limits::default().max_path_length,
            )?;
        }

        // **Both is not a richer package, it is an unanswered question.** A
        // checker judges output a program produced on its own; an interactor
        // produces the input that output answers. Declaring both leaves nothing
        // to say which of them is the judge, and picking one here would be this
        // Runner inventing an answer the author did not give.
        if self.checker.is_some() && self.interactor.is_some() {
            return Err(Error::invalid(
                "a package declares a checker or an interactor, never both: they                  decide the same question and nothing here says which of them judges",
            ));
        }

        Ok(self)
    }

    /// Applies what the Server merged on top of the package's own configuration.
    ///
    /// **The chain is package → `ProblemVersion.Config` → `SeriesProblem.Config`,
    /// and until now the Runner read only the first link.** The Server merges
    /// the upper two — member by member at the top level, since it understands
    /// neither — and sends the result with every job; ignoring it meant one
    /// library problem attached to two activities with different limits was
    /// judged under the package's limits in both. The format describes the chain
    /// as working and the Server computes it; the Runner was throwing it away.
    ///
    /// **Merged in depth, since 2026-08-22.** It used to replace whole top-level
    /// members, on the stated grounds that anything deeper "would require
    /// knowing what the members mean". That is not so — merging two JSON objects
    /// is structural, and the rule cost real work: narrowing a time limit meant
    /// restating the memory limit beside it, and an author who forgot got
    /// `missing field memoryBytes` rather than the limit they asked for.
    ///
    /// So an overlay may now name one option:
    ///
    /// ```yaml
    /// limits: { timeMs: 500 }     # memoryBytes stays whatever the package said
    /// ```
    ///
    /// **Arrays replace, with one exception**: an array whose elements all carry
    /// a distinct numeric `group` merges by it, so an assignment can narrow one
    /// group without restating the rest. Uniqueness is the condition rather than
    /// the field name — `groups` has it and the format enforces it, while
    /// `calibration.measured` repeats a group once per language, and merging
    /// that by group would silently collapse the languages into one row.
    ///
    /// **Nothing can be removed.** A deep merge adds and replaces; it has no way
    /// to say "unset". Everything an overlay could have unset by omission is a
    /// required field, so unsetting it only ever produced a validation error —
    /// an overlay is for narrowing, not for cutting.
    ///
    /// An overlay naming something this format has no field for is **refused**,
    /// not ignored — a misspelled limit is a limit that silently did not apply.
    pub fn overlaid(self, overlay: Option<&serde_json::Value>) -> Result<Self> {
        let Some(serde_json::Value::Object(members)) = overlay else {
            return Ok(self);
        };
        if members.is_empty() {
            return Ok(self);
        }

        let mut merged = match serde_json::to_value(&self) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => return Ok(self),
        };
        for (name, value) in members {
            // These say what the document *is*; an overlay does not get to
            // change that. `format` and `version` are the spelling used before
            // 2026-08-22 and are skipped too, so an overlay cannot reach round
            // the front door by using the old name.
            if name == "type" || name == "format" || name == "version" {
                continue;
            }
            let combined = match merged.get(name) {
                Some(existing) => merge(existing, value),
                None => value.clone(),
            };
            merged.insert(name.clone(), combined);
        }

        let config: Config = serde_json::from_value(serde_json::Value::Object(merged))
            .map_err(|e| Error::invalid(format!("the merged configuration will not read: {e}")))?;
        config.validated()
    }

    /// The limits before any group narrows them: the package's own, with every
    /// override that names this submission's language applied.
    ///
    /// Separate from <see cref="effective"/> because it is the part that holds
    /// for a **whole submission** — a submission has one language and many
    /// groups — and that is what the result document can honestly state in the
    /// single pair of numbers it carries.
    ///
    /// **Keys, plural, least specific first.** A language id used to be one
    /// word (`python`) and is now two levels (`python3`, `pypy3` — see the
    /// Runner's `language.rs`), so an override written the way this format
    /// documents it would have stopped matching anything the day the catalogue
    /// grew, and stopped **silently**: every Python submission held to the C++
    /// limit, no error anywhere. The caller passes the family and then the
    /// toolchain, and both are applied in that order, so `overrideLimits`
    /// under `python` covers PyPy too and an entry under `pypy3` beats it
    /// field by field.
    ///
    /// This crate deliberately does not know what a family is. It is handed
    /// the keys rather than deriving them, because the catalogue that decides
    /// them belongs to the problem type and not to the package format.
    pub fn for_language(&self, keys: &[&str]) -> Limits {
        let mut limits = self.limits;
        for key in keys {
            if let Some(over) = self.override_limits.get(*key) {
                apply(&mut limits, over);
            }
        }
        limits
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
    pub fn effective(&self, group: u32, keys: &[&str]) -> Limits {
        let mut limits = self.for_language(keys);

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

    /// Every model solution the package declares, however it declared them.
    ///
    /// One field written two ways: `modelSolution` for the common case of a
    /// single reference, `modelSolutions` when a package measures several
    /// languages. Reading both here means nothing downstream has to know which
    /// spelling an author used.
    pub fn models(&self) -> Vec<&Source> {
        self.model_solution
            .iter()
            .chain(self.model_solutions.iter())
            .collect()
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

/// One value laid over another, in depth.
///
/// Objects merge member by member, recursively; everything else is replaced.
/// Arrays are the one interesting case — see [`keyed_by_group`].
fn merge(base: &serde_json::Value, over: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;

    match (base, over) {
        (Value::Object(below), Value::Object(above)) => {
            let mut out = below.clone();
            for (name, value) in above {
                let combined = match below.get(name) {
                    Some(existing) => merge(existing, value),
                    None => value.clone(),
                };
                out.insert(name.clone(), combined);
            }
            Value::Object(out)
        }

        (Value::Array(below), Value::Array(above))
            if keyed_by_group(below) && keyed_by_group(above) =>
        {
            let mut out = below.clone();
            for element in above {
                let group = group_of(element);
                match out.iter_mut().find(|existing| group_of(existing) == group) {
                    Some(existing) => *existing = merge(existing, element),
                    // A group the package does not have is an addition rather
                    // than an override. The format decides whether it is legal;
                    // this only decides where it goes.
                    None => out.push(element.clone()),
                }
            }
            Value::Array(out)
        }

        _ => over.clone(),
    }
}

/// Whether an array is a set of things identified by `group`.
///
/// **Uniqueness is the test, not the field name.** `groups` carries one entry
/// per group and the format refuses a duplicate, so the number identifies the
/// entry and merging by it is meaningful. `calibration.measured` carries the
/// same group once per language, so there the number identifies nothing — and
/// merging by it would fold several languages' measurements into one.
fn keyed_by_group(elements: &[serde_json::Value]) -> bool {
    let groups: Vec<Option<u64>> = elements.iter().map(group_of).collect();
    if groups.iter().any(Option::is_none) {
        return false;
    }
    let mut seen = std::collections::HashSet::new();
    groups.iter().all(|g| seen.insert(*g))
}

fn group_of(element: &serde_json::Value) -> Option<u64> {
    element.get("group").and_then(serde_json::Value::as_u64)
}

fn apply(limits: &mut Limits, over: &PartialLimits) {
    if let Some(time) = over.time_ms {
        limits.time_ms = time;
    }
    if let Some(memory) = over.memory_bytes {
        limits.memory_bytes = memory;
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
    if let Some(memory) = limits.memory_bytes {
        check(memory, &format!("{what}.memoryBytes"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The specification's own example, so this test fails if the document and
    /// the reader ever part company.
    const FROM_THE_SPECIFICATION: &str = r#"
type: "standard-io@1"

limits:
  timeMs: 1000
  memoryBytes: 268435456

overrideLimits:
  python:
    timeMs: 3000
    memoryBytes: 536870912

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
  memory: { factor: 1, add: 16777216, roundTo: 1048576 }

extraCompilationFiles: []
"#;

    #[test]
    fn the_specifications_own_example_reads() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();

        assert_eq!(config.limits.time_ms, 1000);
        assert_eq!(config.limits.memory_bytes, 268435456);
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
        let group2 = config.effective(2, &["cpp"]);
        assert_eq!(group2.time_ms, 2000);
        assert_eq!(group2.memory_bytes, 268435456);

        let group1 = config.effective(1, &["cpp"]);
        assert_eq!(group1.time_ms, 1000);
    }

    #[test]
    fn a_language_override_applies_where_a_group_says_nothing() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();

        let python = config.effective(1, &["python"]);
        assert_eq!(python.time_ms, 3000);
        assert_eq!(python.memory_bytes, 536870912);
    }

    /// **The trap the second key exists to close.** A language id used to be one
    /// word, and every `overrideLimits` on disk is written under it. When ids
    /// became two levels, an override under `python` stopped matching `pypy3` —
    /// silently, because a missing override is not an error, it is the package's
    /// own limits. Every PyPy submission would have been held to the C++ limit
    /// and nothing anywhere would have said so.
    #[test]
    fn an_override_written_for_a_family_reaches_its_toolchains() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();

        // What a package author wrote, and what the Runner now asks with.
        let pypy = config.for_language(&["python", "pypy3"]);
        assert_eq!(pypy.time_ms, 3000, "PyPy is Python");
        assert_eq!(pypy.memory_bytes, 536870912);

        // And a toolchain that has no entry of its own is unaffected by
        // another family's.
        let cpp = config.for_language(&["cpp", "cpp17-gcc"]);
        assert_eq!(cpp.time_ms, 1000);
    }

    /// The other half of the rule: **least specific first**, so a package that
    /// wants PyPy alone to differ can say so without restating the family.
    #[test]
    fn a_toolchains_own_override_beats_its_familys_field_by_field() {
        let both = FROM_THE_SPECIFICATION.replace(
            "overrideLimits:\n  python:\n",
            "overrideLimits:\n  pypy3:\n    timeMs: 1500\n  python:\n",
        );
        let config = Config::parse(&both).unwrap();

        let pypy = config.for_language(&["python", "pypy3"]);
        assert_eq!(pypy.time_ms, 1500, "PyPy is faster, and says so itself");
        assert_eq!(
            pypy.memory_bytes, 536870912,
            "and the family's entry still fills what PyPy did not name"
        );

        // CPython keeps the family's, untouched by PyPy's.
        assert_eq!(config.for_language(&["python", "python3"]).time_ms, 3000);
    }

    /// Pins the reading documented on `effective`: most specific first. If this
    /// test is ever changed, the open question it stands for was answered.
    #[test]
    fn a_group_limit_beats_a_language_override() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();

        let python = config.effective(2, &["python"]);
        assert_eq!(python.time_ms, 2000, "the group's own limit wins");
        assert_eq!(
            python.memory_bytes, 536870912,
            "and the override still fills the rest"
        );
    }

    /// The link in the chain the Runner used to throw away.
    #[test]
    fn what_the_server_merged_is_applied_on_top_of_the_package() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();
        assert_eq!(config.limits.time_ms, 1000);

        // The same problem, attached to an activity that gives it longer.
        let overlay = serde_json::json!({ "limits": { "timeMs": 5000, "memoryBytes": 268435456 } });
        let attached = config.overlaid(Some(&overlay)).unwrap();

        assert_eq!(attached.limits.time_ms, 5000);
        // Everything the overlay did not name is untouched.
        assert_eq!(attached.max_score(), 100);
        assert_eq!(
            attached.effective(2, &["cpp"]).time_ms,
            2000,
            "the group still states its own"
        );
    }

    #[test]
    fn an_absent_or_empty_overlay_changes_nothing() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();

        assert_eq!(config.clone().overlaid(None).unwrap().limits.time_ms, 1000);
        let empty = serde_json::json!({});
        assert_eq!(config.overlaid(Some(&empty)).unwrap().limits.time_ms, 1000);
    }

    /// A misspelled limit is a limit that silently did not apply, and an
    /// overlay is written by hand in a manager's screen.
    #[test]
    fn an_overlay_naming_something_this_format_has_no_field_for_is_refused() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();
        let overlay = serde_json::json!({ "limits": { "timeMS": 5000 } });

        assert!(config.overlaid(Some(&overlay)).is_err());
    }

    /// An overlay cannot turn a `standard-io` package into something else.
    #[test]
    fn an_overlay_may_not_change_what_the_document_is() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();
        let overlay = serde_json::json!({
            "type": "interactive@9", "format": "interactive", "version": 9,
        });

        let attached = config.overlaid(Some(&overlay)).unwrap();
        assert_eq!(attached.kind.as_deref(), Some("standard-io@1"));
    }

    /// **A package built before 2026-08-22 still judges.**
    ///
    /// It carries `format` and `version` rather than `type`, and refusing it
    /// would be an infrastructure failure on every submission to it — not a
    /// message anybody could act on. Read, normalised, and never written back
    /// in that spelling.
    #[test]
    fn the_two_fields_that_used_to_say_this_are_still_read() {
        let old = FROM_THE_SPECIFICATION.replace(
            "type: \"standard-io@1\"",
            "format: standard-io
version: 1",
        );
        let config = Config::parse(&old).unwrap();

        assert_eq!(config.kind.as_deref(), Some("standard-io@1"));
        assert!(config.format.is_none(), "and is not carried forward");
        assert!(config.version.is_none());

        let written = serde_yaml_ng::to_string(&config).unwrap();
        assert!(written.contains("type: standard-io@1"), "{written}");
        assert!(
            !written.contains("format:"),
            "one spelling out, not two: {written}"
        );
    }

    /// A document that says nothing about what it is.
    #[test]
    fn a_package_that_does_not_say_what_it_is_is_refused() {
        let anonymous = FROM_THE_SPECIFICATION.replace(
            "type: \"standard-io@1\"
",
            "",
        );
        let refused = Config::parse(&anonymous).unwrap_err().to_string();
        assert!(refused.contains("does not say what it is"), "{refused}");
    }

    /// **Open question, and this is the one line.** The specification says only
    /// that each layer beats the one before, never in which direction. Today an
    /// overlay applies as given — it may raise a limit as well as tighten one.
    /// If that is settled the other way, it is settled in `overlaid`.
    /// One option, without restating the ones beside it.
    ///
    /// This is what the deep merge was for. Under the old rule `limits` was
    /// replaced whole, so narrowing the time meant repeating the memory — and an
    /// author who forgot got `missing field memoryBytes` instead of a limit.
    #[test]
    fn an_overlay_may_name_one_option_and_leave_the_rest() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();
        let before = config.limits.memory_bytes;

        let narrowed = config
            .overlaid(Some(&serde_json::json!({ "limits": { "timeMs": 250 } })))
            .unwrap();

        assert_eq!(narrowed.limits.time_ms, 250);
        assert_eq!(
            narrowed.limits.memory_bytes, before,
            "the memory limit should have been inherited, not dropped",
        );
    }

    /// One group narrowed, the others left as the package wrote them.
    #[test]
    fn an_overlay_may_narrow_one_group_without_restating_the_others() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();
        let groups_before = config.groups.len();
        let points_before: u32 = config.groups.iter().map(|g| g.points).sum();

        let narrowed = config
            .overlaid(Some(&serde_json::json!({
                "groups": [{ "group": 1, "limits": { "timeMs": 250 } }]
            })))
            .unwrap();

        assert_eq!(
            narrowed.groups.len(),
            groups_before,
            "the other groups should have survived: {:?}",
            narrowed.groups,
        );
        assert_eq!(
            narrowed.groups.iter().map(|g| g.points).sum::<u32>(),
            points_before,
            "and kept their points, which the overlay never mentioned",
        );
        let one = narrowed.groups.iter().find(|g| g.group == 1).unwrap();
        assert_eq!(one.limits.and_then(|l| l.time_ms), Some(250));
    }

    /// An array whose `group` is not an identity replaces instead of merging.
    ///
    /// `calibration.measured` carries one row per group **per language**, so the
    /// number identifies nothing there. Merging by it would fold two languages'
    /// measurements into one row, quietly, and the calibration that came out
    /// would be a number nobody measured.
    #[test]
    fn an_array_that_repeats_a_group_is_replaced_rather_than_merged() {
        let two_languages = serde_json::json!([
            { "group": 1, "language": "cpp",    "timeMs": 100 },
            { "group": 1, "language": "python", "timeMs": 900 },
        ]);
        let one_language = serde_json::json!([
            { "group": 1, "language": "cpp", "timeMs": 120 },
        ]);

        let merged = merge(&two_languages, &one_language);

        assert_eq!(
            merged.as_array().map(Vec::len),
            Some(1),
            "a repeated group is not a key, so the array is replaced: {merged}",
        );
    }

    #[test]
    fn an_overlay_may_currently_raise_a_limit_as_well_as_lower_one() {
        let config = Config::parse(FROM_THE_SPECIFICATION).unwrap();

        let raised = config
            .clone()
            .overlaid(Some(&serde_json::json!({
                "limits": { "timeMs": 9000, "memoryBytes": 268435456 }
            })))
            .unwrap();
        assert_eq!(raised.limits.time_ms, 9000);

        let lowered = config
            .overlaid(Some(&serde_json::json!({
                "limits": { "timeMs": 250, "memoryBytes": 268435456 }
            })))
            .unwrap();
        assert_eq!(lowered.limits.time_ms, 250);
    }

    // ── calibration ─────────────────────────────────────────────────────────

    /// The specification's own worked example, so this fails if the document
    /// and the arithmetic ever part company.
    #[test]
    fn the_default_rules_are_the_ones_the_format_states() {
        let calibration = Calibration::default();

        // 240 × 3 + 0 → 720, rounded up to 100 → 800 ms.
        assert_eq!(calibration.time_rule().applied(240.0), 800.0);
        // 31744000 + 16777216 → 48521216, rounded up to 1 MiB → 49283072 bytes.
        assert_eq!(calibration.memory_rule().applied(31744000.0), 49283072.0);
    }

    /// **Up, never to nearest.** A limit landing below the measurement it came
    /// from fails the solution it was derived from.
    #[test]
    fn a_derived_limit_is_never_below_the_measurement_it_came_from() {
        let rule = Rule {
            factor: 1.0,
            add: 0.0,
            round_to: 100.0,
        };

        assert_eq!(
            rule.applied(201.0),
            300.0,
            "rounding to nearest would give 200"
        );
        assert_eq!(
            rule.applied(200.0),
            200.0,
            "and an exact multiple stays put"
        );
    }

    const SEVERAL_MODELS: &str = r#"
type: "standard-io@1"
limits:
  timeMs: 1000
  memoryBytes: 268435456
groups:
  - group: 1
    points: 100
modelSolutions:
  - { source: solutions/model.cpp, language: cpp }
  - { source: solutions/model.py, language: python }
calibration:
  measured:
    - { group: 1, timeMs: 240, memoryBytes: 31744000 }
    - { group: 2, timeMs: 900 }
    - { group: 2, language: python, timeMs: 3100 }
  at: 2026-08-09T10:00:00Z
  runner: runner-01
"#;

    #[test]
    fn a_package_may_declare_one_model_solution_or_one_per_language() {
        let one = Config::parse(FROM_THE_SPECIFICATION).unwrap();
        assert_eq!(one.models().len(), 1);
        assert_eq!(one.models()[0].language, "cpp");

        let many = Config::parse(SEVERAL_MODELS).unwrap();
        assert_eq!(many.models().len(), 2);
        assert_eq!(many.models()[1].language, "python");
    }

    /// A measurement is per group, because that is where a limit lives: a
    /// package whose group 2 states three seconds is calibrated wrongly by one
    /// number for the whole problem.
    #[test]
    fn a_measurement_is_recorded_per_group_and_optionally_per_language() {
        let config = Config::parse(SEVERAL_MODELS).unwrap();
        let measured = &config.calibration.as_ref().unwrap().measured;

        assert_eq!(measured.len(), 3);
        assert_eq!(measured[0].group, 1);
        assert_eq!(measured[0].memory_bytes, Some(31744000));
        assert!(measured[1].memory_bytes.is_none(), "absent is not zero");
        assert_eq!(measured[2].language.as_deref(), Some("python"));
        assert_eq!(
            config.calibration.as_ref().unwrap().runner.as_deref(),
            Some("runner-01")
        );
    }

    #[test]
    fn a_format_this_runner_does_not_know_is_refused() {
        let yaml = FROM_THE_SPECIFICATION.replace("standard-io@1", "standard-io@2");
        let error = Config::parse(&yaml).unwrap_err();
        assert!(
            matches!(&error, Error::UnknownFormat { format, .. } if format == "standard-io@2"),
            "got {error}"
        );

        let yaml = FROM_THE_SPECIFICATION.replace("standard-io@1", "interactive@1");
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

    /// An interactive problem parses, and says so in one field.
    #[test]
    fn an_interactor_is_read_where_a_checker_would_be() {
        let yaml = FROM_THE_SPECIFICATION.replace("checker:", "interactor:");
        let config = Config::parse(&yaml).expect("an interactive package");

        assert!(config.checker.is_none());
        let interactor = config.interactor.expect("the interactor");
        assert_eq!(interactor.source, "checker/checker.cpp");
        assert_eq!(interactor.language, "cpp");
    }

    /// **Both is not a richer package, it is an unanswered question**, and the
    /// refusal is what stops this Runner answering it on the author's behalf.
    #[test]
    fn a_package_declaring_both_a_checker_and_an_interactor_is_refused() {
        let yaml = FROM_THE_SPECIFICATION.replace(
            "checker:",
            "interactor:\n  source: checker/checker.cpp\n  language: cpp\nchecker:",
        );
        let said = Config::parse(&yaml)
            .expect_err("a package cannot declare both")
            .to_string();

        assert!(said.contains("never both"), "got {said}");
    }

    /// The path rule holds for the new field, which is the whole reason to have
    /// written it into the same loop rather than beside it.
    #[test]
    fn an_interactor_outside_the_package_is_refused() {
        let yaml = FROM_THE_SPECIFICATION.replace(
            "checker:\n  source: checker/checker.cpp",
            "interactor:\n  source: /var/lib/algojudge-runner/identity.key",
        );
        assert!(
            Config::parse(&yaml).is_err(),
            "an absolute path names a file the Runner reads and quotes back",
        );
    }

    /// **A path in this document is a path inside the package.** It is joined
    /// onto the package root and `join` discards that root the moment the path
    /// is absolute — so an unvalidated one names any file the Runner can read,
    /// and the build that then fails carries its contents back to the Server in
    /// the compiler's own diagnostics.
    #[test]
    fn a_checker_outside_the_package_is_refused() {
        for source in [
            "/var/lib/algojudge-runner/identity.key",
            "/etc/passwd",
            "../../../../etc/passwd",
            "checker/../../../etc/shadow",
        ] {
            let yaml = FROM_THE_SPECIFICATION.replace("checker/checker.cpp", source);
            let error = Config::parse(&yaml).unwrap_err();
            assert!(matches!(error, Error::Refused(_)), "{source} got {error}");
        }
    }

    /// The same join, in `calibrate.rs`, for a package's own reference.
    #[test]
    fn a_model_solution_outside_the_package_is_refused() {
        for source in ["/etc/passwd", "../../elsewhere/model.cpp"] {
            let yaml = FROM_THE_SPECIFICATION.replace("solutions/model.cpp", source);
            let error = Config::parse(&yaml).unwrap_err();
            assert!(matches!(error, Error::Refused(_)), "{source} got {error}");
        }
    }

    /// **An overlay is a second author.** The activity's `config` merges the
    /// `checker` member like any other, so the rule has to hold after the merge
    /// and not only on the package's own document.
    #[test]
    fn an_overlay_cannot_repoint_the_checker_out_of_the_package() {
        let overlay = serde_json::json!({
            "checker": { "source": "/var/lib/algojudge-runner/identity.key" },
        });
        let error = Config::parse(FROM_THE_SPECIFICATION)
            .unwrap()
            .overlaid(Some(&overlay))
            .unwrap_err();
        assert!(matches!(error, Error::Refused(_)), "got {error}");
    }

    /// An unknown field is a typo, and a typo in a limit is a limit that
    /// silently did not apply.
    #[test]
    fn a_misspelled_field_is_refused_rather_than_ignored() {
        let yaml =
            FROM_THE_SPECIFICATION.replace("memoryBytes: 268435456", "memoryBYTES_TYPO: 262144");
        let error = Config::parse(&yaml).unwrap_err();
        assert!(matches!(error, Error::Malformed(_)), "got {error}");
    }
}
