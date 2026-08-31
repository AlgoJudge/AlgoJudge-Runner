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

    /// A line for the participant's `note`: **where** the output differed, and
    /// what they produced there.
    ///
    /// **It does not name the expected token, and that is the whole point.** It
    /// did until 2026-08-31, quoted to 32 characters on the stated grounds that
    /// a bound keeps the expected output from being disclosed. A bound is not a
    /// boundary: an answer is usually far shorter than 32 characters, so the
    /// whole of it was disclosed — and this note travels for **every** test in
    /// the result document, so one deliberately wrong submission came back with
    /// the first differing token of every failing test at once. For a problem
    /// answered with one number per test, that is the answer key.
    ///
    /// `input_mount` was changed on 2026-08-09 so that `<name>.out` never
    /// reaches the participant's container. This was the same file reaching
    /// them by the other road, and `output-only@1` shares this function — where
    /// the answers *are* the submission, so it mattered more there still.
    ///
    /// What is left is what they can act on: which token, and what they put
    /// there. Their own output discloses nothing they did not send.
    pub fn note(&self) -> String {
        match self {
            Comparison::Equal => String::new(),
            Comparison::Different { token, actual, .. } => {
                // Bounded on purpose: a `note` travels in the result document
                // for every test, and a program that printed a megabyte on one
                // line would otherwise put it there.
                format!("token {token} differs: got {}", quote(actual))
            }
        }
    }
}

fn quote(token: &str) -> String {
    const LIMIT: usize = 32;
    if token.is_empty() {
        // English, like everything the Runner writes itself (2026-08-09). This
        // note travels to the participant and can become the verdict's own
        // text, so it is not the place for the one Polish word left in it.
        return "nothing".to_owned();
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

    /// **The note must not name the expected token.**
    ///
    /// It is read out of `<name>.out`, and this note travels for every test in
    /// the result document — so one deliberately wrong submission would come
    /// back with the first differing token of every failing test, which for a
    /// problem answered in one number per test is the answer key.
    #[test]
    fn a_note_never_discloses_the_expected_output() {
        let found = compare(b"1 42 3\n", b"1 0 3\n");
        let note = found.note();

        assert!(note.contains("token 2"), "{note}");
        assert!(
            note.contains(r#""0""#),
            "their own output is theirs to read: {note}",
        );
        assert!(
            !note.contains("42"),
            "the expected token reached the note: {note}",
        );
    }

    /// **Anything the Runner writes itself is English** (2026-08-09), and a
    /// `note` is named in that rule. This one also reaches the participant as
    /// the submission's verdict by way of `score.rs`.
    #[test]
    fn a_token_the_program_never_printed_is_reported_in_english() {
        let found = compare(b"1 2\n", b"1\n");
        let note = found.note();
        assert!(note.ends_with("got nothing"), "{note}");
        assert!(note.is_ascii(), "{note}");
    }

    /// Invalid bytes are a wrong answer, not a broken evaluation.
    #[test]
    fn output_that_is_not_utf8_still_compares() {
        assert!(!compare(b"3\n", &[0xff, 0xfe, 0x00]).equal());
    }
}
