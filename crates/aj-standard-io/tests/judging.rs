//! Judging a real package, with a real compiler, in real containers.
//!
//! Everything under `src/` is tested in isolation and none of it proves the
//! thing that matters: that a submission goes in and a correct mark comes out.
//! This does, and it is the only test here that can.
//!
//! **Nothing builds the four language images for you**, and every case here
//! fails at its first line without them:
//!
//! ```text
//! docker build -t algojudge/lang-gcc:local    images/gcc
//! docker build -t algojudge/lang-clang:local  images/clang
//! docker build -t algojudge/lang-python:local images/python
//! docker build -t algojudge/lang-pypy:local   images/pypy
//! AJ_DOCKER_SOCKET=1 ./x test -p aj-standard-io --test judging -- --include-ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` is not a preference: run in parallel these fight over the
//! container runtime and all of them fail.

use std::path::{Path, PathBuf};

use aj_package::{Config, TestSet};
use aj_sandbox::{Docker, Sandbox};
use aj_standard_io::{catalogue, for_id, Evaluated, Family, Images, Job, Pipeline, Places};

const CONFIG: &str = r#"
type: "standard-io@1"
limits:
  timeMs: 2000
  memoryBytes: 268435456
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

/// The same program in C, written to the **oldest** standard the catalogue
/// offers so that one source serves all eight C rows.
///
/// `long` rather than `long long`, declarations before statements, no `//`
/// comment: `-std=c89 -pedantic-errors` rejects each of those, and the largest
/// sum this package asks for is three million, which fits a 32-bit `long`.
const CORRECT_C: &str = r#"
#include <stdio.h>
int main(void) {
    long a, b;
    if (scanf("%ld %ld", &a, &b) != 2) return 1;
    printf("%ld\n", a + b);
    return 0;
}
"#;

