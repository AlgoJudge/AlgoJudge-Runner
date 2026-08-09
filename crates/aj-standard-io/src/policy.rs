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
//! Two languages are enforced — C++ and Python, the two the Runner judges. Rust
//! and Java are carried in the file and are **not** applied, because enforcing a
//! dictionary for a language nothing can submit would be a rule nobody could
//! trip and nobody could test.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use indexmap::IndexMap;
use regex::Regex;
use serde::Deserialize;

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
    /// What the participant is told. Not the group's letter — "Otwieranie
    /// pliku" teaches something, "F" does not.
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
        format!("{} — {} (wiersz {})", self.rule, self.matched, self.line)
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
    pub fn check(&self, language: &str, source: &str) -> Vec<Violation> {
        let Some(rules) = self.languages.get(language) else {
            return Vec::new();
        };

        let stripped = strip(source, language, &self.matching);
        let mut found = Vec::new();

        match language {
            "cpp" => {
                headers(&stripped, rules, &mut found);
                // Identifiers are matched against everything **except** the
                // include directives, which are rule H's business alone (§7.4).
                // Otherwise `#include <asm/unistd.h>` also trips the inline-
                // assembly group, because `/` is a word boundary — a false
                // positive on a line that was already correctly reported once.
                groups(&without_includes(&stripped), rules, &mut found);
            }
            "python" => {
                imports(&stripped, rules, &mut found);
                builtins(&stripped, rules, &mut found);
                patterns(
                    &stripped,
                    &rules.denied_patterns,
                    "Zakazane wywołanie",
                    &mut found,
                );
            }
            // Carried in the file, not enforced. Saying so here is better than a
            // silent `_ => {}` that reads as "checked and clean".
            _ => tracing::debug!(language, "no policy rules are enforced for this language"),
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
fn strip(source: &str, language: &str, matching: &Matching) -> String {
    let bytes: Vec<char> = source.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(bytes.len());
    let mut i = 0;

    let line_comment = if language == "python" { "#" } else { "//" };

    while i < bytes.len() {
        let rest: String = bytes[i..bytes.len().min(i + 3)].iter().collect();

        if matching.strip_comments && rest.starts_with(line_comment) {
            while i < bytes.len() && bytes[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if matching.strip_comments && language != "python" && rest.starts_with("/*") {
            while i < bytes.len() {
                let two: String = bytes[i..bytes.len().min(i + 2)].iter().collect();
                let end = two.starts_with("*/");
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
            let triple = language == "python"
                && bytes.len() > i + 2
                && bytes[i + 1] == quote
                && bytes[i + 2] == quote;

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

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

/// `#include <x>` — matched as a directive, never as a word.
fn headers(source: &str, rules: &LanguageRules, found: &mut Vec<Violation>) {
    for capture in include_pattern().captures_iter(source) {
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
                rule: format!("Zakazany nagłówek <{named}>"),
                matched: named.to_owned(),
                line: line_of(source, capture.get(0).expect("the whole match").start()),
            });
        }
    }
}

fn groups(source: &str, rules: &LanguageRules, found: &mut Vec<Violation>) {
    for group in rules.groups.values() {
        // A group that is off is off. `remove` collides with `std::remove`,
        // which is entirely legal, and a rule that fails correct solutions is
        // worse than a rule that misses something the sandbox catches anyway.
        if !group.default {
            continue;
        }
        for identifier in &group.identifiers {
            whole_word(source, identifier, &group.name, found);
        }
        patterns(source, &group.patterns, &group.name, found);
    }
}

fn whole_word(source: &str, word: &str, rule: &str, found: &mut Vec<Violation>) {
    let Ok(pattern) = Regex::new(&format!(r"\b{}\b", regex::escape(word))) else {
        return;
    };
    for hit in pattern.find_iter(source) {
        found.push(Violation {
            rule: rule.to_owned(),
            matched: word.to_owned(),
            line: line_of(source, hit.start()),
        });
    }
}

fn patterns(source: &str, patterns: &[String], rule: &str, found: &mut Vec<Violation>) {
    for declared in patterns {
        // A pattern that will not compile is a mistake in the profile, not in
        // the submission. Skipped and logged rather than failing somebody's
        // solution over it.
        let Ok(pattern) = Regex::new(declared) else {
            tracing::warn!(pattern = %declared, "a policy pattern will not compile");
            continue;
        };
        for hit in pattern.find_iter(source) {
            found.push(Violation {
                rule: rule.to_owned(),
                matched: hit.as_str().trim().to_owned(),
                line: line_of(source, hit.start()),
            });
        }
    }
}

/// `import x`, `from x import`, `__import__("x")`.
fn imports(source: &str, rules: &LanguageRules, found: &mut Vec<Violation>) {
    let denied: BTreeSet<&str> = rules.denied_imports.iter().map(String::as_str).collect();

    static IMPORT: OnceLock<Regex> = OnceLock::new();
    let import = IMPORT.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:from\s+([A-Za-z_][\w.]*)|import\s+([A-Za-z_][\w.]*))")
            .expect("a fixed pattern")
    });

    for capture in import.captures_iter(source) {
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
                rule: format!("Zakazany moduł {root}"),
                matched: named.to_owned(),
                line: line_of(source, capture.get(0).expect("the whole match").start()),
            });
        }
    }
}

fn builtins(source: &str, rules: &LanguageRules, found: &mut Vec<Violation>) {
    for builtin in &rules.denied_builtins {
        // Only where it is called: `open` as a variable name is not the
        // built-in, and failing a solution over a local called `open` is the
        // false positive this whole design is trying to avoid.
        let Ok(pattern) = Regex::new(&format!(r"\b{}\s*\(", regex::escape(builtin))) else {
            continue;
        };
        for hit in pattern.find_iter(source) {
            found.push(Violation {
                rule: format!("Zakazana funkcja wbudowana {builtin}"),
                matched: builtin.clone(),
                line: line_of(source, hit.start()),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(language: &str, source: &str) -> Vec<Violation> {
        Dictionary::built_in().check(language, source)
    }

    #[test]
    fn the_shipped_profile_parses_and_says_what_it_is() {
        let dictionary = Dictionary::built_in();
        assert_eq!(dictionary.id, "standard-io/default@1");
        assert_eq!(dictionary.version, 1);
        assert!(dictionary.languages.contains_key("cpp"));
        assert!(dictionary.languages.contains_key("python"));
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
    #[test]
    fn languages_that_cannot_be_submitted_are_not_enforced() {
        assert!(check("rust", "use std::fs::File;\n").is_empty());
        assert!(check("java", "new java.io.File(\"x\");\n").is_empty());
        assert!(check("cobol", "anything at all").is_empty());
    }
}
