//! The forbidden-identifier dictionary.
//!
//! **A policy control, not a security boundary.** This is the project's own
//! conclusion, from its engineering thesis and from the wider consensus: source
//! filtering fails on token pasting, macro indirection, runtime name
//! construction, inline assembly and encoding tricks, and none of `isolate`,
//! `nsjail` or `sio2jail` implements one. What stops a malicious program is the
//! sandbox. What this does is catch a rule violation **early**, so a participant
//! learns they broke a rule instead of discovering it in a results table.
//!
//! It therefore runs **before the build**, produces the verdict
//! `PolicyViolation` with score 0, tells the participant **which rule** matched,
//! and leaves the submission rejudgeable. A bypass is expected and is not a bug.
//!
//! The default profile is `policy/standard-io-default.yml`, **compiled into the
//! Runner** (D-10): a package names the profile rather than restating sixty
//! words, so a task written today still gets a reviewed set tomorrow.
//!
//! **In this version the profile is fixed and no package configures it**
//! (decided 2026-08-09). `PACKAGE_FORMAT.md` has no `policy` section and is not
//! gaining one now — a format section invented ahead of anyone needing it
//! acquires a shape nobody chose. The profile being *versioned* is what keeps
//! that reversible: when a package can vary the rules, one that names
//! `standard-io/default@1` keeps working unchanged.
//!
//! **Three families are enforced — C, C++ and Python**, which between them are
//! the eighteen toolchains the Runner judges. Rust and Java are carried in the
//! file and are **not** applied, because enforcing a dictionary for a language
//! nothing can submit would be a rule nobody could trip and nobody could test.
//!
//! C's section is a YAML alias of C++'s rather than a second copy. See the
//! profile, and `language.rs` for why the lookup is by family at all.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use indexmap::IndexMap;
use regex::Regex;
use serde::Deserialize;

use crate::language::{Family, Language};

