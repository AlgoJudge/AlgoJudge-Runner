//! Turning tests into a mark.
//!
//! Two rules from `PACKAGE_FORMAT.md`, and the second is the one that makes a
//! group mean something:
//!
//! - a test is worth `points × percentage / tests in the group`;
//! - **a group's points are awarded only if every test in it passes**, which is
//!   what makes a group a group rather than a label.
//!
//! Read together they say: a test that a checker accepted with partial credit
//! has *passed*, and contributes its share; a test that failed outright takes
//! its whole group to zero. That is the only reading under which partial credit
//! and the group rule can both be true, and it is pinned by the tests below.

use aj_package::{Config, TestSet};
use serde::Serialize;

/// `OK`, `ERROR`, `WARNING` — the document's whole vocabulary.
///
/// Anything finer belongs in `note`, because the Client must not have to
/// understand a vocabulary that grows with every problem type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Status {
    Ok,
    Error,
    Warning,
}

impl Status {
    pub fn passed(self) -> bool {
        matches!(self, Status::Ok | Status::Warning)
    }
}

/// What running one test produced, before it is worth anything.
#[derive(Debug, Clone)]
pub struct TestOutcome {
    pub name: String,
    pub group: u32,
    pub status: Status,
    /// 0–100. A checker's, or 100/0 where there is none.
    pub percentage: u32,
    pub time_ms: u64,
    pub memory_kib: Option<u64>,
    /// Reaches the participant, and originates beside untrusted code.
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct Judgement {
    pub score: f64,
    pub max_score: f64,
    pub verdict: String,
    pub groups: Vec<GroupScore>,
    pub tests: Vec<ScoredTest>,
}

#[derive(Debug, Clone)]
pub struct GroupScore {
    pub group: u32,
    pub points: f64,
    pub max_points: f64,
    pub status: Status,
}

#[derive(Debug, Clone)]
pub struct ScoredTest {
    pub outcome: TestOutcome,
    pub score: f64,
    pub max_score: f64,
}

pub fn judge(config: &Config, tests: &TestSet, outcomes: &[TestOutcome]) -> Judgement {
    let mut groups = Vec::new();
    let mut scored = Vec::new();

    for group in &config.groups {
        let count = tests.in_group(group.group).max(1) as f64;
        let per_test = f64::from(group.points) / count;

        let mine: Vec<&TestOutcome> = outcomes.iter().filter(|o| o.group == group.group).collect();
        let all_passed = !mine.is_empty() && mine.iter().all(|o| o.status.passed());

        for outcome in &mine {
            let earned = if all_passed {
                per_test * f64::from(outcome.percentage) / 100.0
            } else {
                // The group rule, applied where a person reads it: a test in a
                // failed group shows what it was worth and that it earned
                // nothing, rather than a number that does not add up to the
                // group's.
                0.0
            };
            scored.push(ScoredTest {
                outcome: (*outcome).clone(),
                score: round(earned),
                max_score: round(per_test),
            });
        }

        let points = if all_passed {
            mine.iter()
                .map(|o| per_test * f64::from(o.percentage) / 100.0)
                .sum()
        } else {
            0.0
        };

        groups.push(GroupScore {
            group: group.group,
            points: round(points),
            max_points: f64::from(group.points),
            status: if all_passed {
                Status::Ok
            } else {
                Status::Error
            },
        });
    }

    let score: f64 = groups.iter().map(|g| g.points).sum();
    let max_score: f64 = groups.iter().map(|g| g.max_points).sum();

    Judgement {
        verdict: verdict(&scored, score, max_score),
        score: round(score),
        max_score,
        groups,
        tests: scored,
    }
}

/// One word for the submission as a whole.
///
/// Opaque to the Server, which stores it and never branches on it — which is
/// what lets a problem type introduce a verdict without a Server release.
fn verdict(tests: &[ScoredTest], score: f64, max_score: f64) -> String {
    if tests.iter().all(|t| t.outcome.status.passed()) && score >= max_score {
        return "Accepted".to_owned();
    }
    // The first thing that went wrong, in test order, is the most useful single
    // word: it is where a participant starts reading.
    tests
        .iter()
        .find(|t| !t.outcome.status.passed())
        .map(|t| {
            if t.outcome.note.is_empty() {
                "Wrong answer".to_owned()
            } else {
                t.outcome.note.clone()
            }
        })
        .unwrap_or_else(|| "Partial".to_owned())
}

/// Two decimals. A mark shown to a participant with fifteen of them reads as a
/// machine's arithmetic rather than as their score.
fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
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
    points: 30
  - group: 2
    points: 70
"#;

