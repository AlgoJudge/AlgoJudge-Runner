//! Which processors this Runner was given, and whether it was given any choice.
//!
//! **A timed run is pinned to what the Runner itself may use, and to nothing
//! when the Runner may use everything.** Those are two different situations and
//! only one of them is an instruction.
//!
//! *Given a set* means an operator wrote `cpuset` on this Runner's container.
//! That has to be passed on to every job container, because a job container is
//! the daemon's child and not the Runner's: it inherits no affinity at all, so a
//! Runner confined to two processors would otherwise start jobs across the whole
//! host and quietly undo the split its operator drew.
//!
//! *Given everything* is the default, and pinning inside it is a worse idea than
//! it looks. Several Runners on one host each choose independently -- nothing
//! coordinates them -- so three jobs can land on one processor while another
//! sits idle. The pin then also forbids the one repair available: the kernel
//! cannot move a starved job to a free processor. Measured 2026-09-03 on twelve
//! Runners over sixteen processors, that produced fifteen submissions in a
//! hundred and fifty reported as `Time limit exceeded` while inside their
//! limits.
//!
//! **What is lost by not pinning is smaller than it sounds, and it is measured.**
//! `--cpus=1` is on every container regardless, so a run cannot buy more
//! processor time by spreading over cores; `cpu.stat` sums the whole subtree, so
//! threads spend the budget faster rather than escaping it. What the pin bought
//! on top was wall clock inside a single CFS period, and
//! `a_pinned_run_is_given_one_core_and_the_one_it_asked_for` records the
//! measurement: four spinners burning 1.4 s took 1835/1867/1886 ms unpinned
//! against 1902/1909/1919 ms pinned, the quota having equalized them. Since
//! 2026-09-02 a limit is processor time, so that residue decides nothing.

/// The processors this Runner may use, when that is fewer than the host has.
///
/// `None` means *use the host's own judgment* -- either because nobody narrowed
/// this process, or because the question could not be answered here, which is
/// the same answer for the same reason: an unasked-for pin is the failure this
/// module exists to avoid.
pub fn allowed() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let mine = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))?
        .trim();
    let online = std::fs::read_to_string("/sys/devices/system/cpu/online").ok()?;
    restricted(mine, online.trim())
}

/// The set to pin to, or `None` when it is the whole machine.
///
/// **Compared as sets, not as text.** `0-15` and `0,1,2,…,15` are one answer
/// spelled two ways, and the kernel is free to use either.
fn restricted(mine: &str, online: &str) -> Option<String> {
    let ours = parse(mine)?;
    if ours.is_empty() {
        return None;
    }
    match parse(online) {
        // Without the host's own list there is nothing to compare against, and
        // guessing would mean pinning a Runner nobody narrowed.
        None => None,
        Some(all) if ours.len() >= all.len() => None,
        Some(_) => Some(mine.to_owned()),
    }
}

/// `0-3,8` into the processors it names. `None` if it names none of them.
fn parse(list: &str) -> Option<Vec<usize>> {
    let mut found = Vec::new();
    for group in list.trim().split(',').filter(|g| !g.is_empty()) {
        match group.split_once('-') {
            None => found.push(group.trim().parse().ok()?),
            Some((first, last)) => {
                let first: usize = first.trim().parse().ok()?;
                let last: usize = last.trim().parse().ok()?;
                if last < first || last - first > 4096 {
                    return None;
                }
                found.extend(first..=last);
            }
        }
    }
    if found.is_empty() {
        None
    } else {
        Some(found)
    }
}

/// How many processors this Runner was given, where it was given a set.
///
/// `None` is the whole machine, and there it stays `None` rather than becoming
/// the host's count: how many lanes to judge in is the operator's to say, and
/// nothing here would be checking it against a division anybody drew.
pub fn width() -> Option<usize> {
    allowed()
        .as_deref()
        .and_then(parse)
        .map(|found| found.len())
}

