//! Measuring a package's own model solutions, so limits can be derived from
//! them.
//!
//! **The judging pipeline does the work.** A model solution is compiled and run
//! against every test exactly as a submission is, because a measurement taken
//! any other way would be a measurement of a different thing — a limit derived
//! from a faster path than the one a participant meets is a limit nobody can
//! hit.
//!
//! What is added here is the fold: per test becomes **per group**, by taking
//! the maximum. A group's limit has to accommodate its slowest test, and a mean
//! would let one slow test sit above the limit derived from it.

use std::collections::BTreeMap;

use aj_package::{Config, Measurement, TestSet};
use aj_sandbox::Sandbox;

use crate::pipeline::{Evaluated, Job, Pipeline, Places};

/// What one trial produced: the rows that go into `calibration.measured`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Measured {
    pub measured: Vec<Measurement>,
}

/// Runs every model solution the package declares and reports per-group maxima.
///
/// **One row per group per language when there is more than one model**, and
/// rows without a language when there is exactly one — which is what
/// `PACKAGE_FORMAT.md` asks for, and what makes a single `.cpp` reference set
/// the limits every language then meets.
pub async fn measure<S: Sandbox>(
    pipeline: &Pipeline<S>,
    config: &Config,
    tests: &TestSet,
    package: &Places,
    work: &Places,
) -> Result<Measured, String> {
    let models = config.models();
    if models.is_empty() {
        return Err(
            "this package declares no model solution, so there is nothing to measure".into(),
        );
    }

    // Named only when there is more than one. With a single reference the rows
    // are the package's own, and adding its language would quietly turn them
    // into an override for that language alone.
    let name_languages = models.len() > 1;

    let mut rows = Vec::new();
    for (index, model) in models.iter().enumerate() {
        let source = package.here.join(&model.source);
        let bytes = std::fs::read(&source)
            .map_err(|e| format!("the model solution {} could not be read: {e}", model.source))?;

        // Its own scratch, because the pipeline empties what it is given and
        // two models sharing a directory would each delete the other's build.
        let mine = work.join(&format!("model-{index}"));

        let evaluated = pipeline
            .evaluate(&Job {
                config,
                tests,
                language: &model.language,
                source: &bytes,
                package: package.clone(),
                work: mine,
            })
            .await;

        let verdict = match evaluated {
            Evaluated::Judged(verdict) => verdict,
            Evaluated::Failed(reason) => {
                return Err(format!(
                    "the model solution {} could not be run: {reason}",
                    model.source
                ));
            }
        };

        // **A model solution that does not pass is not a measurement.** Timing a
        // wrong answer would derive a limit from a program that never did the
        // work, and the number would look entirely reasonable.
        if verdict.judgement.score < verdict.judgement.max_score {
            return Err(format!(
                "the model solution {} scored {} of {}, so it is not a reference to measure",
                model.source, verdict.judgement.score, verdict.judgement.max_score
            ));
        }

        // Ordered, so two runs of the same package produce the same document.
        let mut peaks: BTreeMap<u32, (u64, Option<u64>)> = BTreeMap::new();
        for test in &verdict.judgement.tests {
            let entry = peaks.entry(test.outcome.group).or_insert((0, None));
            entry.0 = entry.0.max(test.outcome.time_ms);
            // Absent stays absent: a group where one test could not be measured
            // has no honest peak, and the larger of "something" and "nothing" is
            // nothing.
            entry.1 = match (entry.1, test.outcome.memory_bytes) {
                (Some(a), Some(b)) => Some(a.max(b)),
                _ => None,
            };
        }

        for (group, (time_ms, memory_bytes)) in peaks {
            rows.push(Measurement {
                group,
                language: name_languages.then(|| model.language.clone()),
                time_ms,
                memory_bytes,
            });
        }
    }

    Ok(Measured { measured: rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule that decides whether a row is the package's or one language's.
    #[test]
    fn one_model_names_no_language_and_several_name_theirs() {
        let one = r#"
format: standard-io
version: 1
limits: { timeMs: 1000, memoryBytes: 268435456 }
groups: [{ group: 1, points: 100 }]
modelSolution: { source: solutions/model.cpp, language: cpp }
"#;
        assert_eq!(Config::parse(one).unwrap().models().len(), 1);

        let several = r#"
format: standard-io
version: 1
limits: { timeMs: 1000, memoryBytes: 268435456 }
groups: [{ group: 1, points: 100 }]
modelSolutions:
  - { source: solutions/model.cpp, language: cpp }
  - { source: solutions/model.py, language: python }
"#;
        assert_eq!(Config::parse(several).unwrap().models().len(), 2);
    }
}
