//! Judging a real package, with a real compiler, in real containers.
//!
//! Everything under `src/` is tested in isolation and none of it proves the
//! thing that matters: that a submission goes in and a correct mark comes out.
//! This does, and it is the only test here that can.
//!
//! ```text
//! docker build -t algojudge/lang-cpp:local    images/cpp
//! docker build -t algojudge/lang-python:local images/python
//! AJ_DOCKER_SOCKET=1 ./x test -p aj-standard-io --test judging -- --include-ignored --test-threads=1
//! ```

use std::path::{Path, PathBuf};

use aj_package::{Config, TestSet};
use aj_sandbox::{Docker, Sandbox};
use aj_standard_io::{Evaluated, Images, Job, Pipeline, Places};

const CONFIG: &str = r#"
format: standard-io
version: 1
limits:
  timeMs: 2000
  memoryKib: 262144
groups:
  - group: 0
    points: 0
    examples: true
  - group: 1
    points: 40
  - group: 2
    points: 60
"#;

/// Adds two numbers, correctly.
const CORRECT_CPP: &str = r#"
#include <iostream>
int main() { long long a, b; std::cin >> a >> b; std::cout << a + b << "\n"; }
"#;

const CORRECT_PYTHON: &str = "a, b = input().split()\nprint(int(a) + int(b))\n";

async fn pipeline() -> Pipeline<Docker> {
    let docker = Docker::connect().expect("a container runtime");
    if let Err(e) = docker.preflight().await {
        assert!(
            std::env::var("AJ_SANDBOX_ALLOW_CGROUP_V1").is_ok(),
            "{e}\n\nSet AJ_SANDBOX_ALLOW_CGROUP_V1=1 to judge on this host anyway.",
        );
    }
    let images = Images::default();
    for image in [&images.cpp, &images.python] {
        docker
            .ensure_image(image)
            .await
            .unwrap_or_else(|e| panic!("{image} is not built: {e}\nbuild it from images/"));
    }
    docker.sweep().await.expect("a clean slate");

    Pipeline::new(docker, images)
}

/// A package on disk, in both the views a bind mount needs.
fn package(name: &str) -> (Places, Config, TestSet) {
    let (here, on_the_host) = fixture(name);
    std::fs::create_dir_all(here.join("tests")).unwrap();
    std::fs::write(here.join("config.yml"), CONFIG).unwrap();

    for (test, input, expected) in [
        ("0a", "1 2\n", "3\n"),
        ("1a", "10 20\n", "30\n"),
        ("2a", "1000000 2000000\n", "3000000\n"),
    ] {
        std::fs::write(here.join(format!("tests/{test}.in")), input).unwrap();
        std::fs::write(here.join(format!("tests/{test}.out")), expected).unwrap();
    }

    let config = Config::parse(CONFIG).unwrap();
    let tests = TestSet::read(&here, &config).unwrap();

    (
        Places {
            here,
            on_host: on_the_host,
        },
        config,
        tests,
    )
}

fn work(name: &str) -> Places {
    let (here, on_the_host) = fixture(&format!("{name}-work"));
    Places {
        here,
        on_host: on_the_host,
    }
}

/// See `aj-sandbox`'s adversarial suite: a bind mount is resolved by the
/// **daemon**, so a path that is real to this process and meaningless to the
/// daemon silently produces an empty directory.
fn fixture(name: &str) -> (PathBuf, PathBuf) {
    let relative = format!(".sandbox-fixtures/judging-{name}");

    let here = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(&relative);
    let _ = std::fs::remove_dir_all(&here);
    std::fs::create_dir_all(&here).expect("a fixture directory");

    let on_the_host = match std::env::var("AJ_HOST_WORKDIR") {
        Ok(root) => {
            let separator = if root.contains('\\') { "\\" } else { "/" };
            PathBuf::from(root).join(relative.replace('/', separator))
        }
        Err(_) => here.clone(),
    };
    (here, on_the_host)
}

async fn judge(name: &str, language: &str, source: &str) -> Evaluated {
    let pipeline = pipeline().await;
    let (package, config, tests) = package(name);

    pipeline
        .evaluate(&Job {
            config: &config,
            tests: &tests,
            language,
            source: source.as_bytes(),
            package,
            work: work(name),
        })
        .await
}

fn verdict(evaluated: Evaluated) -> aj_standard_io::Verdict {
    match evaluated {
        Evaluated::Judged(verdict) => {
            // Printed so that a failing assertion is readable. Without the
            // build's own words, "compilation error" says nothing about why.
            if verdict.details.compilation.log.contains("error")
                || !verdict.details.compilation.log.is_empty()
            {
                eprintln!("--- build said ---\n{}", verdict.details.compilation.log);
            }
            *verdict
        }
        Evaluated::Failed(reason) => panic!("the evaluation failed: {reason}"),
    }
}

