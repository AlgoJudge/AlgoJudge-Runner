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
//! against 1902/1909/1919 ms pinned, the quota having equalised them. Since
//! 2026-09-02 a limit is processor time, so that residue decides nothing.

/// The processors this Runner may use, when that is fewer than the host has.
///
/// `None` means *use the host's own judgement* -- either because nobody narrowed
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