/// How many processors this host has online, where that can be read.
///
/// **Only ever used to refuse an impossible width.** A Runner given the whole
/// machine still pins nothing; this says how much "the whole machine" is, so an
/// operator who asks for sixty-four lanes on four processors is told at start
/// rather than discovering it as every submission taking longer than it should.
pub fn on_this_host() -> Option<usize> {
    let online = std::fs::read_to_string("/sys/devices/system/cpu/online").ok()?;
    parse(online.trim()).map(|found| found.len())
}

/// The processors each of `lanes` lanes may use, in lane order.
///
/// **Cut in the order this Runner's own list names them.** On a host whose
/// thread siblings are `0,8` rather than `0,1`, the order an operator wrote is
/// the only thing that says which processors belong together -- Ops asks them
/// to read `thread_siblings_list` before writing it -- so sorting here, or
/// cutting on the numbers rather than on the list, would take that back.
///
/// `None` in a lane means **pin nothing**, at any width. A Runner given the
/// whole machine is the measurement in this module's head, and widening it does
/// not turn an unasked-for pin into a good idea.
pub fn cut(lanes: usize) -> Vec<Option<String>> {
    split(allowed().as_deref(), lanes)
}

/// The same cut as a function of what was read, so every case of it is testable
/// on a host that was never divided.
fn split(mine: Option<&str>, lanes: usize) -> Vec<Option<String>> {
    // One lane is what a Runner has always had, spelled the way it has always
    // been spelled: not a width-1 case of something new, but the same answer.
    if lanes <= 1 {
        return vec![mine.map(str::to_owned)];
    }
    // Unreadable is unrestricted, which is what `restricted` already decides
    // for the same reason: every way of failing to read the question ends in
    // the answer that pins nothing.
    let Some(found) = mine.and_then(parse) else {
        return vec![None; lanes];
    };
    // **Not enough processors to give each lane one of its own.** Every lane
    // gets the whole set, which is a container that starts; the refusal belongs
    // where an operator can be told what to change, and that is the Runner's
    // start-up rather than here.
    if found.len() < lanes {
        return vec![mine.map(str::to_owned); lanes];
    }

    let each = found.len() / lanes;
    let over = found.len() % lanes;
    let mut at = 0;
    (0..lanes)
        .map(|lane| {
            // The first lanes take the remainder, so nothing of the operator's
            // set goes unused and no lane is ever given the empty one -- which
            // is a `cpuset_cpus` the daemon refuses.
            let width = each + usize::from(lane < over);
            let piece: Vec<String> = found[at..at + width].iter().map(usize::to_string).collect();
            at += width;
            Some(piece.join(","))
        })
        .collect()
}

/// The processors that share a physical core with this one, as the kernel says.
///
/// `None` where the host does not answer — a virtual machine that publishes no
/// topology, or a kernel built without it. Nothing is refused on a silence.
fn siblings_of(cpu: usize) -> Option<Vec<usize>> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    parse(std::fs::read_to_string(path).ok()?.trim())
}

/// Lanes that were given one **thread** of a core rather than a core.
///
/// **Measured 2026-09-15, and the reason this exists.** The same submission
/// judged in lanes of one thread measured a median 318 ms of processor time per
/// test against 196 ms in lanes of a whole core — and a time limit is processor
/// time, so 71% of that submission's tests went over a limit that none of them
/// reached at the wider setting. A lane holds a judged run, the judge reading
/// it, and the measuring shim; one thread is not enough processor for three
/// things, and nothing in the verdict says so.
///
/// Reported rather than refused: an operator may have a host with no siblings
/// to give, and a Runner that will not start is worse than one that says what
/// it would rather have.
pub fn threads_not_cores(lanes: &[Option<String>]) -> Vec<String> {
    thin(lanes, siblings_of)
}

