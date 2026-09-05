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

/// A comparison in progress, fed the program's output as it arrives.
///
/// **The point is not the memory, it is the moment.** Held to the end, a
/// submission wrong on its first token still spends its whole limit, still
/// prints whatever it was going to print, and is only then found out. Compared
/// as it goes, it is decided in microseconds and can be stopped — which is what
/// makes a wrong answer, the commonest verdict there is, also the cheapest.
///
/// **One asymmetry runs through this and it is worth stating plainly.** Too
/// *much* output is decidable the moment it arrives: the expected tokens have
/// run out, so anything further is a difference. Too *little* is not decidable
/// at all until the stream ends, because a program that has printed half its
/// answer looks exactly like one that is about to print the rest. So a short
/// answer is found by [`Comparing::finish`] and never by [`Comparing::feed`].
pub struct Comparing<'a> {
    ours: std::str::SplitWhitespace<'a>,
    /// What the program has written since the last complete token.
    ///
    /// Bytes rather than text: a multi-byte character split across two chunks
    /// would otherwise be converted to a replacement character on the first of
    /// them, and the token would differ for a reason the program did not cause.
    holding: Vec<u8>,
    index: usize,
    decided: Option<Comparison>,
}

impl<'a> Comparing<'a> {
    /// Against a reference answer the Runner already holds, in full.
    ///
    /// Only the participant's side is a stream. The `.out` file is ours, it is
    /// small, and reading it twice would buy nothing.
    pub fn against(expected: &'a str) -> Self {
        Self {
            ours: expected.split_whitespace(),
            holding: Vec::new(),
            index: 0,
            decided: None,
        }
    }

    /// Takes what has arrived. `Some` once the answer is settled.
    ///
    /// Settled means settled: further bytes change nothing, and a caller that
    /// keeps feeding — a relay draining a pipe so the program does not take a
    /// `SIGPIPE` — costs only the copy.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<&Comparison> {
        if self.decided.is_some() {
            return self.decided.as_ref();
        }
        let held = self.holding.len();
        self.holding.extend_from_slice(bytes);

        // **Split at the last ASCII space, and everything before it is whole.**
        // An ASCII whitespace byte cannot be part of a multi-byte character, so
        // the prefix is safe to convert; the tokens in it are then found by the
        // same `split_whitespace` the whole-slice comparison uses, keeping the
        // Unicode-aware set of separators rather than quietly narrowing it.
        //
        // **Only the arriving bytes are searched, and that is not an
        // optimisation.** Whatever was held over from last time was held over
        // *because* it had no separator in it, so searching it again can only
        // find nothing — and searching it again is quadratic in the length of a
        // token the submission chooses. Measured before it was fixed: a program
        // printing one endless token spent 20 seconds of the judge's processor
        // time to reach a cap it should have hit in well under one. A
        // participant should not be able to buy that.
        let cut = held + bytes.iter().rposition(u8::is_ascii_whitespace)?;
        let whole: Vec<u8> = self.holding.drain(..=cut).collect();
        let whole = String::from_utf8_lossy(&whole);
        for got in whole.split_whitespace() {
            if let Some(found) = self.settle(Some(got)) {
                self.decided = Some(found);
                return self.decided.as_ref();
            }
        }
        None
    }

    /// No more is coming.
    pub fn finish(mut self) -> Comparison {
        if let Some(decided) = self.decided {
            return decided;
        }
        // Whatever is left had no whitespace after it, which is the ordinary
        // way a program ends: `printf("%d", answer)` with no newline.
        let last = std::mem::take(&mut self.holding);
        let last = String::from_utf8_lossy(&last);
        for got in last.split_whitespace() {
            if let Some(found) = self.settle(Some(got)) {
                return found;
            }
        }
        // And now, and only now, a side that ran out means something.
        match self.settle(None) {
            Some(found) => found,
            None => Comparison::Equal,
        }
    }

    /// One token of theirs against one of ours. `Some` ends it.
    fn settle(&mut self, got: Option<&str>) -> Option<Comparison> {
        let want = self.ours.next();
        if want.is_none() && got.is_none() {
            return None;
        }
        self.index += 1;
        if want == got {
            return None;
        }
        Some(Comparison::Different {
            token: self.index,
            expected: want.unwrap_or_default().to_owned(),
            actual: got.unwrap_or_default().to_owned(),
        })
    }
}