async fn pipeline() -> Pipeline<Docker> {
    // This suite's own name, so the clean slate below is this suite's and not a
    // Runner's that happens to be judging on the same host. Fixed rather than
    // per-run, so a previous run's leftovers are still swept.
    let docker = Docker::connect("test-judging").expect("a container runtime");
    // **No override here, and there used to be one.** This suite judges, and a
    // verdict is made of processor time read from the run's own cgroup — so
    // continuing past a refused preflight would make every case below an
    // infrastructure failure, and each would report that the evaluation failed
    // rather than that the host cannot measure. Fail here, where the reason is.
    docker.preflight().await.expect(
        "this suite judges, and judging needs a host that can measure processor time: \
         cgroup v2, a cgroup driver this Runner knows — cgroupfs or systemd — and a tree it \
         can use. See docs/CGROUP_V2.md — ./x mounts it only with AJ_DOCKER_SOCKET=1",
    );
    let images = Images::default();
    for image in images.all() {
        docker
            .ensure_image(&image)
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

/// A file name the toolchain accepts, from the catalogue rather than guessed.
///
/// This suite is about judging and not about the extension check, so every job
/// in it carries a name that matches what it says it is.
fn file_named(language: &str) -> &'static str {
    let resolved = for_id(language, &Images::default())
        .unwrap_or_else(|| panic!("{language} is not a toolchain"));

    match resolved.family {
        Family::C => "main.c",
        Family::Cpp => "main.cpp",
        Family::Python => "main.py",
    }
}

async fn judge(name: &str, language: &str, source: &str) -> Evaluated {
    let pipeline = pipeline().await;
    let (package, config, tests) = package(name);

    pipeline
        .evaluate(&Job {
            config: &config,
            tests: &tests,
            language,
            file_name: file_named(language),
            source: source.as_bytes(),
            package,
            work: work(name),
            outputs: None,
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

/// **A language the assignment excluded is refused here, and nowhere else.**
///
/// The Server used to refuse it, against a list on the activity. It cannot: the
/// language is one member of a document it does not read, so the allowed set
/// travels in the assignment's `config` and the refusal happens where a language
/// id means something.
///
/// `PolicyViolation`, not a compilation error: nothing was offered to a
/// compiler, the code may be perfect, and what was broken is a rule of the
/// activity. It leaves the submission rejudgeable if a manager widens the set.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_language_the_assignment_excluded_is_a_policy_violation() {
    const PYTHON_ONLY: &str = r#"
type: "standard-io@1"
limits:
  timeMs: 2000
  memoryBytes: 268435456
languages: [python3]
groups:
  - group: 0
    points: 0
    examples: true
  - group: 1
    points: 100
"#;

    let pipeline = pipeline().await;
    let (here, on_host) = fixture("excluded-language");
    std::fs::create_dir_all(here.join("tests")).unwrap();
    std::fs::write(here.join("config.yml"), PYTHON_ONLY).unwrap();
    for (test, input, expected) in [
        (
            "0a", "1 2
", "3
",
        ),
        (
            "1a", "10 20
", "30
",
        ),
    ] {
        std::fs::write(here.join(format!("tests/{test}.in")), input).unwrap();
        std::fs::write(here.join(format!("tests/{test}.out")), expected).unwrap();
    }

    let config = Config::parse(PYTHON_ONLY).unwrap();
    let tests = TestSet::read(&here, &config).unwrap();

    let judged = verdict(
        pipeline
            .evaluate(&Job {
                config: &config,
                tests: &tests,
                language: "cpp20-gcc",
                file_name: "main.cpp",
                source: CORRECT_CPP.as_bytes(),
                package: Places { here, on_host },
                work: work("excluded-language"),
                outputs: None,
            })
            .await,
    );

    assert_eq!(judged.judgement.verdict, "PolicyViolation");
    assert_eq!(judged.judgement.score, 0.0);

    let said = &judged.details.compilation.log;
    assert!(
        said.contains("C++20 (GCC)"),
        "the language is named as a person reads it: {said}"
    );
    assert!(
        said.contains("python3"),
        "and what is accepted is listed: {said}"
    );
}

/// The other half of the same rule: an assignment that names no languages allows
/// everything this Runner can build. Empty is "the assignment did not say", not
/// "the assignment allows none".
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn an_assignment_that_names_no_languages_allows_them_all() {
    let judged = verdict(judge("no-language-list", "cpp20-gcc", CORRECT_CPP).await);
    assert_eq!(judged.judgement.verdict, "Accepted");
}

// ── The whole catalogue ─────────────────────────────────────────────────────

/// **Every row of the table, built and run for real.**
///
/// The catalogue is data, and data is exactly the kind of change that looks
/// right and is not: `-std=c23` is a flag GCC 12 rejects and GCC 14 accepts,
/// `-static` needs a static libstdc++ that a Clang image does not get by
/// installing Clang, and `pypy3` is a binary that either is on the path or is
/// not. None of that is visible in a unit test over the strings — the strings
/// are fine in all four failing cases.
///
/// So this judges a correct solution through each of the eighteen and expects
/// full marks. It is the slowest test in the repository and it is the only
/// evidence that the toolchains exist.
///
/// **Every failure is collected rather than the first one panicking.** A broken
/// image usually breaks its whole half of the table, and being told about
/// `c89-gcc` alone would cost eight runs of a two-minute test to learn what one
/// run already knew.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn every_toolchain_in_the_catalogue_builds_and_runs() {
    let mut broken: Vec<String> = Vec::new();

    for language in catalogue(&Images::default()) {
        let source = match language.family {
            Family::C => CORRECT_C,
            Family::Cpp => CORRECT_CPP,
            Family::Python => CORRECT_PYTHON,
        };

        match judge(language.id, language.id, source).await {
            Evaluated::Judged(judged) => {
                if judged.judgement.verdict != "Accepted" {
                    broken.push(format!(
                        "{} ({}): {} — {}",
                        language.id,
                        language.image,
                        judged.judgement.verdict,
                        judged.details.compilation.log.trim(),
                    ));
                }
            }
            Evaluated::Failed(reason) => {
                broken.push(format!("{} ({}): {reason}", language.id, language.image));
            }
        }
    }

    assert!(
        broken.is_empty(),
        "{} of 18 toolchains did not judge a correct solution:
{}",
        broken.len(),
        broken.join(
            "
"
        ),
    );
}

/// The two ids every package on disk was written with still judge, and judge
/// as the toolchains they now name.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn the_ids_packages_were_written_with_still_judge() {
    for (alias, source) in [("cpp", CORRECT_CPP), ("python", CORRECT_PYTHON)] {
        let judged = verdict(judge(&format!("alias-{alias}"), alias, source).await);
        assert_eq!(judged.judgement.verdict, "Accepted", "{alias}");
        assert_eq!(judged.judgement.score, 100.0, "{alias}");
    }
}

/// A participant who picked the wrong language from the form is told so, and is
/// told it as a **verdict** — the submission was judged, badly, by them.
///
/// The alternative was an infrastructure failure, which would leave the
/// submission in a state that says the platform broke. It did not; the compiler
/// would have refused this thirty seconds later with a worse message.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_file_the_chosen_toolchain_does_not_accept_is_a_compilation_error() {
    let pipeline = pipeline().await;
    let (package, config, tests) = package("wrong-extension");

    let judged = verdict(
        pipeline
            .evaluate(&Job {
                config: &config,
                tests: &tests,
                language: "cpp17-gcc",
                // Python, submitted as C++. The Client offers a select and a
                // file field, and nothing stops the two disagreeing.
                file_name: "solution.py",
                source: CORRECT_PYTHON.as_bytes(),
                package,
                work: work("wrong-extension"),
                outputs: None,
            })
            .await,
    );

    assert_eq!(judged.judgement.verdict, "Compilation error");
    assert_eq!(judged.judgement.score, 0.0);

    let said = &judged.details.compilation.log;
    assert!(said.contains("solution.py"), "{said}");
    assert!(
        said.contains(".cpp"),
        "the accepted extensions are named: {said}"
    );
    assert!(
        said.contains("C++17 (GCC)"),
        "the language is named as a person reads it: {said}"
    );
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
    assert_eq!(document["type"], "standard-io@1");
    assert_eq!(document["compilation"]["status"], "OK");
    assert_eq!(document["tests"].as_array().unwrap().len(), 3);
    // **Tightened when the quantity changed.** Under the wall clock this had to
    // allow for the container's own start, some 374 ms on the machine it was
    // written on, so it could only say "not the whole limit" and mean it. The
    // number is processor time now, and an adding program spends milliseconds
    // of it.
    assert!(
        document["tests"][0]["timeMs"].as_u64().unwrap() < 200,
        "an adding program spends milliseconds of processor time: {document}",
    );
}

