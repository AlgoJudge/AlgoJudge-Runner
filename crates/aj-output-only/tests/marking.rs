//! Marking answers, without a container in sight.
//!
//! `output-only@1` had no test of its own until 2026-08-30, which is an odd gap
//! for the crate that exists **to be evidence**: it is the second problem type,
//! and the claim it was written to check is that adding one costs the Server
//! nothing. Evidence nobody runs is not evidence.
//!
//! The whole handler is exercised here and **none of it needs Docker**, because
//! nothing executes — the participant sent the answers. That is the property
//! that makes this type the safest in the product, and it is why these are
//! ordinary tests rather than the `#[ignore]`d kind the sandbox needs.

use std::path::{Path, PathBuf};

use aj_output_only::{details, mark, Answers};
use aj_package::{Config, TestSet};
use aj_standard_io::score::{Judgement, Reason, Status};

/// Two groups worth 40 and 60, so a partial mark is distinguishable from both
/// zero and full — with one group, a wrong answer and a missing one would look
/// alike and half these tests would pass for the wrong reason.
const CONFIG: &str = r#"
type: "output-only@1"
limits:
  timeMs: 1000
  memoryBytes: 268435456
groups:
  - group: 1
    points: 40
  - group: 2
    points: 60
"#;

/// One test's working directory, emptied once so a previous run cannot leak
/// into this one. **Called once per test**: it deletes, so a second call would
/// take the package away from the answers that were about to be marked
/// against it.
fn case(name: &str) -> PathBuf {
    let mut root = std::env::temp_dir();
    root.push(format!("aj-output-only-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// A package with one test in each group: `1a` expects `A`, `2a` expects `B`.
fn package(case: &Path) -> (PathBuf, Config, TestSet) {
    let root = case.join("package");
    std::fs::create_dir_all(root.join("tests")).unwrap();
    for (test, expected) in [("1a", "A\n"), ("2a", "B\n")] {
        std::fs::write(root.join(format!("tests/{test}.in")), "ignored\n").unwrap();
        std::fs::write(root.join(format!("tests/{test}.out")), expected).unwrap();
    }
    let config = Config::parse_as(CONFIG, "output-only").unwrap();
    let tests = TestSet::read(&root, &config).unwrap();
    (root, config, tests)
}

/// An answer archive, unpacked as a submission would be.
fn answers(case: &Path, files: &[(&str, &str)]) -> Answers {
    let zipped = case.join("answers.zip");
    let file = std::fs::File::create(&zipped).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (path, body) in files {
        use std::io::Write;
        zip.start_file(*path, options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap();

    Answers::unpack(&zipped, &case.join("unpacked")).unwrap()
}

fn note_for<'a>(judged: &'a Judgement, test: &str) -> &'a str {
    &judged
        .tests
        .iter()
        .find(|t| t.outcome.name == test)
        .expect("the test is in the judgement")
        .outcome
        .note
}

#[test]
fn a_correct_answer_set_scores_full_marks() {
    let case = case("correct");
    let (root, config, tests) = package(&case);
    let given = answers(&case, &[("1a.out", "A\n"), ("2a.out", "B\n")]);

    let judged = mark(&root, &config, &tests, &given);

    assert_eq!(judged.score, 100.0);
    assert_eq!(judged.max_score, 100.0);
    assert!(judged.tests.iter().all(|t| t.outcome.status == Status::Ok));
}

#[test]
fn a_wrong_answer_scores_zero_and_says_the_answer_was_the_problem() {
    let case = case("wrong");
    let (root, config, tests) = package(&case);
    let given = answers(&case, &[("1a.out", "A\n"), ("2a.out", "not B\n")]);

    let judged = mark(&root, &config, &tests, &given);

    // 40 of 100: group 1 stands, group 2 does not.
    assert_eq!(judged.score, 40.0);
    let failed = judged
        .tests
        .iter()
        .find(|t| t.outcome.name == "2a")
        .unwrap();
    assert_eq!(failed.outcome.status, Status::Error);
    // Nothing ran, so `WrongAnswer` is the only reason available; any other
    // would mean the handler had invented a failure that could not happen.
    assert_eq!(failed.outcome.reason, Some(Reason::WrongAnswer));
}

#[test]
fn a_missing_answer_costs_only_its_own_test() {
    let case = case("partial");
    let (root, config, tests) = package(&case);
    let given = answers(&case, &[("1a.out", "A\n")]);

    let judged = mark(&root, &config, &tests, &given);

    // The behaviour the handler's own comment promises: somebody who answered
    // one of two keeps the one, rather than the upload failing as a whole.
    assert_eq!(judged.score, 40.0);
    assert_eq!(
        note_for(&judged, "2a"),
        "no answer was uploaded for this test"
    );
}

#[test]
fn a_package_missing_its_expected_output_says_so_rather_than_blaming_the_answer() {
    let case = case("no-expected");
    let (root, config, tests) = package(&case);
    // Read the package first and break it after: that is the only way to reach
    // the branch that reports a broken package, because `TestSet::read` refuses
    // a test with no expected output before `mark` ever sees it.
    std::fs::remove_file(root.join("tests/2a.out")).unwrap();
    let given = answers(&case, &[("1a.out", "A\n"), ("2a.out", "B\n")]);

    let judged = mark(&root, &config, &tests, &given);

    assert_eq!(judged.score, 40.0);
    assert_eq!(
        note_for(&judged, "2a"),
        "the package has no expected output for this test"
    );
}

#[test]
fn answers_zipped_as_a_folder_are_found_too() {
    let case = case("nested");
    let (root, config, tests) = package(&case);
    // Selecting two files and zipping the folder that holds them are both
    // ordinary things to do, and the second is the more common one.
    let given = answers(
        &case,
        &[("answers/1a.out", "A\n"), ("answers/2a.out", "B\n")],
    );

    let judged = mark(&root, &config, &tests, &given);

    assert_eq!(judged.score, 100.0);
}

#[test]
fn one_bare_file_is_an_answer_set_for_a_one_test_problem() {
    let case = case("single");
    let (root, config, tests) = package(&case);
    let given = Answers::single(b"A\n", "1a", &case.join("bare")).unwrap();

    let judged = mark(&root, &config, &tests, &given);

    // It answers `1a` and nothing else, which is what one file can do.
    assert_eq!(judged.score, 40.0);
    assert_eq!(
        note_for(&judged, "2a"),
        "no answer was uploaded for this test"
    );
}

#[test]
fn the_document_names_the_type_and_refuses_to_imply_a_compilation() {
    let case = case("document");
    let (root, config, tests) = package(&case);
    let given = answers(&case, &[("1a.out", "A\n"), ("2a.out", "B\n")]);

    let document = details(&mark(&root, &config, &tests, &given), &config);

    assert_eq!(document.kind, "output-only@1");
    // `Ok` would tell a participant something was built. Nothing was.
    assert_eq!(document.compilation.status, Status::Warning);
    assert_eq!(
        document.compilation.log,
        "nothing was compiled: this is an output-only problem"
    );
    assert_eq!(document.limits.time_ms, 1000);
}