/// The whole of both sides at once, for a caller that has them.
///
/// **The same code as the streaming form**, deliberately: two comparisons that
/// could disagree would disagree about a verdict, and the one place it would
/// show is a participant told different things by two paths through the same
/// judge. `output-only@1` uses this one, where the answers arrive as files and
/// there is nothing to stream.
pub fn compare(expected: &[u8], actual: &[u8]) -> Comparison {
    // Lossy on purpose. A program that emitted invalid UTF-8 has produced wrong
    // output, and refusing to compare it would turn a wrong answer into an
    // infrastructure failure — which is a claim about the system, not about the
    // solution.
    let expected = String::from_utf8_lossy(expected);
    let mut comparing = Comparing::against(&expected);
    comparing.feed(actual);
    comparing.finish()
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

    /// **A token that arrives in pieces is still one token.**
    ///
    /// A pipe hands over whatever has been written, not whatever is meant; a
    /// four-digit answer can reach the reader as `12` and then `34`. Compared
    /// piece by piece it would be two wrong tokens, and the participant would
    /// be told their first answer was wrong when it was right.
    #[test]
    fn a_token_split_across_chunks_is_one_token() {
        let expected = "1234 5\n";
        let mut comparing = Comparing::against(expected);
        assert!(comparing.feed(b"12").is_none(), "nothing is settled yet");
        assert!(comparing.feed(b"34 ").is_none());
        assert!(comparing.feed(b"5").is_none());
        assert_eq!(comparing.finish(), Comparison::Equal);
    }

    /// **Too little is only knowable at the end, and this is the asymmetry the
    /// whole design turns on.**
    ///
    /// A program that has printed half its answer is indistinguishable from one
    /// about to print the rest. Deciding against it on `feed` would fail every
    /// correct submission that pauses to think.
    #[test]
    fn a_short_answer_is_decided_only_when_it_ends() {
        let mut comparing = Comparing::against("1 2 3\n");
        assert!(
            comparing.feed(b"1 2 ").is_none(),
            "two of three tokens is not yet a wrong answer",
        );
        assert_eq!(
            comparing.finish(),
            Comparison::Different {
                token: 3,
                expected: "3".into(),
                actual: String::new(),
            },
        );
    }

    /// **Too much is knowable at once**, which is the other half of it: the
    /// expected tokens have run out, so anything further can only be wrong.
    /// This is what stops a flooding submission before it floods.
    #[test]
    fn output_that_runs_on_is_decided_as_it_arrives() {
        let mut comparing = Comparing::against("1 2\n");
        assert!(comparing.feed(b"1 2 ").is_none());
        let found = comparing
            .feed(b"3 ")
            .expect("a token past the end of the answer settles it at once");
        assert!(matches!(found, Comparison::Different { token: 3, .. }));
    }

    /// A mismatch settles on the chunk that carries it, and not a byte later.
    #[test]
    fn a_wrong_token_settles_on_the_chunk_that_carries_it() {
        let mut comparing = Comparing::against("1 2 3\n");
        assert!(comparing.feed(b"1 ").is_none());
        assert!(
            comparing.feed(b"9 ").is_some(),
            "the second token was wrong and there was nothing left to learn",
        );
    }

    /// Settled is settled: a relay goes on draining the pipe so the program
    /// does not take a `SIGPIPE` before it can be stopped properly, and none of
    /// what it drains may change the answer.
    #[test]
    fn nothing_fed_after_a_decision_changes_it() {
        let mut comparing = Comparing::against("1\n");
        let decided = comparing.feed(b"9 ").cloned().expect("wrong at once");
        comparing.feed(b"1 1 1 ");
        assert_eq!(comparing.finish(), decided);
    }

    /// The two forms are one implementation, so this can only fail if somebody
    /// gives the slice version a shortcut of its own.
    #[test]
    fn feeding_it_a_byte_at_a_time_agrees_with_comparing_the_whole() {
        for (expected, actual) in [
            (&b"1 2 3\n"[..], &b"1 2 3\n"[..]),
            (b"1 2 3\n", b"1 9 3\n"),
            (b"1 2 3\n", b"1 2\n"),
            (b"1 2\n", b"1 2 3\n"),
            (b"3\n", b"   3   "),
            (b"", b"  \n"),
        ] {
            let whole = compare(expected, actual);
            let text = String::from_utf8_lossy(expected);
            let mut comparing = Comparing::against(&text);
            for byte in actual {
                comparing.feed(&[*byte]);
            }
            assert_eq!(
                comparing.finish(),
                whole,
                "{:?} against {:?}",
                String::from_utf8_lossy(expected),
                String::from_utf8_lossy(actual),
            );
        }
    }

    /// **The offset the linear scan rests on.**
    ///
    /// `feed` searches only what has just arrived, because what it was holding
    /// had no separator by construction. That turns the position it finds into
    /// an offset needing the held length added back, and getting that addition
    /// wrong cuts the token in the wrong place — which shows up as a wrong
    /// answer for a correct program, and only for one whose output happens to
    /// straddle a chunk.
    #[test]
    fn a_token_held_across_many_chunks_is_cut_in_the_right_place() {
        let long: String = std::iter::repeat_n('7', 5000).collect();
        let expected = format!("{long} 1\n");
        let mut comparing = Comparing::against(&expected);
        for chunk in long.as_bytes().chunks(64) {
            assert!(comparing.feed(chunk).is_none(), "no separator has arrived");
        }
        assert!(
            comparing.feed(b" 1 ").is_none(),
            "and both tokens are right"
        );
        assert_eq!(comparing.finish(), Comparison::Equal);
    }

    /// Invalid bytes are a wrong answer, not a broken evaluation.
    #[test]
    fn output_that_is_not_utf8_still_compares() {
        assert!(!compare(b"3\n", &[0xff, 0xfe, 0x00]).equal());
    }
}