#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_correct_python_solution_is_accepted() {
    let judged = verdict(judge("python-correct", "python", CORRECT_PYTHON).await);

    assert_eq!(judged.judgement.verdict, "Accepted");
    assert_eq!(judged.judgement.score, 100.0);
}

/// The memory a solution used reaches the result document.
///
/// The last thing calibration was waiting for: `PACKAGE_FORMAT.md` lets
/// `memoryBytes` be absent because the Runner could not measure it honestly, and
/// on a cgroup v2 host with a writable cgroup mount it now can. Absent stays a
/// legitimate answer, so this skips rather than fails where there is nowhere to
/// measure from.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_judged_solution_reports_what_memory_it_used() {
    let judged = verdict(judge("cpp-memory", "cpp", CORRECT_CPP).await);
    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();

    // Bytes, like every memory figure in the product since 2026-08-09.
    // **No skip any more.** It used to return quietly where the host gave the
    // Runner nowhere to measure from; `preflight` refuses such a host outright
    // since 2026-09-02, so an absence here is the reporting having been dropped
    // rather than the machine, and skipping would be a green test over nothing.
    let memory = document["tests"][0]["memoryBytes"]
        .as_u64()
        .unwrap_or_else(|| panic!("preflight passed, so the cgroup is readable: {document}"));

    // A container floor of roughly 2 MiB, plus whatever the program did. Bounds
    // rather than a value, because the point is that it is a real measurement
    // and not a plausible-looking constant.
    assert!(
        (1024 * 1024..256 * 1024 * 1024).contains(&memory),
        "an adding program should use a few MiB, not bytes and not gigabytes: {memory} bytes",
    );
}

