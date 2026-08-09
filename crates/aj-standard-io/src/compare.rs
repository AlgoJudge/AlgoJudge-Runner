//! Comparing output when the package has no checker.
//!
//! **Open question — the format does not settle this.** `PACKAGE_FORMAT.md` says
//! only that an absent checker means "the `.out` files decide" and that the
//! comparison is then all or nothing. Which whitespace differences count is
//! listed in the technology analysis as still open (Q-H, blocking the first full
//! flow), together with float tolerance and whether either is configurable per
//! problem.
//!
//! The reading implemented here is **token comparison**: both sides are split on
//! whitespace and the sequences must match. It is what nearly every judge does
//! by default, and it is the reading that does not reject a correct solution
//! over a trailing newline — the single most common complaint against exact
//! comparison, and one a participant cannot debug from a verdict.
//!
//! Whichever way it is settled, it is this function. A problem needing anything
//! finer — a float tolerance, a set rather than a sequence — declares a checker,
//! which is what checkers are for.

/// What a comparison found, with enough to tell a participant where to look.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comparison {
    Equal,
    Different {
        /// One-based index of the first token that differed, or of the point
        /// where one side ran out.
        token: usize,
        expected: String,
        actual: String,
    },
}

impl Comparison {
    pub fn equal(&self) -> bool {
        matches!(self, Comparison::Equal)
    }

    /// A line for the participant's `note`, naming what differed without
    /// quoting so much that the expected output is disclosed.
    pub fn note(&self) -> String {
        match self {
            Comparison::Equal => String::new(),
            Comparison::Different {
                token,
                expected,
                actual,
            } => {
                // Bounded on purpose: a `note` travels in the result document
                // for every test, and a program that printed a megabyte on one
                // line would otherwise put it there.
                format!(
                    "token {token} differs: expected {}, got {}",
                    quote(expected),
                    quote(actual),
                )
            }
        }
    }
}

fn quote(token: &str) -> String {
    const LIMIT: usize = 32;
    if token.is_empty() {
        return "brak".to_owned();
    }
    let shown: String = token.chars().take(LIMIT).collect();
    if shown.chars().count() < token.chars().count() {
        format!("\"{shown}…\"")
    } else {
        format!("\"{shown}\"")
    }
}

pub fn compare(expected: &[u8], actual: &[u8]) -> Comparison {
    // Lossy on purpose. A program that emitted invalid UTF-8 has produced wrong
    // output, and refusing to compare it would turn a wrong answer into an
    // infrastructure failure — which is a claim about the system, not about the
    // solution.
    let expected = String::from_utf8_lossy(expected);
    let actual = String::from_utf8_lossy(actual);

    let mut theirs = actual.split_whitespace();
    let mut ours = expected.split_whitespace();
    let mut index = 0;

    loop {
        index += 1;
        match (ours.next(), theirs.next()) {
            (None, None) => return Comparison::Equal,
            (want, got) => {
                if want == got {
                    continue;
                }
                return Comparison::Different {
                    token: index,
                    expected: want.unwrap_or_default().to_owned(),
                    actual: got.unwrap_or_default().to_owned(),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_output_matches() {
        assert!(compare(b"3\n", b"3\n").equal());
    }

    /// The reason this is token comparison rather than byte comparison. Every
    /// one of these is a correct solution.
    #[test]
    fn whitespace_that_does_not_change_the_answer_is_tolerated() {
        for actual in [
            &b"3"[..],
            b"3\n",
            b"3\n\n",
            b"3 \n",
            b"  3  ",
            b"\r\n3\r\n",
            b"3\t",
        ] {
            assert!(
                compare(b"3\n", actual).equal(),
                "{:?} should have matched",
                String::from_utf8_lossy(actual),
            );
        }
    }

    #[test]
    fn several_values_are_compared_in_order() {
        assert!(compare(b"1 2 3\n", b"1\n2\n3\n").equal());
        assert!(!compare(b"1 2 3\n", b"1 3 2\n").equal());
    }

    #[test]
    fn a_difference_says_where_it_was() {
        let found = compare(b"1 2 3\n", b"1 9 3\n");
        assert_eq!(
            found,
            Comparison::Different {
                token: 2,
                expected: "2".into(),
                actual: "9".into(),
            },
        );
        assert!(found.note().contains("token 2"));
    }

    #[test]
    fn output_that_stops_early_or_runs_on_is_different() {
        assert!(!compare(b"1 2 3\n", b"1 2\n").equal());
        assert!(!compare(b"1 2\n", b"1 2 3\n").equal());
        assert!(!compare(b"1\n", b"").equal());
        assert!(compare(b"", b"   \n").equal(), "nothing matches nothing");
    }

    #[test]
    fn a_long_token_is_not_quoted_whole_into_a_note() {
        let long = "x".repeat(5_000);
        let found = compare(b"7\n", long.as_bytes());
        assert!(
            found.note().len() < 200,
            "the note was {} bytes",
            found.note().len()
        );
    }

    /// Invalid bytes are a wrong answer, not a broken evaluation.
    #[test]
    fn output_that_is_not_utf8_still_compares() {
        assert!(!compare(b"3\n", &[0xff, 0xfe, 0x00]).equal());
    }
}