/// The same, as a function of what was read, so it is testable off a host.
fn thin(lanes: &[Option<String>], siblings: impl Fn(usize) -> Option<Vec<usize>>) -> Vec<String> {
    let cut: Vec<Vec<usize>> = lanes
        .iter()
        .map(|lane| lane.as_deref().and_then(parse).unwrap_or_default())
        .collect();

    let mut said = Vec::new();
    for (index, mine) in cut.iter().enumerate() {
        for &cpu in mine {
            let Some(family) = siblings(cpu) else {
                continue;
            };
            for kin in family.into_iter().filter(|&kin| kin != cpu) {
                if mine.contains(&kin) {
                    continue;
                }
                let elsewhere = cut
                    .iter()
                    .position(|other| other.contains(&kin))
                    .map(|at| format!("lane {at}"))
                    .unwrap_or_else(|| "no lane of this Runner".to_owned());
                said.push(format!(
                    "lane {index} has cpu {cpu}, whose sibling cpu {kin} is in {elsewhere}"
                ));
            }
        }
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host whose siblings are `0-1`, `2-3`, ... as this machine's are.
    fn pairs(cpu: usize) -> Option<Vec<usize>> {
        let low = cpu - cpu % 2;
        Some(vec![low, low + 1])
    }

    #[test]
    fn a_lane_holding_a_whole_core_is_not_complained_about() {
        let lanes = vec![Some("0,1".to_owned()), Some("2,3".to_owned())];
        assert_eq!(thin(&lanes, pairs), Vec::<String>::new());
    }

    #[test]
    fn a_lane_holding_one_thread_of_a_core_is_named_with_where_its_sibling_went() {
        let lanes = vec![Some("0".to_owned()), Some("1".to_owned())];
        let said = thin(&lanes, pairs);
        assert_eq!(
            said.len(),
            2,
            "both halves of the split core are named: {said:?}"
        );
        assert!(said[0].contains("lane 0 has cpu 0"), "{said:?}");
        assert!(said[0].contains("sibling cpu 1 is in lane 1"), "{said:?}");
    }

    #[test]
    fn a_sibling_this_runner_was_never_given_is_said_to_be_nobodys() {
        // `0,2,4,6` is one thread of each of four cores -- the arrangement that
        // measured no better than four threads of two cores.
        let lanes = vec![Some("0".to_owned()), Some("2".to_owned())];
        let said = thin(&lanes, pairs);
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(
            said.iter()
                .all(|one| one.contains("no lane of this Runner")),
            "{said:?}"
        );
    }

    #[test]
    fn a_host_that_publishes_no_topology_is_not_complained_about() {
        let lanes = vec![Some("0".to_owned()), Some("1".to_owned())];
        assert_eq!(thin(&lanes, |_| None), Vec::<String>::new());
    }

    #[test]
    fn a_runner_that_pins_nothing_has_nothing_to_say() {
        assert_eq!(thin(&[None, None], pairs), Vec::<String>::new());
    }

    #[test]
    fn a_runner_that_may_use_the_whole_machine_pins_nothing() {
        assert_eq!(restricted("0-15", "0-15"), None);
        // The same set, spelled the way the kernel sometimes spells it.
        assert_eq!(restricted("0,1,2,3", "0-3"), None);
    }

    #[test]
    fn a_runner_given_a_set_hands_that_set_to_its_jobs() {
        assert_eq!(restricted("0-1", "0-15"), Some("0-1".to_owned()));
        assert_eq!(restricted("2,3", "0-15"), Some("2,3".to_owned()));
        assert_eq!(restricted("4", "0-7"), Some("4".to_owned()));
    }

    /// **Unanswerable is not the same as unrestricted, and both mean no pin.**
    /// A pin nobody asked for is what this module exists to prevent, so every
    /// way of failing to read the question ends in the same place.
    #[test]
    fn nothing_readable_pins_nothing() {
        assert_eq!(restricted("", "0-15"), None);
        assert_eq!(restricted("nonsense", "0-15"), None);
        assert_eq!(restricted("0-1", "also nonsense"), None);
        assert_eq!(restricted("0-1", ""), None);
    }

    /// A set that is not smaller than the host's is not a narrowing, whatever
    /// it says -- and a Runner told it may use more processors than exist is a
    /// misconfiguration this must not turn into a pin.
    #[test]
    fn a_set_no_smaller_than_the_machine_is_not_a_narrowing() {
        assert_eq!(restricted("0-15", "0-7"), None);
    }

    #[test]
    fn ranges_and_lists_name_the_same_processors() {
        assert_eq!(parse("0-3"), Some(vec![0, 1, 2, 3]));
        assert_eq!(parse("0,2,4"), Some(vec![0, 2, 4]));
        assert_eq!(parse("0-1,8-9"), Some(vec![0, 1, 8, 9]));
        assert_eq!(parse("7"), Some(vec![7]));
        assert_eq!(parse("3-2"), None);
        assert_eq!(parse(""), None);
    }

    /// **The cut is a division of the operator's set, not a choice of our own.**
    #[test]
    fn a_runner_given_processors_hands_each_lane_a_piece_of_them() {
        assert_eq!(
            split(Some("0,1,2,3"), 2),
            vec![Some("0,1".to_owned()), Some("2,3".to_owned())]
        );
        assert_eq!(
            split(Some("0-7"), 4),
            vec![
                Some("0,1".to_owned()),
                Some("2,3".to_owned()),
                Some("4,5".to_owned()),
                Some("6,7".to_owned())
            ]
        );
        assert_eq!(
            split(Some("0-3"), 4),
            vec![
                Some("0".to_owned()),
                Some("1".to_owned()),
                Some("2".to_owned()),
                Some("3".to_owned())
            ]
        );
    }

    /// **In the order it was written, and that is the whole of what a Runner
    /// knows about this host's topology.** Two threads of one core are `0,1` on
    /// one machine and `0,8` on another; an operator who read
    /// `thread_siblings_list` and wrote `0,8,1,9` meant two lanes of one core
    /// each, and sorting the numbers would hand each lane half of two cores.
    #[test]
    fn the_list_is_cut_in_the_order_it_was_written() {
        assert_eq!(
            split(Some("0,8,1,9"), 2),
            vec![Some("0,8".to_owned()), Some("1,9".to_owned())]
        );
    }

    #[test]
    fn a_set_that_does_not_divide_evenly_gives_the_first_lanes_the_extra() {
        assert_eq!(
            split(Some("0-4"), 2),
            vec![Some("0,1,2".to_owned()), Some("3,4".to_owned())]
        );
        // Every processor the operator named is in exactly one lane.
        let cut = split(Some("0-6"), 3);
        let named: Vec<usize> = cut
            .iter()
            .flat_map(|lane| parse(lane.as_deref().unwrap()).unwrap())
            .collect();
        assert_eq!(named, (0..=6).collect::<Vec<usize>>());
    }

    /// The measurement in this module's head does not stop applying because
    /// there are several lanes: a pin nobody asked for is the same mistake N
    /// times over.
    #[test]
    fn a_runner_given_the_whole_machine_pins_nothing_at_any_width() {
        assert_eq!(split(None, 4), vec![None, None, None, None]);
        assert_eq!(split(None, 1), vec![None]);
        // Unreadable is unrestricted here too.
        assert_eq!(split(Some("nonsense"), 3), vec![None, None, None]);
    }

    /// **A one-lane Runner is the code that was here before**, which is what
    /// makes an installation that sets nothing unchanged rather than newly
    /// arranged.
    #[test]
    fn one_lane_is_what_it_always_was() {
        assert_eq!(split(Some("2,3"), 1), vec![Some("2,3".to_owned())]);
        assert_eq!(split(Some("0-15"), 1), vec![Some("0-15".to_owned())]);
    }

    /// An empty `cpuset_cpus` is a container the daemon refuses, so a lane
    /// without a processor of its own must be given the whole set rather than
    /// nothing. The refusal that stops an operator from getting here lives at
    /// start-up, where it can name the variable.
    #[test]
    fn a_lane_is_never_given_no_processors() {
        let cut = split(Some("0,1"), 4);
        assert_eq!(cut.len(), 4);
        assert!(cut.iter().all(|lane| lane.as_deref() == Some("0,1")));
    }

    /// What [`width`] counts, on the readable half of it: processors, not
    /// characters and not commas.
    #[test]
    fn a_width_counts_processors_and_not_characters() {
        assert_eq!(parse("0-3,8").map(|found| found.len()), Some(5));
        assert_eq!(parse("7").map(|found| found.len()), Some(1));
    }
}