/// **The one test that could not pass before 2026-09-02, in either direction.**
///
/// A limit is processor time. This program spends two seconds and burns none of
/// it, so it is now well inside a one-second limit — and under the wall clock it
/// reported about 2.4 s, the sleep plus the container's own start, and came back
/// `Time limit exceeded`. The same source, the same limit, the opposite verdict.
///
/// **It also documents a real behaviour change rather than only proving one.**
/// Waiting is free up to the reaping deadline, which is four times the limit
/// and four seconds. Every judge that limits processor time works this way, and
/// Codeforces gives it a verdict of its own — *Idleness limit exceeded* —
/// precisely because it surprises people.
///
/// **How it waits is load-bearing.** `unistd.h` is a denied header, `<thread>`
/// is denied and `std::this_thread` is a denied pattern, so `sleep`, `usleep`
/// and `sleep_for` would all come back `PolicyViolation` and this test would
/// pass on entirely the wrong thing. `nanosleep` is in `<ctime>`, which is not
/// denied, and appears in no list at all. If the dictionary ever gains it the
/// verdict becomes `PolicyViolation` and the assertion below fails loudly,
/// which is the outcome to want.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_program_that_waits_without_computing_is_inside_its_limit() {
    let document = waited_for("waiting-inside", 1000, 2).await;

    assert_eq!(
        document["score"], document["maxScore"],
        "two seconds of waiting is no processor time, so this is a correct \
         solution under a limit stated in processor time: {document}",
    );
    let spent = document["tests"][0]["timeMs"].as_u64().expect("a time");
    assert!(
        spent < 200,
        "the wall clock would have been about 2400 ms here — the sleep plus the \
         container's start. {spent} ms says this is still measuring the wrong \
         quantity: {document}",
    );
}

/// The other side of the same rule, so neither half can drift alone.
///
/// Eight seconds of waiting against a 300 ms limit passes the reaping deadline
/// of 5.2 s, and a reaped program is `Time limit exceeded` — **the same verdict and
/// the same `reason` as one that computed too long**, deliberately. The
/// vocabulary is shared with the Client, the documentation and every package on
/// disk, and a new word for this would be a cross-repository change to say
/// something a participant does not need told apart.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_program_that_waits_past_the_backstop_is_still_a_time_limit() {
    let document = waited_for("waiting-past", 300, 8).await;

    assert_ne!(document["score"], document["maxScore"], "{document}");
    assert_eq!(
        document["tests"][0]["reason"], "timeLimit",
        "reaped at the backstop, and it is a time limit like any other: {document}",
    );
    assert!(
        document["tests"][0]["note"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Time limit exceeded"),
        "the note leads with the same words, and says which kind after: {document}",
    );
}

/// One package, one program, two limits — so the pair above differ in exactly
/// the number under test.
async fn waited_for(name: &str, limit_ms: u64, seconds: u64) -> serde_json::Value {
    let config_yml = format!(
        "type: \"standard-io@1\"\nlimits:\n  timeMs: {limit_ms}\n  memoryBytes: 268435456\n\
         groups:\n  - group: 1\n    points: 100\n"
    );

    let pipeline = pipeline().await;
    let (here, on_host) = fixture(name);
    std::fs::create_dir_all(here.join("tests")).unwrap();
    std::fs::write(here.join("config.yml"), &config_yml).unwrap();
    std::fs::write(here.join("tests/1a.in"), "1 2\n").unwrap();
    std::fs::write(here.join("tests/1a.out"), "3\n").unwrap();

    let config = Config::parse(&config_yml).unwrap();
    let tests = TestSet::read(&here, &config).unwrap();

    let waiting = format!(
        "#include <cstdio>\n#include <ctime>\nint main(){{long long a,b;\
         if(scanf(\"%lld %lld\",&a,&b)!=2)return 1;\
         timespec t{{{seconds},0}};nanosleep(&t,nullptr);\
         printf(\"%lld\\n\",a+b);}}\n"
    );

    let judged = verdict(
        pipeline
            .evaluate(&Job {
                config: &config,
                tests: &tests,
                language: "cpp",
                file_name: "main.cpp",
                source: waiting.as_bytes(),
                package: Places { here, on_host },
                work: work(name),
                outputs: None,
            })
            .await,
    );

    serde_json::from_slice(&judged.details.to_bytes()).unwrap()
}