/// The profile shipped with this Runner.
const BUILT_IN: &str = include_str!("../policy/standard-io-default.yml");

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dictionary {
    pub version: u32,
    pub id: String,
    #[serde(default)]
    pub matching: Matching,
    #[serde(default)]
    pub languages: IndexMap<String, LanguageRules>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Matching {
    #[serde(default = "yes")]
    pub strip_comments: bool,
    #[serde(default = "yes")]
    pub strip_string_literals: bool,
    #[serde(default = "yes")]
    pub whole_token: bool,
    #[serde(default = "yes")]
    pub case_sensitive: bool,
    #[serde(default = "yes")]
    pub report_all_matches: bool,
}

impl Default for Matching {
    fn default() -> Self {
        Self {
            strip_comments: true,
            strip_string_literals: true,
            whole_token: true,
            case_sensitive: true,
            report_all_matches: true,
        }
    }
}

fn yes() -> bool {
    true
}

/// `on` / `off` as well as `true` / `false`.
///
/// **YAML 1.1 made `on`, `off`, `yes` and `no` booleans; YAML 1.2 made them
/// strings**, and this parser follows 1.2. `PACKAGE_FORMAT.md` warns about
/// exactly this edge for language ids. The profile is written the way a person
/// reading a switch expects — `default: on` — so the reader accepts both rather
/// than the file being rewritten to suit the parser.
fn switch<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    match serde_yaml_ng::Value::deserialize(deserializer)? {
        serde_yaml_ng::Value::Bool(value) => Ok(value),
        serde_yaml_ng::Value::String(word) => match word.as_str() {
            "on" | "yes" | "true" => Ok(true),
            "off" | "no" | "false" => Ok(false),
            other => Err(D::Error::custom(format!(
                "{other:?} is not a switch; write on or off"
            ))),
        },
        other => Err(D::Error::custom(format!("{other:?} is not a switch"))),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanguageRules {
    #[serde(default)]
    pub denied_headers: Vec<String>,
    /// Ordered, because the report's order has to be the same on two Runners
    /// judging the same submission.
    #[serde(default)]
    pub groups: IndexMap<String, Group>,
    #[serde(default)]
    pub denied_imports: Vec<String>,
    #[serde(default)]
    pub denied_builtins: Vec<String>,
    #[serde(default)]
    pub denied_patterns: Vec<String>,
    /// Recorded so nobody re-adds them from the legacy list. Never matched.
    #[serde(default)]
    pub never_denied: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    /// What the participant is told. Not the group's letter — "Opening a
    /// file" teaches something, "F" does not.
    pub name: String,
    #[serde(default = "yes", deserialize_with = "switch")]
    pub default: bool,
    #[serde(default)]
    pub identifiers: Vec<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
}

/// One thing a submission did that the activity's rules forbid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The rule's name, as the participant reads it.
    pub rule: String,
    pub matched: String,
    pub line: usize,
}

impl Violation {
    /// The line for a result's `note`.
    pub fn note(&self) -> String {
        format!("{} — {} (line {})", self.rule, self.matched, self.line)
    }
}

impl Dictionary {
    /// The profile compiled into this Runner.
    ///
    /// Parsed once and then shared. A dictionary that failed to parse is a
    /// Runner that cannot enforce the rules it claims to, so this panics rather
    /// than degrading quietly — and it panics at first use, in a build where
    /// the file is right there to fix.
    pub fn built_in() -> &'static Dictionary {
        static PARSED: OnceLock<Dictionary> = OnceLock::new();
        PARSED.get_or_init(|| {
            Dictionary::parse(BUILT_IN).expect("the built-in policy profile must parse")
        })
    }

    pub fn parse(yaml: &str) -> Result<Self, String> {
        serde_yaml_ng::from_str(yaml).map_err(|e| format!("the policy profile will not read: {e}"))
    }

    /// Everything this submission does that the rules forbid.
    ///
    /// Empty means it broke none of them — and an empty rule set for a language
    /// means the check is **skipped**, not that everything is denied.
    ///
    /// **Takes the language rather than its name**, and this is not tidiness.
    /// The profile was looked up by whatever string the submission carried, and
    /// a string that matches nothing returns *no violations* — indistinguishable
    /// from a clean submission. That was survivable while the only ids in
    /// existence were `cpp` and `python`; it stopped being on 2026-08-22, when
    /// they became `cpp17-gcc` and `pypy3` and every one of the eighteen would
    /// have looked up nothing. So the rules are found by **id and then family**,
    /// and which checks run is decided by matching the family as an enum — a
    /// fourth family will not compile until somebody says what happens to it.
    pub fn check(&self, language: &Language, source: &str) -> Vec<Violation> {
        let Some(rules) = self
            .languages
            .get(language.id)
            .or_else(|| self.languages.get(language.family.as_str()))
        else {
            // Reached only by a profile that omits a family this Runner can
            // judge — which is the profile being wrong, not the submission.
            tracing::warn!(
                language = language.id,
                family = language.family.as_str(),
                "the policy profile has no rules for this language, so nothing is enforced"
            );
            return Vec::new();
        };

        let stripped = Scanned::of(strip(source, language.family, &self.matching));
        let mut found = Vec::new();

        match language.family {
            Family::C | Family::Cpp => {
                headers(&stripped, rules, &mut found);
                // Identifiers are matched against everything **except** the
                // include directives, which are rule H's business alone (§7.4).
                // Otherwise `#include <asm/unistd.h>` also trips the inline-
                // assembly group, because `/` is a word boundary — a false
                // positive on a line that was already correctly reported once.
                groups(
                    &Scanned::of(without_includes(&stripped.text)),
                    rules,
                    &mut found,
                );
            }
            Family::Python => {
                imports(&stripped, rules, &mut found);
                builtins(&stripped, rules, &mut found);
                patterns(
                    &stripped,
                    &rules.denied_patterns,
                    "forbidden call",
                    &mut found,
                );
            }
        }

        if !self.matching.report_all_matches {
            found.truncate(1);
        }
        found
    }
}

// ── matching ────────────────────────────────────────────────────────────────