// ── The one that matters ────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_correct_cpp_solution_is_accepted_with_full_marks() {
    let judged = verdict(judge("cpp-correct", "cpp", CORRECT_CPP).await);

    assert_eq!(judged.judgement.verdict, "Accepted");
    assert_eq!(judged.judgement.score, 100.0);
    assert_eq!(judged.judgement.max_score, 100.0);

    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();
    assert_eq!(document["kind"], "standard-io");
    assert_eq!(document["compilation"]["status"], "OK");
    assert_eq!(document["tests"].as_array().unwrap().len(), 3);
    assert!(
        document["tests"][0]["timeMs"].as_u64().unwrap() < 2000,
        "a trivial program should not have taken the whole limit",
    );
}

#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_correct_python_solution_is_accepted() {
    let judged = verdict(judge("python-correct", "python", CORRECT_PYTHON).await);

    assert_eq!(judged.judgement.verdict, "Accepted");
    assert_eq!(judged.judgement.score, 100.0);
}

// ── Every other outcome a participant can get ───────────────────────────────

/// Wrong on one test of one group. The group rule then takes that group to
/// zero and leaves the other alone.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_wrong_answer_loses_its_group_and_only_its_group() {
    let wrong = r#"
#include <iostream>
int main() { long long a, b; std::cin >> a >> b; std::cout << (a > 100 ? 0 : a + b) << "\n"; }
"#;
    let judged = verdict(judge("cpp-wrong", "cpp", wrong).await);

    // Group 2's test uses numbers over 100, so only it is wrong.
    assert_eq!(judged.judgement.score, 40.0, "groups 0 and 1 are untouched");
    assert_ne!(judged.judgement.verdict, "Accepted");

    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();
    let group2 = document["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["group"] == 2)
        .unwrap()
        .clone();
    assert_eq!(group2["points"], 0.0);
    assert_eq!(group2["status"], "ERROR");
}

#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_program_that_never_stops_is_a_time_limit() {
    let looping = r#"
#include <iostream>
int main() { long long a, b; std::cin >> a >> b; while (true) { } }
"#;
    let judged = verdict(judge("cpp-loop", "cpp", looping).await);

    assert_eq!(judged.judgement.score, 0.0);
    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();
    assert!(
        document["tests"][0]["note"]
            .as_str()
            .unwrap()
            .contains("Time limit exceeded"),
        "got {}",
        document["tests"][0]["note"],
    );
}

/// A crash and a timeout are different things to be told, and they are decided
/// in different places: the wall clock is the Runner killing the container, the
/// crash is the exit code of a container that stopped on its own. This asserts
/// they do not bleed into each other — a segmentation fault must not be
/// reported as a time limit, nor the other way round.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_program_that_crashes_is_a_runtime_error_and_not_a_time_limit() {
    let crashing = r#"
#include <iostream>
int main() { long long a, b; std::cin >> a >> b; volatile int *p = nullptr; *p = 1; }
"#;
    let judged = verdict(judge("cpp-crash", "cpp", crashing).await);

    assert_eq!(judged.judgement.score, 0.0);
    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();
    let note = document["tests"][0]["note"].as_str().unwrap().to_owned();

    assert!(
        note.contains("segmentation fault"),
        "a crash should say what kind, got {note}",
    );
    assert!(
        !note.contains("Time limit exceeded"),
        "a crash must not be reported as a time limit, got {note}",
    );
}

/// A submission that does not build is a **verdict**, not an infrastructure
/// failure, and the compiler's own words reach the participant.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_submission_that_does_not_build_says_why() {
    let judged = verdict(judge("cpp-broken", "cpp", "int main() { this is not c++ }").await);

    assert_eq!(judged.judgement.verdict, "Compilation error");
    assert_eq!(judged.judgement.score, 0.0);

    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();
    assert_eq!(document["compilation"]["status"], "ERROR");
    assert!(
        document["compilation"]["log"]
            .as_str()
            .unwrap()
            .contains("error"),
        "the compiler's own words are the participant's most useful artefact",
    );
}

/// Python's build step exists precisely for this: without it a missing colon
/// fails every test with the same traceback instead of once, legibly.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_python_syntax_error_is_a_compilation_error_and_not_three_failures() {
    let judged = verdict(judge("python-broken", "python", "if True\n  print(1)\n").await);

    assert_eq!(judged.judgement.verdict, "Compilation error");

    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();
    assert_eq!(document["compilation"]["status"], "ERROR");
    assert!(document["compilation"]["log"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("syntax"),);
}