/// **The stdout file follows where it is pointed, and a wrong path is a wrong
/// answer rather than an error.**
///
/// `Job::outputs` exists so an operator can put the one file in the loop that
/// nothing needs to keep — a submission's own output — on a host tmpfs, and
/// keep it off a disk entirely. This drives that path with a directory of its
/// own and judges a correct solution through it.
///
/// **It bites for the reason the field is dangerous.** The daemon resolves the
/// bind mount, so a path it cannot open produces an *empty directory* rather
/// than an error; the shim then writes into nothing, the Runner reads nothing,
/// and every test is compared against an empty answer. That is a wrong verdict
/// with no error anywhere — so the assertion here is the score, not the plumbing.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn the_stdout_file_follows_where_the_outputs_are_pointed() {
    let name = "cpp-outputs-elsewhere";
    let correct = r#"
#include <iostream>
int main() { long long a, b; std::cin >> a >> b; std::cout << a + b << "\n"; }
"#;

    // A root of its own, and deliberately not under `work`: on a real host this
    // is where a tmpfs would be mounted.
    // **Through `fixture`, and that is the lesson this test paid for.** The
    // first version pointed at this process's own `temp_dir()`, which the
    // daemon cannot open -- so it made an empty directory, the shim wrote into
    // nothing, and the verdict came back `Wrong answer` with no error anywhere.
    // Exactly the failure the doc comment above predicts.
    let (out_here, out_on_host) = fixture(&format!("{name}-outputs"));
    let elsewhere = Places {
        here: out_here,
        on_host: out_on_host,
    };

    let pipeline = pipeline().await;
    let (here, on_host) = fixture(name);
    std::fs::create_dir_all(here.join("tests")).unwrap();
    let config_yml = "type: \"standard-io@1\"
limits:
  timeMs: 2000
  memoryBytes: 268435456
groups:
  - group: 1
    points: 100
"
    .to_owned();
    std::fs::write(here.join("config.yml"), &config_yml).unwrap();
    std::fs::write(
        here.join("tests/1a.in"),
        "2 3
",
    )
    .unwrap();
    std::fs::write(
        here.join("tests/1a.out"),
        "5
",
    )
    .unwrap();

    let config = Config::parse(&config_yml).unwrap();
    let tests = TestSet::read(&here, &config).unwrap();

    let judged = verdict(
        pipeline
            .evaluate(&Job {
                config: &config,
                tests: &tests,
                language: "cpp",
                file_name: "main.cpp",
                source: correct.as_bytes(),
                package: Places { here, on_host },
                work: work(name),
                outputs: Some(elsewhere.clone()),
            })
            .await,
    );

    assert_eq!(
        judged.judgement.verdict, "Accepted",
        "the answer came back through the directory it was pointed at",
    );
    assert_eq!(judged.judgement.score, judged.judgement.max_score);

    let _ = std::fs::remove_dir_all(&elsewhere.here);
}

/// **A submission that floods is stopped by the kernel, and it is still an
/// output limit.**
///
/// Since 2026-09-05 a judged run's stdout goes to a file the shim opens, so the
/// cap is `RLIMIT_FSIZE` on the child rather than a count of the stream — the
/// kernel stops the program on the write that would cross it. That means the
/// child dies of `SIGXFSZ`, and **every other path in the pipeline reads a
/// fatal signal as a runtime error**. This is the test that says which it is,
/// and it went red the first time it was run.
///
/// The cap a judged run carries is 64 MiB, so this writes until it is stopped
/// rather than counting to a number of its own: a test that knew the figure
/// would pass on a cap that had quietly moved.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_flooding_submission_is_an_output_limit_and_not_a_crash() {
    let flooding = r#"
