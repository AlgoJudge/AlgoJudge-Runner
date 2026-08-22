//! The document the Runner attaches to a result, under the name `details`.
//!
//! **This schema is shared between the Runner and the Client**: the Runner fills
//! it in, the Server stores it without reading it, and the Client's
//! `standard-io` renderer draws it. Both sides depend on it, which is why it
//! lives in `PACKAGE_FORMAT.md` rather than in either of them.
//!
//! It travels as an **attachment**, not inline. A problem may have two thousand
//! tests per attempt, and a contest's submissions came to hundreds of megabytes
//! in a database column before it moved.
//!
//! One thing here is not a mistake and looks like one: the package's config
//! states memory in **kibibytes** and this document states it in **mebibytes**.
//! The first is a format meant to import from `sinolpack` without arithmetic;
//! the second is a number a person reads beside a verdict.

use serde::Serialize;

use crate::score::{Judgement, Reason, Status};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Details {
    /// The problem type, as **one string** — `standard-io@1`.
    ///
    /// **It was `kind` plus `version` until 2026-08-22.** The type envelope was
    /// decided as one string on 2026-08-02 and the product had written it four
    /// different ways since: `Activity.Type` as `name@version`, `format` plus
    /// `version` in `config.yml`, `kind` plus `version` here, and a bare
    /// `version` in `content.md`. A convention with four spellings is not a
    /// convention, and no reader checked any of them — this one's two fields
    /// were parsed by the Client's renderer and both ignored.
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub limits: Limits,
    pub score: f64,
    pub max_score: f64,
    pub compilation: Compilation,
    pub groups: Vec<GroupReport>,
    pub tests: Vec<TestReport>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub time_ms: u64,
    pub memory_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Compilation {
    pub status: Status,
    /// The compiler's own words, as text. It is the participant's most useful
    /// single artefact when their submission does not build.
    pub log: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupReport {
    pub group: u32,
    pub points: f64,
    pub max_points: f64,
    pub status: Status,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestReport {
    /// `1a`, `2c` — the test's name, called `no` on the wire.
    pub no: String,
    pub group: u32,
    pub status: Status,
    pub time_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    pub score: f64,
    pub max_score: f64,
    /// **Reaches the participant and originates beside untrusted code** — a
    /// checker may echo a program's output into it. Carried as text and
    /// rendered as text; nothing here escapes it, and nothing downstream should
    /// treat it as markup.
    pub note: String,
    /// Why, as a value. Absent on a test that passed — `status` already says
    /// that, and a reason beside a pass would be a reason for nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
}

impl Details {
    pub fn of(judged: &Judgement, limits: Limits, compilation: Compilation) -> Self {
        Self {
            kind: "standard-io@1",
            limits,
            score: judged.score,
            max_score: judged.max_score,
            compilation,
            groups: judged
                .groups
                .iter()
                .map(|g| GroupReport {
                    group: g.group,
                    points: g.points,
                    max_points: g.max_points,
                    status: g.status,
                })
                .collect(),
            tests: judged
                .tests
                .iter()
                .map(|t| TestReport {
                    no: t.outcome.name.clone(),
                    group: t.outcome.group,
                    status: t.outcome.status,
                    time_ms: t.outcome.time_ms,
                    memory_bytes: t.outcome.memory_bytes,
                    score: t.score,
                    max_score: t.max_score,
                    note: t.outcome.note.clone(),
                    reason: t.outcome.reason,
                })
                .collect(),
        }
    }

    /// The bytes that go through the file API under the name `details`.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a document of numbers and strings always serialises")
    }
}

/// Compilation that produced nothing to say.
pub fn compiled() -> Compilation {
    Compilation {
        status: Status::Ok,
        log: String::new(),
    }
}

pub fn failed_to_compile(log: impl Into<String>) -> Compilation {
    Compilation {
        status: Status::Error,
        log: log.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::score::{GroupScore, ScoredTest, TestOutcome};

    fn judged() -> Judgement {
        Judgement {
            score: 70.0,
            max_score: 100.0,
            verdict: "Wrong answer".into(),
            groups: vec![GroupScore {
                group: 1,
                points: 70.0,
                max_points: 100.0,
                status: Status::Ok,
            }],
            tests: vec![ScoredTest {
                outcome: TestOutcome {
                    name: "1a".into(),
                    group: 1,
                    status: Status::Ok,
                    percentage: 100,
                    time_ms: 20,
                    memory_bytes: Some(12 * 1024 * 1024),
                    note: String::new(),
                    reason: None,
                },
                score: 70.0,
                max_score: 100.0,
            }],
        }
    }

    /// The field names are a contract with the Client's renderer, so they are
    /// asserted rather than assumed.
    #[test]
    fn the_document_has_the_shape_the_format_specifies() {
        let details = Details::of(
            &judged(),
            Limits {
                time_ms: 1000,
                memory_bytes: 256 * 1024 * 1024,
            },
            compiled(),
        );

        let json: serde_json::Value = serde_json::from_slice(&details.to_bytes()).unwrap();

        assert_eq!(json["type"], "standard-io@1");
        assert!(json["kind"].is_null(), "one string, not two fields");
        assert!(json["version"].is_null());
        assert_eq!(json["limits"]["timeMs"], 1000);
        assert_eq!(json["limits"]["memoryBytes"], 268435456);
        assert_eq!(json["score"], 70.0);
        assert_eq!(json["maxScore"], 100.0);
        assert_eq!(json["compilation"]["status"], "OK");
        assert_eq!(json["groups"][0]["maxPoints"], 100.0);
        assert_eq!(json["tests"][0]["no"], "1a");
        assert_eq!(
            json["tests"][0]["memoryBytes"],
            12 * 1024 * 1024,
            "bytes here, as everywhere: the measurement crosses no conversion"
        );
        assert_eq!(json["tests"][0]["status"], "OK");
        assert_eq!(json["tests"][0]["note"], "");
    }

    #[test]
    fn a_status_serialises_as_the_documents_vocabulary() {
        for (status, expected) in [
            (Status::Ok, "OK"),
            (Status::Error, "ERROR"),
            (Status::Warning, "WARNING"),
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), expected);
        }
    }

    /// An unmeasured value is absent rather than zero. Zero milliseconds of
    /// memory is a claim; absence is the truth.
    #[test]
    fn memory_that_was_not_measured_is_absent() {
        let mut judgement = judged();
        judgement.tests[0].outcome.memory_bytes = None;

        let details = Details::of(
            &judgement,
            Limits {
                time_ms: 1,
                memory_bytes: 1,
            },
            compiled(),
        );
        let json: serde_json::Value = serde_json::from_slice(&details.to_bytes()).unwrap();

        assert!(json["tests"][0].get("memoryBytes").is_none());
    }

    #[test]
    fn a_compilation_failure_carries_the_compilers_own_words() {
        let details = Details::of(
            &judged(),
            Limits {
                time_ms: 1,
                memory_bytes: 1,
            },
            failed_to_compile("main.cpp:3:1: error: expected ';'"),
        );
        let json: serde_json::Value = serde_json::from_slice(&details.to_bytes()).unwrap();

        assert_eq!(json["compilation"]["status"], "ERROR");
        assert!(json["compilation"]["log"]
            .as_str()
            .unwrap()
            .contains("expected ';'"));
    }
}