    fn package() -> (Config, TestSet) {
        let config = Config::parse(CONFIG).unwrap();

        let mut root = std::env::temp_dir();
        root.push(format!("aj-score-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("tests")).unwrap();
        for name in ["0a", "1a", "1b", "2a"] {
            std::fs::write(root.join(format!("tests/{name}.in")), "x").unwrap();
            std::fs::write(root.join(format!("tests/{name}.out")), "x").unwrap();
        }

        let tests = TestSet::read(&root, &config).unwrap();
        (config, tests)
    }

    fn outcome(name: &str, group: u32, status: Status, percentage: u32) -> TestOutcome {
        TestOutcome {
            name: name.into(),
            group,
            status,
            percentage,
            time_ms: 10,
            memory_kib: None,
            note: String::new(),
        }
    }

    fn all_passing() -> Vec<TestOutcome> {
        vec![
            outcome("0a", 0, Status::Ok, 100),
            outcome("1a", 1, Status::Ok, 100),
            outcome("1b", 1, Status::Ok, 100),
            outcome("2a", 2, Status::Ok, 100),
        ]
    }

    #[test]
    fn everything_passing_is_full_marks() {
        let (config, tests) = package();
        let judged = judge(&config, &tests, &all_passing());

        assert_eq!(judged.score, 100.0);
        assert_eq!(judged.max_score, 100.0);
        assert_eq!(judged.verdict, "Accepted");
    }

    /// Group 0 is the examples and is worth zero. It is judged like any other,
    /// so failing an example fails visibly rather than silently.
    #[test]
    fn failing_an_example_costs_nothing_and_is_still_reported() {
        let (config, tests) = package();
        let mut outcomes = all_passing();
        outcomes[0].status = Status::Error;

        let judged = judge(&config, &tests, &outcomes);

        assert_eq!(judged.score, 100.0, "group 0 carries no points");
        assert_ne!(judged.verdict, "Accepted", "but it is not a clean pass");
        assert_eq!(judged.groups[0].status, Status::Error);
    }

    /// The rule that makes a group a group.
    #[test]
    fn one_failed_test_takes_its_whole_group_to_zero() {
        let (config, tests) = package();
        let mut outcomes = all_passing();
        outcomes[2].status = Status::Error; // 1b, of two in group 1

        let judged = judge(&config, &tests, &outcomes);

        assert_eq!(judged.groups[1].points, 0.0, "group 1 is lost entirely");
        assert_eq!(judged.groups[2].points, 70.0, "group 2 is untouched");
        assert_eq!(judged.score, 70.0);

        // And the test that did pass shows zero rather than a number that does
        // not add up to its group's.
        let passed = judged
            .tests
            .iter()
            .find(|t| t.outcome.name == "1a")
            .unwrap();
        assert_eq!(passed.score, 0.0);
        assert_eq!(passed.max_score, 15.0);
    }

    /// A checker's partial credit is a pass, and contributes its share.
    #[test]
    fn partial_credit_is_a_pass_and_scores_proportionally() {
        let (config, tests) = package();
        let mut outcomes = all_passing();
        outcomes[1].percentage = 50; // 1a, worth 15 at full marks

        let judged = judge(&config, &tests, &outcomes);

        assert_eq!(judged.groups[1].points, 22.5, "15 × 50% plus 15");
        assert_eq!(judged.score, 92.5);
        assert_ne!(judged.verdict, "Accepted");
    }

    #[test]
    fn a_group_is_divided_evenly_among_its_tests() {
        let (config, tests) = package();
        let judged = judge(&config, &tests, &all_passing());

        let group1: Vec<f64> = judged
            .tests
            .iter()
            .filter(|t| t.outcome.group == 1)
            .map(|t| t.max_score)
            .collect();
        assert_eq!(group1, vec![15.0, 15.0]);
    }

    #[test]
    fn the_verdict_names_the_first_thing_that_went_wrong() {
        let (config, tests) = package();
        let mut outcomes = all_passing();
        outcomes[3].status = Status::Error;
        outcomes[3].note = "Przekroczenie limitu czasu".into();

        let judged = judge(&config, &tests, &outcomes);
        assert_eq!(judged.verdict, "Przekroczenie limitu czasu");
    }
}