/// A submission that reaches for the network never gets as far as the sandbox.
///
/// This test asserted the opposite first — that the program compiles, fails to
/// connect, and passes — and it **could not be written that way any more**:
/// opening a socket in C++ needs `<sys/socket.h>`, `<netinet/in.h>` and
/// `<arpa/inet.h>`, and all three are on the header deny-list. The dictionary
/// stops it at the source.
///
/// That is a policy control catching it early, **not** the isolation. The
/// isolation is asserted where it belongs, against a shell that needs no
/// headers: `aj-sandbox`'s `there_is_no_network`. Both matter, and they are
/// different claims — a bypass of this one is expected, and the other one is
/// what actually holds.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_submission_reaching_for_the_network_is_stopped_before_it_is_built() {
    let reaching = r#"
#include <iostream>
#include <sys/socket.h>
#include <arpa/inet.h>
int main() {
    long long a, b; std::cin >> a >> b;
    int s = socket(AF_INET, SOCK_STREAM, 0);
    std::cout << (s < 0 ? a + b : -1);
}
"#;
    let judged = verdict(judge("cpp-network", "cpp", reaching).await);

    assert_eq!(judged.judgement.verdict, "PolicyViolation");

    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();
    let said = document["compilation"]["log"].as_str().unwrap();
    assert!(said.contains("sys/socket.h"), "{said}");
    assert!(said.contains("arpa/inet.h"), "{said}");
}

// ── The activity's rules ────────────────────────────────────────────────────

/// Never compiled, never run, and told which rule it broke.
///
/// The three outcomes a participant must be able to tell apart are "your answer
/// is wrong", "your code does not build" and "your code is not allowed". This
/// is the third, and it is neither of the other two.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_submission_that_breaks_the_rules_is_not_compiled() {
    let forbidden = r#"
#include <iostream>
#include <unistd.h>
int main() { long long a, b; std::cin >> a >> b; system("ls"); std::cout << a + b; }
"#;
    let judged = verdict(judge("cpp-policy", "cpp", forbidden).await);

    assert_eq!(judged.judgement.verdict, "PolicyViolation");
    assert_eq!(judged.judgement.score, 0.0);

    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();
    // Not an ERROR: nothing failed to compile, because nothing reached a
    // compiler.
    assert_eq!(document["compilation"]["status"], "WARNING");

    let said = document["compilation"]["log"].as_str().unwrap();
    assert!(
        said.contains("unistd.h"),
        "the header rule is not named: {said}"
    );
    assert!(
        said.contains("Uruchamianie innego programu"),
        "the participant is told the rule's name, not its letter: {said}",
    );
    assert!(said.contains("wiersz"), "and where it was: {said}");
}

/// A correct solution that merely *mentions* a forbidden word in a comment is
/// not a violation. This is the false positive the whole design exists to
/// avoid, and it is worth asserting through the pipeline and not only in a unit
/// test.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_comment_about_a_forbidden_call_is_not_a_violation() {
    let innocent = r#"
#include <iostream>
// nie uzywam system() ani fopen - czytam ze standardowego wejscia
int main() { long long a, b; std::cin >> a >> b; std::cout << a + b; }
"#;
    let judged = verdict(judge("cpp-comment", "cpp", innocent).await);

    assert_eq!(judged.judgement.verdict, "Accepted");
}

// ── The committed package ───────────────────────────────────────────────────

/// Judges the archive in `fixtures/`, through the real extraction path.
///
/// Every other test here builds a package as a directory, which skips the part
/// where a Runner is handed a zip by a Server. This one does not: it is the
/// closest thing to a real job that does not need a Server.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn the_committed_package_judges_a_correct_solution() {
    let pipeline = pipeline().await;

    let archive = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sum.zip");
    let (here, on_the_host) = fixture("archive");
    let unpacked = here.join("package");

    let files = aj_package::extract(&archive, &unpacked, &aj_package::ArchiveLimits::default())
        .expect("the committed package must unpack");
    assert_eq!(
        files, 14,
        "a config, ten test files, a checker and two model solutions"
    );

    let declared = std::fs::read_to_string(unpacked.join("config.yml")).unwrap();
    let config = Config::parse(&declared).expect("the committed config must read");
    let tests = TestSet::read(&unpacked, &config).expect("the committed tests must read");
    assert_eq!(tests.len(), 5);
    assert_eq!(config.max_score(), 100);

    let evaluated = pipeline
        .evaluate(&Job {
            config: &config,
            tests: &tests,
            language: "cpp",
            source: CORRECT_CPP.as_bytes(),
            package: Places {
                here: unpacked,
                on_host: on_the_host.join("package"),
            },
            work: work("archive"),
        })
        .await;

    let judged = verdict(evaluated);
    assert_eq!(judged.judgement.verdict, "Accepted");
    assert_eq!(judged.judgement.score, 100.0);

    // The package declares a checker, so this also proves the checker was
    // built, run in its own sandbox, and read according to the SIO2 contract.
    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();
    assert_eq!(document["tests"].as_array().unwrap().len(), 5);
}