/// Removes comments and literals, **keeping every byte position**.
///
/// Replaced with spaces rather than deleted, so a line number computed
/// afterwards is the line number in the file the participant wrote. A comment
/// saying "nie używam fork" must not fail a submission, and a message pointing
/// at the wrong line is nearly as bad as no message.
fn strip(source: &str, family: Family, matching: &Matching) -> String {
    let bytes: Vec<char> = source.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(bytes.len());
    let mut i = 0;

    let python = family == Family::Python;

    // Characters compared where they stand. This used to build a `String` out
    // of the next three characters on every iteration — **one heap allocation
    // per character of the submission**, in the Runner's own process, on bytes
    // a participant chose.
    let at = |k: usize, c: char| bytes.get(k) == Some(&c);

    while i < bytes.len() {
        let line_comment = if python {
            at(i, '#')
        } else {
            at(i, '/') && at(i + 1, '/')
        };

        if matching.strip_comments && line_comment {
            while i < bytes.len() && bytes[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if matching.strip_comments && !python && at(i, '/') && at(i + 1, '*') {
            while i < bytes.len() {
                let end = at(i, '*') && at(i + 1, '/');
                out.push(if bytes[i] == '\n' { '\n' } else { ' ' });
                i += 1;
                if end {
                    if i < bytes.len() {
                        out.push(' ');
                        i += 1;
                    }
                    break;
                }
            }
            continue;
        }
        if matching.strip_string_literals && (bytes[i] == '"' || bytes[i] == '\'') {
            let quote = bytes[i];
            // Triple quotes are one literal in Python, not three empty ones.
            let triple =
                python && bytes.len() > i + 2 && bytes[i + 1] == quote && bytes[i + 2] == quote;

            out.push(' ');
            i += 1;
            if triple {
                out.push(' ');
                out.push(' ');
                i += 2;
            }

            while i < bytes.len() {
                if bytes[i] == '\\' {
                    out.push(' ');
                    i += 1;
                    if i < bytes.len() {
                        out.push(if bytes[i] == '\n' { '\n' } else { ' ' });
                        i += 1;
                    }
                    continue;
                }
                let closing = if triple {
                    bytes.len() > i + 2
                        && bytes[i] == quote
                        && bytes[i + 1] == quote
                        && bytes[i + 2] == quote
                } else {
                    bytes[i] == quote
                };
                out.push(if bytes[i] == '\n' { '\n' } else { ' ' });
                i += 1;
                if closing {
                    for _ in 0..(if triple { 2 } else { 0 }) {
                        if i < bytes.len() {
                            out.push(' ');
                            i += 1;
                        }
                    }
                    break;
                }
            }
            continue;
        }

        out.push(bytes[i]);
        i += 1;
    }

    out.into_iter().collect()
}

/// Blanks out `#include` lines, keeping every byte position.
fn without_includes(source: &str) -> String {
    include_pattern()
        .replace_all(source, |captured: &regex::Captures| {
            " ".repeat(
                captured
                    .get(0)
                    .expect("the whole match")
                    .as_str()
                    .chars()
                    .count(),
            )
        })
        .into_owned()
}

fn include_pattern() -> &'static Regex {
    static INCLUDE: OnceLock<Regex> = OnceLock::new();
    INCLUDE.get_or_init(|| {
        Regex::new(r#"(?m)^\s*#\s*include\s*[<"]([^>"]+)[>"]"#).expect("a fixed pattern")
    })
}

/// A text to match against, with its line breaks already found.
///
/// **The two travel together on purpose.** Pairing an offset with the wrong
/// text is a wrong line number and nothing else — no error, no failure, just a
/// message pointing a participant at a line they did not write — and there are
/// two texts here: the stripped source, and the same source with its include
/// directives blanked out.
///
/// The line number used to be `text[..offset].matches('\n').count() + 1`,
/// computed **per violation**. That is a scan of everything before the hit, and
/// `reportAllMatches` is on, so a source with many matches cost the product of
/// its length and their number — untrusted bytes, in the Runner's own process,
/// with nothing bounding the work. The offsets are ascending by construction,
/// which is what makes the lookup a binary search.
struct Scanned {
    text: String,
    newlines: Vec<usize>,
}

impl Scanned {
    fn of(text: String) -> Self {
        let newlines = text.match_indices('\n').map(|(at, _)| at).collect();
        Self { text, newlines }
    }

    fn line_of(&self, offset: usize) -> usize {
        self.newlines.partition_point(|&at| at < offset) + 1
    }
}

/// A compiled pattern, kept for the life of the process.
///
/// The profile is fixed and every submission is checked against the same
/// hundred and sixty or so patterns, so compiling them per submission is work
/// done once and then done again for every job the Runner ever takes. A
/// `Regex` matches through `&self` and is `Send + Sync`, so one compilation
/// serves all of them.
///
/// A pattern that will not compile is remembered as such, so the profile's own
/// mistake is not recompiled on every submission either.
fn compiled(pattern: &str) -> Option<Arc<Regex>> {
    static COMPILED: OnceLock<Mutex<HashMap<String, Option<Arc<Regex>>>>> = OnceLock::new();

    COMPILED
        .get_or_init(Default::default)
        .lock()
        .expect("the pattern cache is never poisoned")
        .entry(pattern.to_owned())
        .or_insert_with(|| Regex::new(pattern).ok().map(Arc::new))
        .clone()
}

/// `#include <x>` — matched as a directive, never as a word.
fn headers(scanned: &Scanned, rules: &LanguageRules, found: &mut Vec<Violation>) {
    for capture in include_pattern().captures_iter(&scanned.text) {
        let named = capture
            .get(1)
            .expect("the group is in the pattern")
            .as_str();
        let denied = rules
            .denied_headers
            .iter()
            .any(|d| match d.strip_suffix('*') {
                // `linux/*` denies the tree, which is how a family is written once
                // instead of guessed at entry by entry.
                Some(prefix) => named.starts_with(prefix),
                None => named == d,
            });
        if denied {
            found.push(Violation {
                rule: format!("forbidden header <{named}>"),
                matched: named.to_owned(),
                line: scanned.line_of(capture.get(0).expect("the whole match").start()),
            });
        }
    }
}

fn groups(scanned: &Scanned, rules: &LanguageRules, found: &mut Vec<Violation>) {
    for group in rules.groups.values() {
        // A group that is off is off. `remove` collides with `std::remove`,
        // which is entirely legal, and a rule that fails correct solutions is
        // worse than a rule that misses something the sandbox catches anyway.
        if !group.default {
            continue;
        }
        for identifier in &group.identifiers {
            whole_word(scanned, identifier, &group.name, found);
        }
        patterns(scanned, &group.patterns, &group.name, found);
    }
}

fn whole_word(scanned: &Scanned, word: &str, rule: &str, found: &mut Vec<Violation>) {
    let Some(pattern) = compiled(&format!(r"\b{}\b", regex::escape(word))) else {
        return;
    };
    for hit in pattern.find_iter(&scanned.text) {
        found.push(Violation {
            rule: rule.to_owned(),
            matched: word.to_owned(),
            line: scanned.line_of(hit.start()),
        });
    }
}

fn patterns(scanned: &Scanned, patterns: &[String], rule: &str, found: &mut Vec<Violation>) {
    for declared in patterns {
        // A pattern that will not compile is a mistake in the profile, not in
        // the submission. Skipped and logged rather than failing somebody's
        // solution over it.
        let Some(pattern) = compiled(declared) else {
            tracing::warn!(pattern = %declared, "a policy pattern will not compile");
            continue;
        };
        for hit in pattern.find_iter(&scanned.text) {
            found.push(Violation {
                rule: rule.to_owned(),
                matched: hit.as_str().trim().to_owned(),
                line: scanned.line_of(hit.start()),
            });
        }
    }
}

/// `import x`, `from x import`, `__import__("x")`.
fn imports(scanned: &Scanned, rules: &LanguageRules, found: &mut Vec<Violation>) {
    let denied: BTreeSet<&str> = rules.denied_imports.iter().map(String::as_str).collect();

    static IMPORT: OnceLock<Regex> = OnceLock::new();
    let import = IMPORT.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:from\s+([A-Za-z_][\w.]*)|import\s+([A-Za-z_][\w.]*))")
            .expect("a fixed pattern")
    });

    for capture in import.captures_iter(&scanned.text) {
        let named = capture
            .get(1)
            .or_else(|| capture.get(2))
            .expect("one of the two groups")
            .as_str();
        // `os.path` is `os`. The module is the unit; a submodule of a denied
        // module is denied with it.
        let root = named.split('.').next().unwrap_or(named);
        if denied.contains(root) {
            found.push(Violation {
                rule: format!("forbidden module {root}"),
                matched: named.to_owned(),
                line: scanned.line_of(capture.get(0).expect("the whole match").start()),
            });
        }
    }
}