#include <cstdio>
int main() {
    long long a, b;
    if (scanf("%lld %lld", &a, &b) != 2) return 1;
    for (;;) puts("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
}
"#;
    let judged = verdict(judge("cpp-flooding", "cpp", flooding).await);
    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();

    assert_eq!(
        document["tests"][0]["reason"], "outputLimit",
        "a flood is an output limit and not a crash: {document}"
    );
    assert!(
        document["tests"][0]["note"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Output limit exceeded"),
        "the note says so in the words the Client and every package share: {document}"
    );
    assert_ne!(judged.judgement.score, judged.judgement.max_score);
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

/// **The Runner validates what was uploaded, because nothing above it can.**
///
/// The Server stores a submission without opening it, and what makes one well
/// formed belongs to the problem type — the same reason `output-only@1` bounds
/// its own archive rather than asking for that upstream. A file far too large
/// to be a program breaks a rule of the activity; it is not a compilation
/// error, because nothing was offered to a compiler.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_submission_too_large_to_be_a_program_is_a_policy_violation() {
    // Valid C++ and over the wall, so what refuses it is its size and not the
    // compiler — and padding rather than code, so no rule but the size can
    // match. Past **eight** megabytes, not one: the wall is the Server's own
    // submission ceiling, because a Runner refusing below what a manager may
    // set would be overriding the manager.
    let huge = format!(
        "#include <iostream>\n{}int main() {{ long long a, b; std::cin >> a >> b; std::cout << a + b; }}\n",
        "// padding padding padding padding padding padding padding\n".repeat(150_000),
    );
    assert!(
        huge.len() > 8 * 1024 * 1024,
        "the case needs to be over the wall, and is {} bytes",
        huge.len(),
    );

    let judged = verdict(judge("cpp-too-large", "cpp", &huge).await);

    assert_eq!(judged.judgement.verdict, "PolicyViolation");
    assert_eq!(judged.judgement.score, 0.0);

    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();
    // Nothing failed to compile, because nothing reached a compiler.
    assert_eq!(document["compilation"]["status"], "WARNING");

    let said = document["compilation"]["log"].as_str().unwrap();
    assert!(
        said.contains("KiB"),
        "a participant is not told what the limit was: {said}",
    );
}

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
        said.contains("Running another program"),
        "the participant is told the rule's name, not its letter: {said}",
    );
    assert!(said.contains("(line "), "and where it was: {said}");
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
            file_name: "main.cpp",
            source: CORRECT_CPP.as_bytes(),
            package: Places {
                here: unpacked,
                on_host: on_the_host.join("package"),
            },
            work: work("archive"),
            outputs: None,
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

