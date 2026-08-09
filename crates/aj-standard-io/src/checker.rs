//! The checker contract, taken verbatim from SIO2.
//!
//! | | |
//! |---|---|
//! | `argv[1]` | the input file |
//! | `argv[2]` | the participant's output |
//! | `argv[3]` | the reference output |
//! | stdout, line 1 | `OK` or `WRONG` |
//! | stdout, line 2 | optional comment, shown to the participant |
//! | stdout, line 3 | optional integer 0–100, the percentage of the test's points |
//! | exit code | **always 0** |
//!
//! **The exit code rule is the load-bearing one.** A non-zero code means the
//! *system* failed, not that the answer was wrong. Those are different outcomes
//! for a participant and for an operator, and a judge that conflates them turns
//! a bug in a checker into a rejected submission — the participant is told
//! their correct solution is wrong, and nobody is told the checker is broken.

/// What a checker said, once its output has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub accepted: bool,
    /// Shown to the participant. **Originates beside untrusted code** — a
    /// checker may echo a program's output into it — so it is carried as text
    /// and rendered as text, and nothing here pretends to sanitise it.
    pub comment: String,
    /// 0–100. Absent in the output means full marks for an accepted test.
    pub percentage: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum Broken {
    /// The one that must never be read as a wrong answer.
    #[error("the checker exited with {0}; its exit code is always 0, so this is a system failure and not a verdict")]
    ExitCode(i64),

    #[error("the checker said {0:?}, and the first line is OK or WRONG")]
    FirstLine(String),

    #[error("the checker gave {0:?} as a percentage, and it is an integer from 0 to 100")]
    Percentage(String),
}

/// Reads a checker's output, or says the checker is broken.
///
/// The two are different returns rather than one value with a flag, because
/// every caller has to decide between "the solution was wrong" and "the
/// evaluation failed", and a type that lets them forget is a type that will be
/// forgotten.
pub fn checker_said(exit_code: i64, stdout: &[u8]) -> Result<Checked, Broken> {
    if exit_code != 0 {
        return Err(Broken::ExitCode(exit_code));
    }

    let text = String::from_utf8_lossy(stdout);
    let mut lines = text.lines();

    let accepted = match lines.next().map(str::trim) {
        Some("OK") => true,
        Some("WRONG") => false,
        other => return Err(Broken::FirstLine(other.unwrap_or("").to_owned())),
    };

    let comment = lines.next().unwrap_or("").trim().to_owned();

    let percentage = match lines.next().map(str::trim).filter(|l| !l.is_empty()) {
        // Absent means full marks — for an accepted test. A rejected one is
        // worth nothing whatever the checker left out.
        None => {
            if accepted {
                100
            } else {
                0
            }
        }
        Some(stated) => match stated.parse::<u32>() {
            Ok(value) if value <= 100 => value,
            _ => return Err(Broken::Percentage(stated.to_owned())),
        },
    };

    Ok(Checked {
        accepted,
        comment,
        // A checker that says WRONG and 100 is contradicting itself. The
        // rejection wins: it is the explicit statement, and awarding full marks
        // for a wrong answer is the worse way to resolve it.
        percentage: if accepted { percentage } else { 0 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_alone_is_full_marks() {
        let said = checker_said(0, b"OK\n").unwrap();
        assert!(said.accepted);
        assert_eq!(said.percentage, 100);
        assert_eq!(said.comment, "");
    }

    #[test]
    fn a_comment_and_a_percentage_are_both_read() {
        let said = checker_said(0, b"OK\nclose enough\n60\n").unwrap();
        assert!(said.accepted);
        assert_eq!(said.comment, "close enough");
        assert_eq!(said.percentage, 60);
    }

    #[test]
    fn wrong_is_worth_nothing_even_with_a_percentage() {
        let said = checker_said(0, b"WRONG\nexpected 7\n100\n").unwrap();
        assert!(!said.accepted);
        assert_eq!(said.percentage, 0);
        assert_eq!(said.comment, "expected 7");
    }

    /// The rule the whole contract rests on.
    #[test]
    fn a_non_zero_exit_is_a_system_failure_and_not_a_wrong_answer() {
        let broken = checker_said(1, b"WRONG\n").unwrap_err();
        assert!(matches!(broken, Broken::ExitCode(1)));

        // Even when it printed something perfectly well-formed.
        assert!(matches!(
            checker_said(139, b"OK\n").unwrap_err(),
            Broken::ExitCode(139),
        ));
    }

    #[test]
    fn anything_but_ok_or_wrong_on_the_first_line_is_a_broken_checker() {
        for output in [&b""[..], b"ok\n", b"ACCEPTED\n", b"\n", b"0\nOK\n"] {
            assert!(
                matches!(checker_said(0, output), Err(Broken::FirstLine(_))),
                "{:?} should not have parsed",
                String::from_utf8_lossy(output),
            );
        }
    }

    #[test]
    fn a_percentage_outside_zero_to_a_hundred_is_a_broken_checker() {
        for stated in ["101", "-1", "half", "50.5"] {
            let output = format!("OK\n\n{stated}\n");
            assert!(
                matches!(
                    checker_said(0, output.as_bytes()),
                    Err(Broken::Percentage(_))
                ),
                "{stated} should not have parsed",
            );
        }
    }

    /// A checker's comment reaches the participant and is produced beside
    /// untrusted code, so it must survive being anything at all.
    #[test]
    fn a_comment_may_contain_whatever_the_program_printed() {
        let said = checker_said(0, "OK\n<script>alert(1)</script> — ąęś\n".as_bytes()).unwrap();
        assert_eq!(said.comment, "<script>alert(1)</script> — ąęś");
    }
}