fn builtins(scanned: &Scanned, rules: &LanguageRules, found: &mut Vec<Violation>) {
    for builtin in &rules.denied_builtins {
        // Only where it is called: `open` as a variable name is not the
        // built-in, and failing a solution over a local called `open` is the
        // false positive this whole design is trying to avoid.
        let Some(pattern) = compiled(&format!(r"\b{}\s*\(", regex::escape(builtin))) else {
            continue;
        };
        for hit in pattern.find_iter(&scanned.text) {
            found.push(Violation {
                rule: format!("forbidden builtin {builtin}"),
                matched: builtin.clone(),
                line: scanned.line_of(hit.start()),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::language::{catalogue, for_id, Images};

    /// By toolchain id, because that is what a submission carries. Every
    /// assertion below is therefore also an assertion that the id resolved to a
    /// family and the family found its rules.
    fn check(language: &str, source: &str) -> Vec<Violation> {
        let resolved = for_id(language, &Images::default())
            .unwrap_or_else(|| panic!("{language} is not in the catalogue"));
        Dictionary::built_in().check(&resolved, source)
    }

    #[test]
    fn the_shipped_profile_parses_and_says_what_it_is() {
        let dictionary = Dictionary::built_in();
        assert_eq!(dictionary.id, "standard-io/default@1");
        assert_eq!(dictionary.version, 1);
        assert!(dictionary.languages.contains_key("cpp"));
        assert!(dictionary.languages.contains_key("c"));
        assert!(dictionary.languages.contains_key("python"));
    }

    /// **The trap this lookup exists to close.** Every toolchain in the
    /// catalogue has to reach a rule set, because one that does not returns no
    /// violations — which reads exactly like a clean submission and would have
    /// disabled the dictionary for sixteen of the eighteen the day they were
    /// added.
    ///
    /// Proven by a submission that breaks a rule rather than by looking the
    /// profile up: a lookup asserted against itself would keep passing if the
    /// checks stopped running.
    #[test]
    fn every_toolchain_in_the_catalogue_is_actually_policed() {
        let breaks_a_rule = |family| match family {
            Family::C => {
                "#include <unistd.h>
int main(){return 0;}
"
            }
            Family::Cpp => {
                "#include <unistd.h>
int main(){}
"
            }
            Family::Python => {
                "import os
"
            }
        };

        for language in catalogue(&Images::default()) {
            let found = Dictionary::built_in().check(&language, breaks_a_rule(language.family));
            assert!(
                !found.is_empty(),
                "{} enforced nothing, so its rules were not found",
                language.id,
            );
        }
    }

    /// C is an alias of C++ in the profile, and an alias that stopped resolving
    /// would be a language with no rules at all.
    #[test]
    fn c_is_policed_by_the_same_rules_as_cpp() {
        let opening_a_file = "#include <stdio.h>
int main(){ FILE* f = fopen(\"x\", \"r\"); return 0; }
";

        assert_eq!(
            check("c11-gcc", opening_a_file),
            check("cpp17-gcc", opening_a_file),
        );
        assert!(!check(
            "c89-clang",
            "#include <unistd.h>
"
        )
        .is_empty());
    }

    // ── the reason the header list exists ───────────────────────────────────

    #[test]
    fn a_denied_header_is_caught_as_a_directive() {
        let found = check("cpp", "#include <unistd.h>\nint main(){}\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].rule.contains("unistd.h"));
        assert_eq!(found[0].line, 1);
    }

    #[test]
    fn a_denied_header_family_is_written_once() {
        assert_eq!(check("cpp", "#include <linux/futex.h>\n").len(), 1);
        assert_eq!(check("cpp", "#include <asm/unistd.h>\n").len(), 1);
    }

    /// The whole point of rule H: `open`, `read`, `write`, `close` and `fork`
    /// are **not** in the identifier list, because they live in `<unistd.h>`
    /// and banning them as bare words is the worst false-positive source in
    /// C++ — `open`/`closed` sets in A*, `read()` as a method name.
    #[test]
    fn ordinary_words_that_are_also_syscalls_are_not_matched() {
        let ordinary = r#"
#include <bits/stdc++.h>
struct Node { bool open; bool closed; };
int read() { return 42; }
int main() { std::set<int> open, closed; auto x = read(); (void)x; }
"#;
        assert!(
            check("cpp", ordinary).is_empty(),
            "{:?}",
            check("cpp", ordinary)
        );
    }

    // ── the identifier groups ───────────────────────────────────────────────

    #[test]
    fn opening_a_file_is_caught_by_name_and_by_type() {
        assert!(!check("cpp", "int main(){ auto f = fopen(\"x\", \"r\"); }").is_empty());
        assert!(!check("cpp", "#include <bits/stdc++.h>\nifstream in;").is_empty());
    }

    #[test]
    fn a_qualified_name_is_matched_with_whitespace_between_the_colons() {
        for written in ["std::thread t;", "std :: thread t;", "std::  thread t;"] {
            assert!(
                !check("cpp", written).is_empty(),
                "{written} was not caught"
            );
        }
    }

    /// Decision D-8. `remove` collides with `std::remove` and `std::remove_if`,
    /// which are entirely legal, so the group is off and a correct solution
    /// using them passes.
    #[test]
    fn the_group_that_collides_with_the_standard_library_is_off() {
        let legal = r#"
#include <bits/stdc++.h>
int main() { std::vector<int> v; v.erase(std::remove(v.begin(), v.end(), 3), v.end()); }
"#;
        assert!(check("cpp", legal).is_empty(), "{:?}", check("cpp", legal));
    }

    // ── the matching rules that decide the false-positive rate ──────────────

    /// Rule 1 of §7. A comment is not code, and failing somebody for writing
    /// "nie używam fork" would be the most embarrassing possible false
    /// positive.
    #[test]
    fn a_forbidden_word_in_a_comment_or_a_string_does_not_match() {
        let innocent = r#"
// nie używam system() ani fopen
/* a tu też nie: dlopen */
#include <iostream>
int main() { std::cout << "system fopen dlopen\n"; }
"#;
        assert!(
            check("cpp", innocent).is_empty(),
            "{:?}",
            check("cpp", innocent)
        );
    }

    #[test]
    fn matching_is_on_whole_tokens() {
        assert!(check("cpp", "int myfopen(); int fopen_count;").is_empty());
    }

    /// Rule 6 of §7: every match, not the first. A participant fixing one word
    /// at a time across five submissions is a worse outcome than one clear list.
    #[test]
    fn every_match_is_reported_with_the_line_it_was_on() {
        let several =
            "#include <unistd.h>\nint main(){\n  system(\"ls\");\n  getenv(\"PATH\");\n}\n";
        let found = check("cpp", several);

        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(
            found.iter().map(|v| v.line).collect::<Vec<_>>(),
            vec![1, 3, 4]
        );
    }

    /// Rule 8: two Runners must produce the same report for the same
    /// submission, so the order is the profile's order and then the file's.
    #[test]
    fn the_report_is_in_the_same_order_every_time() {
        let source =
            "#include <dlfcn.h>\nint main(){ system(\"\"); dlopen(\"\",0); getenv(\"\"); }\n";
        let first = check("cpp", source);
        let again = check("cpp", source);
        assert_eq!(first, again);
        assert!(first.len() >= 3, "{first:?}");
    }

    // ── Python ──────────────────────────────────────────────────────────────

    #[test]
    fn a_denied_module_is_caught_however_it_is_imported() {
        assert!(!check("python", "import os\n").is_empty());
        assert!(!check("python", "from os import system\n").is_empty());
        assert!(
            !check("python", "import os.path\n").is_empty(),
            "a submodule goes with its module"
        );
    }

    #[test]
    fn the_modules_a_solution_actually_needs_stay_permitted() {
        let ordinary =
            "import sys\nimport gc\ngc.disable()\ndata = sys.stdin.read()\nprint(len(data))\n";
        assert!(
            check("python", ordinary).is_empty(),
            "{:?}",
            check("python", ordinary)
        );
    }

    #[test]
    fn a_denied_builtin_is_caught_where_it_is_called_and_not_where_it_is_a_name() {
        assert!(!check("python", "f = open('x')\n").is_empty());
        assert!(
            check("python", "open = 3\nprint(open)\n").is_empty(),
            "a local called `open` is not the built-in",
        );
    }

    #[test]
    fn a_forbidden_word_in_a_python_string_or_comment_does_not_match() {
        let innocent = "# import os\ns = '''import subprocess'''\nt = \"open(\"\nprint(s, t)\n";
        assert!(
            check("python", innocent).is_empty(),
            "{:?}",
            check("python", innocent)
        );
    }

    // ── the boundary ────────────────────────────────────────────────────────

    /// Carried in the profile, deliberately not enforced: nothing can submit
    /// Rust or Java, so a rule for them could never be tripped or tested.
    ///
    /// The boundary moved when `check` started taking a `Language`. It is no
    /// longer possible to *ask* about Rust — there is no such toolchain to ask
    /// with — which is a better answer than the old one, where an unsubmittable
    /// language and a misspelled one were both silently clean.
    #[test]
    fn languages_that_cannot_be_submitted_have_no_way_in() {
        let images = Images::default();

        for carried in ["rust", "java"] {
            assert!(
                Dictionary::built_in().languages.contains_key(carried),
                "{carried} is still carried in the profile",
            );
            assert!(
                for_id(carried, &images).is_none(),
                "{carried} must not be submittable",
            );
        }

        assert!(for_id("cobol", &images).is_none());
    }
}