/// Measuring the committed package's own model solutions.
///
/// The end of the calibration chain: two model solutions, two languages, and a
/// row per group per language — which is what makes "one `.cpp` sets the limits
/// for everybody" a *choice* rather than the only thing possible.
///
/// Uses `fixtures/sum`, the package that ships, so this fails if the format and
/// the code ever part company.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_trial_measures_every_model_solution_per_group() {
    let pipeline = pipeline().await;

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sum");
    let on_host = match std::env::var("AJ_HOST_WORKDIR") {
        Ok(host) => {
            let separator = if host.contains('\\') { "\\" } else { "/" };
            PathBuf::from(host).join(format!("fixtures{separator}sum"))
        }
        Err(_) => root.clone(),
    };
    let package = Places {
        here: root.clone(),
        on_host,
    };

    let declared = std::fs::read_to_string(root.join("config.yml")).unwrap();
    let config = Config::parse(&declared).unwrap();
    let tests = TestSet::read(&root, &config).unwrap();

    let measured = aj_standard_io::measure(&pipeline, &config, &tests, &package, &work("trial"))
        .await
        .expect("the package's own model solutions should measure");

    assert!(!measured.measured.is_empty(), "nothing was measured");

    // Two models, so every row names its language: a row without one would be
    // the package's own limit, which is not what two references produce.
    assert!(
        measured.measured.iter().all(|m| m.language.is_some()),
        "with several models every row names a language: {:?}",
        measured.measured,
    );

    let languages: std::collections::BTreeSet<_> = measured
        .measured
        .iter()
        .filter_map(|m| m.language.clone())
        .collect();
    assert_eq!(
        languages,
        ["cpp".to_owned(), "python".to_owned()]
            .into_iter()
            .collect(),
        "both declared model solutions should have been run",
    );

    // A measurement of nothing is not a measurement. Every group that has tests
    // reports a time, and time is never zero for a program that actually ran.
    //
    // **This holds because the conversion rounds up.** Processor time is finer
    // than a millisecond and a program that adds two numbers can spend less than
    // one; truncating would put a zero here, and a zero is what the calibration
    // rule then multiplies by three to produce `limits.timeMs: 0` — which the
    // format refuses. So this assertion is also the guard on that.
    assert!(
        measured.measured.iter().all(|m| m.time_ms > 0),
        "a measured group with no time did not run: {:?}",
        measured.measured,
    );
}

/// The reason a test failed reaches the document as a **value**.
///
/// The point of the field: a Client can show a clock beside a time limit and a
/// different thing beside a crash without matching on prose. Asserted on two
/// different failures, because a field that is always the same value is not
/// carrying information.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn a_failure_says_why_as_a_value_and_not_only_as_prose() {
    let looping = r#"
#include <iostream>
int main() { long long a, b; std::cin >> a >> b; while (true) { } }
"#;
    let timed = verdict(judge("cpp-reason-loop", "cpp", looping).await);
    let timed_doc: serde_json::Value = serde_json::from_slice(&timed.details.to_bytes()).unwrap();
    assert_eq!(timed_doc["tests"][0]["reason"], "timeLimit");

    let crashing = r#"
#include <iostream>
int main() { long long a, b; std::cin >> a >> b; volatile int *p = nullptr; *p = 1; }
"#;
    let crashed = verdict(judge("cpp-reason-crash", "cpp", crashing).await);
    let crashed_doc: serde_json::Value =
        serde_json::from_slice(&crashed.details.to_bytes()).unwrap();
    assert_eq!(crashed_doc["tests"][0]["reason"], "runtimeError");

    // A test that passed carries no reason: `status` already says so, and a
    // reason beside a pass would be a reason for nothing.
    let fine = verdict(judge("cpp-reason-ok", "cpp", CORRECT_CPP).await);
    let fine_doc: serde_json::Value = serde_json::from_slice(&fine.details.to_bytes()).unwrap();
    assert!(fine_doc["tests"][0].get("reason").is_none());
}

/// The document states the limits the submission was actually held to.
///
/// **It used to state the package's global pair**, whatever the run had been
/// judged under. A package that gives Python longer — which is the whole reason
/// `overrideLimits` exists — produced a result document telling the participant
/// a `timeMs` no test of theirs was ever measured against. The document
/// contradicted the run it describes, on the same screen.
///
/// Judged in Python, because that is the language the override below names.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn the_document_reports_the_limit_the_run_was_held_to() {
    const OVERRIDDEN: &str = r#"
type: "standard-io@1"
limits:
  timeMs: 2000
  memoryBytes: 268435456
overrideLimits:
  python:
    timeMs: 9000
groups:
  - group: 0
    points: 0
    examples: true
  - group: 1
    points: 40
  - group: 2
    points: 60
