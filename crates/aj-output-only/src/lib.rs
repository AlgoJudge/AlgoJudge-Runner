//! `output-only@1` — the participant sends answers, not a program.
//!
//! A second problem type, and the reason it exists is to test a claim rather
//! than to be wanted: **"adding a problem type does not require a Server
//! change"** is an invariant the product has asserted from the beginning and
//! nothing had ever checked. Either this lands with the Server byte-identical,
//! or the invariant is narrower than it says and that is worth knowing.
//!
//! It differs from `standard-io@1` on all three axes that could have needed a
//! Server change, which is why it is the cheapest honest test:
//!
//! - **the submission** is a file, not source in an editor;
//! - **the package** declares no language and needs no compiler;
//! - **the evaluation** runs no untrusted code at all — there is nothing to
//!   sandbox, because the participant sent the answers.
//!
//! That last one makes it the safest type in the product and the one with the
//! least machinery: no build container, no run container, no policy dictionary
//! (there is no code to read), and nothing that can escape because nothing
//! executes.

use std::path::Path;

use aj_package::{Config, TestSet};
use aj_standard_io::compare::compare;
use aj_standard_io::details::{Compilation, Details, Limits};
use aj_standard_io::score::{judge, Judgement, Status, TestOutcome};

/// What the participant sent, unpacked: their answer for each test.
pub struct Answers {
    root: std::path::PathBuf,
}

impl Answers {
    /// Unpacks the submitted archive.
    ///
    /// **The same defences as a package**, and for a better reason: this archive
    /// came from a participant rather than from a problem author, so it is not
    /// semi-trusted — it is untrusted, and it is the only untrusted archive the
    /// product opens.
    pub fn unpack(archive: &Path, into: &Path) -> Result<Self, String> {
        aj_package::extract(archive, into, &limits())
            .map_err(|e| format!("the submitted archive could not be opened: {e}"))?;
        Ok(Self {
            root: into.to_path_buf(),
        })
    }

    /// A participant may also send one bare file, for a problem with one test.
    pub fn single(bytes: &[u8], test: &str, into: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(into).map_err(|e| e.to_string())?;
        std::fs::write(into.join(format!("{test}.out")), bytes).map_err(|e| e.to_string())?;
        Ok(Self {
            root: into.to_path_buf(),
        })
    }

    fn answer_to(&self, test: &str) -> Option<Vec<u8>> {
        // Flat or one directory deep: an archive made by selecting files and one
        // made by zipping a folder both arrive, and refusing the second would be
        // refusing the more common one.
        let flat = self.root.join(format!("{test}.out"));
        if let Ok(bytes) = std::fs::read(&flat) {
            return Some(bytes);
        }
        let nested = std::fs::read_dir(&self.root).ok()?;
        for entry in nested.flatten() {
            let inside = entry.path().join(format!("{test}.out"));
            if let Ok(bytes) = std::fs::read(&inside) {
                return Some(bytes);
            }
        }
        None
    }
}

/// Limits for an archive a **participant** sent.
///
/// Tighter than a package's: an answer set is a few files of text, and the
/// ceiling that matters here is the one the activity already enforces on the
/// upload. These stop a zip bomb from being unpacked at all.
fn limits() -> aj_package::ArchiveLimits {
    aj_package::ArchiveLimits {
        max_entries: 2_000,
        max_entry_bytes: 64 * 1024 * 1024,
        max_total_bytes: 256 * 1024 * 1024,
        max_ratio: 200,
        max_path_length: 255,
    }
}

/// Marks a set of answers against the package's expected output.
///
/// No checker yet: the type is here to test the Server's boundary, and a checker
/// would add a sandbox to a handler whose whole point is that it needs none.
/// When one is wanted it is the same contract as `standard-io@1` — the checker
/// module is already shared.
pub fn mark(package: &Path, config: &Config, tests: &TestSet, answers: &Answers) -> Judgement {
    let mut outcomes = Vec::new();

    for test in tests.iter() {
        let expected = match std::fs::read(&test.expected) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::error!(test = %test.name, %e, "the package's expected output is unreadable");
                outcomes.push(missing(test, "Brak oczekiwanego wyniku w paczce"));
                continue;
            }
        };

        let Some(given) = answers.answer_to(&test.name) else {
            // Not an error and not a crash: a participant who answered four of
            // five tests gets four tests' worth of credit and is told which one
            // is missing.
            outcomes.push(missing(test, "Brak odpowiedzi dla tego testu"));
            continue;
        };

        let found = compare(&expected, &given);
        outcomes.push(TestOutcome {
            name: test.name.clone(),
            group: test.group,
            status: if found.equal() {
                Status::Ok
            } else {
                Status::Error
            },
            percentage: if found.equal() { 100 } else { 0 },
            // Nothing ran, so there is no time to report. Zero is honest here
            // in a way it would not be for a program.
            time_ms: 0,
            memory_kib: None,
            note: found.note(),
        });
    }

    let _ = package;
    let _ = config;
    judge(config, tests, &outcomes)
}

fn missing(test: &aj_package::Test, why: &str) -> TestOutcome {
    TestOutcome {
        name: test.name.clone(),
        group: test.group,
        status: Status::Error,
        percentage: 0,
        time_ms: 0,
        memory_kib: None,
        note: why.to_owned(),
    }
}

/// The document a Client renders. Same schema as `standard-io@1` apart from its
/// `kind`, because a per-test table is a per-test table.
pub fn details(judged: &Judgement, config: &Config) -> Details {
    let mut document = Details::of(
        judged,
        Limits {
            time_ms: config.limits.time_ms,
            memory_mb: config.limits.memory_kib / 1024,
        },
        Compilation {
            // Nothing was compiled, and saying `OK` would imply something was.
            status: Status::Warning,
            log: "nothing was compiled: this is an output-only problem".into(),
        },
    );
    document.kind = "output-only";
    document
}