"#;

    let pipeline = pipeline().await;
    let (here, on_host) = fixture("limits-reported");
    std::fs::create_dir_all(here.join("tests")).unwrap();
    std::fs::write(here.join("config.yml"), OVERRIDDEN).unwrap();
    for (test, input, expected) in [
        ("0a", "1 2\n", "3\n"),
        ("1a", "10 20\n", "30\n"),
        ("2a", "1000000 2000000\n", "3000000\n"),
    ] {
        std::fs::write(here.join(format!("tests/{test}.in")), input).unwrap();
        std::fs::write(here.join(format!("tests/{test}.out")), expected).unwrap();
    }

    let config = Config::parse(OVERRIDDEN).unwrap();
    let tests = TestSet::read(&here, &config).unwrap();

    let judged = verdict(
        pipeline
            .evaluate(&Job {
                config: &config,
                tests: &tests,
                language: "python",
                file_name: "main.py",
                source: b"a, b = map(int, input().split())\nprint(a + b)\n",
                package: Places { here, on_host },
                work: work("limits-reported"),
                outputs: None,
            })
            .await,
    );

    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();
    assert_eq!(
        document["limits"]["timeMs"], 9000,
        "the document should carry Python's own limit, not the package's global one: {}",
        document["limits"],
    );
}

/// Overrunning the limit is a time limit, even when nothing had to kill it.
///
/// **Until 2026-08-22 it was not.** A test container is given the limit *plus a
/// second of grace*, because a program wedged in an uninterruptible syscall has
/// to be reaped from outside — and nothing then compared the measurement against
/// the limit itself, although the comment beside the deadline said the verdict
/// came from exactly that. So a solution that overran by anything less than the
/// grace stopped on its own, was never reaped, and was marked correct: at a
/// one-second limit, a program taking 1.9 s was `Accepted`.
///
/// This one is given half a second of processor time and **spins** for about
/// eight tenths — spinning rather than waiting, deliberately, because since
/// 2026-09-02 the limit is processor time and waiting would not reach it. It is
/// comfortably over the limit and comfortably inside the reaping deadline,
/// which is 2.5 s here, so it stops on its own and the comparison is what
/// catches it. That window is the one that used to be free.
#[tokio::test]
#[ignore = "needs a container runtime and the language images"]
async fn overrunning_the_limit_inside_the_backstop_is_still_a_time_limit() {
    const TIGHT: &str = r#"
type: "standard-io@1"
limits:
  timeMs: 500
  memoryBytes: 268435456
groups:
  - group: 0
    points: 0
    examples: true
  - group: 1
    points: 40
  - group: 2
    points: 60
"#;

    let pipeline = pipeline().await;
    let (here, on_host) = fixture("overrun");
    std::fs::create_dir_all(here.join("tests")).unwrap();
    std::fs::write(here.join("config.yml"), TIGHT).unwrap();
    for (test, input, expected) in [
        ("0a", "1 2\n", "3\n"),
        ("1a", "10 20\n", "30\n"),
        ("2a", "1000000 2000000\n", "3000000\n"),
    ] {
        std::fs::write(here.join(format!("tests/{test}.in")), input).unwrap();
        std::fs::write(here.join(format!("tests/{test}.out")), expected).unwrap();
    }

    let config = Config::parse(TIGHT).unwrap();
    let tests = TestSet::read(&here, &config).unwrap();

    let slow = "#include <iostream>\n#include <chrono>\nint main(){long long a,b;std::cin>>a>>b;auto t=std::chrono::steady_clock::now();while(std::chrono::steady_clock::now()-t<std::chrono::milliseconds(800)){}std::cout<<a+b<<std::endl;}\n";

    let judged = verdict(
        pipeline
            .evaluate(&Job {
                config: &config,
                tests: &tests,
                language: "cpp",
                file_name: "main.cpp",
                source: slow.as_bytes(),
                package: Places { here, on_host },
                work: work("overrun"),
                outputs: None,
            })
            .await,
    );

    let document: serde_json::Value = serde_json::from_slice(&judged.details.to_bytes()).unwrap();
    assert_ne!(
        judged.judgement.verdict, "Accepted",
        "a solution that took longer than its limit was accepted: {document}",
    );
    assert!(
        document["tests"][0]["note"]
            .as_str()
            .unwrap_or_default()
            .contains("Time limit exceeded"),
        "and it should say so by name: {}",
        document["tests"][0],
    );
}
