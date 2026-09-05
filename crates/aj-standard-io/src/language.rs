//! What each toolchain needs before and while it runs.
//!
//! **Eighteen toolchains, three families.** The set was two — `cpp` and
//! `python` — on the argument that toolchains are what costs and the MVP needs
//! one or two. What that missed is that a *standard* is not a toolchain: a
//! course teaching C++17 and a course teaching C89 are not asking for a
//! different compiler, they are asking the same compiler for a different `-std`,
//! and refusing them cost a Runner release each. So the catalogue is a table
//! now, and adding a row is a data change.
//!
//! ## Two levels, because they answer two different questions
//!
//! A participant reading a problem header wants the **standard** — "C++17" — and
//! a participant choosing on the submit form needs the **toolchain**, because
//! `g++` and `clang++` disagree about enough that a submission accepted by one
//! is occasionally rejected by the other. The id carries both (`cpp17-gcc`) and
//! the label spells it out ("C++17 (GCC)"). Nothing here decides which of the
//! two a screen shows; that is the Client's, from these ids and its own labels.
//!
//! ## Family, and the two silent traps it exists to close
//!
//! Two things are keyed by language and are **not** per toolchain:
//!
//! - the forbidden-identifier dictionary (`policy.rs`), and
//! - `overrideLimits` in a package's `config.yml`.
//!
//! Both were keyed by the *whole* id while the whole id was `cpp` or `python`,
//! and both fail **silently** when it stops being: a dictionary asked about
//! `cpp17-gcc` finds no rules and reports **no violations**, which reads exactly
//! like a clean submission, and an `overrideLimits: { python: … }` stops
//! reaching `python3`, so every Python submission is quietly held to the C++
//! limit. Neither produces an error anybody would see.
//!
//! So each entry names a family, the lookups try **the id first and the family
//! second**, and `policy.rs` matches on the family as an enum — a fourth family
//! will not compile until somebody says what its rules are.

use std::collections::BTreeMap;

use aj_sandbox::SHIM;

/// What a toolchain has in common with the others that share its rules.
///
/// Deliberately an enum rather than a string: this is what `policy.rs` switches
/// on, and the whole point is that a new family cannot be added without
/// somebody deciding what the dictionary does about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    C,
    Cpp,
    Python,
}

impl Family {
    /// The key a package's `overrideLimits` and the policy profile use.
    pub fn as_str(self) -> &'static str {
        match self {
            Family::C => "c",
            Family::Cpp => "cpp",
            Family::Python => "python",
        }
    }
}

/// How a submission in one toolchain is built and started.
#[derive(Debug, Clone)]
pub struct Language {
    pub id: &'static str,
    /// What a person reads. Not derived from the id — "C89 / ANSI C (GCC)"
    /// carries a synonym no rule could produce from `c89-gcc`.
    pub label: &'static str,
    pub family: Family,
    pub image: String,
    /// The submission's file name inside the build container.
    pub source_name: &'static str,
    /// What a participant may upload for it, lower-case and with the dot.
    pub extensions: &'static [&'static str],
    /// Run in the build container. `None` for a language with nothing to build.
    pub build: Option<Vec<String>>,
    /// What the build leaves behind, and what the run container mounts.
    pub artefact: &'static str,
    /// Run in the test container. The caller wraps this to redirect input.
    pub start: Vec<String>,
}

impl Language {
    /// Both keys this submission's limits and rules may be written under,
    /// **least specific first**, so a toolchain's own entry is applied over its
    /// family's.
    pub fn keys(&self) -> [&str; 2] {
        [self.family.as_str(), self.id]
    }

    /// Whether this is a file the toolchain will accept.
    ///
    /// Checked before the build container is started, and refused as a
    /// compilation error rather than an infrastructure failure: choosing C++ and
    /// uploading `main.py` is the participant's own mistake, the compiler would
    /// have said the same thing thirty seconds later, and a message naming the
    /// extensions is more use than `main.py:1: error: expected expression`.
    pub fn accepts(&self, file_name: &str) -> bool {
        let lowered = file_name.to_ascii_lowercase();
        self.extensions.iter().any(|e| lowered.ends_with(e))
    }
}

/// Where each container sees things. Fixed rather than configurable: the
/// commands below name these paths, and a path that can move is a path that
/// will move in one of the two places.
pub const SOURCE: &str = "/src";
/// On the build container's own writable layer, and read back from there.
///
/// **The build gets no writable host path.** It was handed one at first, and a
/// container running as an unprivileged user could not write to a directory the
/// Runner owned — the fix is not to open that directory to everybody but to
/// take the artefact back through the runtime API. It is not a tmpfs either:
/// that is destroyed with the container, so the archive endpoint would find
/// nothing to hand over.
pub const BUILD_OUTPUT: &str = "/out";
pub const PROGRAM: &str = "/program";
pub const INPUT: &str = "/in";
/// Where the directory holding the run's own stdout is mounted.
///
/// One directory per run, holding one file, and root's: the submission
/// runs as `nobody` and cannot create, rename or read anything in it. What
/// it gets is the descriptor the shim opened before dropping privileges.
pub const OUTPUT: &str = "/aj-out";

// ── the images, by key ──────────────────────────────────────────────────────

/// GCC, and the image a package's C or C++ checker is built and run in.
pub const GCC: &str = "gcc";
pub const CLANG: &str = "clang";
pub const CPYTHON: &str = "cpython";
pub const PYPY: &str = "pypy";

/// The images each toolchain runs in, keyed rather than one named field each.
///
/// Pinned by the operator, reported as capabilities. A tag that moves means two
/// submissions to the same problem were judged by two compilers.
///
/// **A partial map is the normal case.** An operator names the one image they
/// have republished and the rest fall back to the compiled-in default, which is
/// what a struct of fields made awkward and what kept `Sandbox__Image__Cpp` and
/// `Sandbox__Image__Python` the only two things anybody could configure.
#[derive(Debug, Clone, Default)]
pub struct Images {
    named: BTreeMap<String, String>,
}

impl Images {
    /// Overrides one image. Chained, because that is how the environment reads.
    pub fn with(mut self, key: &str, image: impl Into<String>) -> Self {
        self.named.insert(key.to_owned(), image.into());
        self
    }

    /// The image for a key — the operator's, if they named one.
    pub fn named(&self, key: &str) -> Option<&str> {
        self.named
            .get(key)
            .map(String::as_str)
            .or_else(|| built_in_image(key))
    }

    /// Every image this Runner needs to judge the whole catalogue.
    ///
    /// For a caller that pulls or preflights them: the set, not one per
    /// toolchain, because eighteen toolchains share four images.
    pub fn all(&self) -> Vec<String> {
        let mut seen: Vec<String> = CATALOGUE
            .iter()
            .filter_map(|e| self.named(e.image))
            .map(str::to_owned)
            .collect();
        seen.sort();
        seen.dedup();
        seen
    }
}

fn built_in_image(key: &str) -> Option<&'static str> {
    match key {
        GCC => Some("algojudge/lang-gcc:local"),
        CLANG => Some("algojudge/lang-clang:local"),
        CPYTHON => Some("algojudge/lang-python:local"),
        PYPY => Some("algojudge/lang-pypy:local"),
        _ => None,
    }
}

// ── the catalogue ───────────────────────────────────────────────────────────

struct Entry {
    id: &'static str,
    label: &'static str,
    family: Family,
    image: &'static str,
    source_name: &'static str,
    extensions: &'static [&'static str],
    /// `{BUILD_OUTPUT}` and `{SOURCE}` are substituted before the shell sees it.
    build: &'static str,
    artefact: &'static str,
    start: &'static [&'static str],
}

const C: &[&str] = &[".c"];
const CPP: &[&str] = &[".cpp", ".cc", ".cxx", ".c++"];
const PY: &[&str] = &[".py"];

/// The eighteen, as the specification writes them.
///
/// The compile commands are the specified ones, with two additions that are the
/// Runner's own necessity rather than part of any of them:
///
/// - `mkdir -p {BUILD_OUTPUT} &&` before a compiler, so the step does not depend
///   on the image having declared the directory as well as the contract saying
///   it must;
/// - `cp {SOURCE}/main.py {BUILD_OUTPUT}/program.py &&` before a Python compile,
///   because the submission is mounted **read-only** and `py_compile` writes a
///   `__pycache__` beside the file it is given. Compiling it in place failed
///   with "Read-only file system", which reported every correct Python solution
///   as a compilation error.
///
/// `-O2` because a limit is stated against optimised code, and judging a debug
/// build would make every limit a different limit.
///
/// `-static` on every compiled row — and **not** for the reason this file gave
/// until 2026-08-22. It said the run container mounts the binary and not the
/// toolchain image's libraries, which describes a topology this pipeline has
/// never had: a test is started from the *same* image the build ran in, so
/// `libstdc++.so.6` and the loader are both right there, and a dynamically
/// linked submission would have run perfectly well. Checked, rather than
/// reasoned about, by trying it.
///
/// What it does buy is worth keeping, so the flag stays:
///
/// - the artefact stops depending on the image it happens to be run in, which
///   is what lets a run image be slimmed — or replaced with a distroless one —
///   without quietly changing what every submission is judged on; and
/// - the measured run does not include the loader resolving shared libraries.
///   A limit is stated against the program, and start-up that varies with the
///   image is not the program.
const CATALOGUE: &[Entry] = &[
    Entry {
        id: "c89-gcc",
        label: "C89 / ANSI C (GCC)",
        family: Family::C,
        image: GCC,
        source_name: "main.c",
        extensions: C,
        build: "mkdir -p {BUILD_OUTPUT} && gcc -O2 -std=c89 -pedantic-errors -static -o {BUILD_OUTPUT}/program {SOURCE}/main.c",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "c89-clang",
        label: "C89 / ANSI C (Clang)",
        family: Family::C,
        image: CLANG,
        source_name: "main.c",
        extensions: C,
        build: "mkdir -p {BUILD_OUTPUT} && clang -O2 -std=c89 -pedantic-errors -static -o {BUILD_OUTPUT}/program {SOURCE}/main.c",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "c99-gcc",
        label: "C99 (GCC)",
        family: Family::C,
        image: GCC,
        source_name: "main.c",
        extensions: C,
        build: "mkdir -p {BUILD_OUTPUT} && gcc -O2 -std=c99 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.c",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "c99-clang",
        label: "C99 (Clang)",
        family: Family::C,
        image: CLANG,
        source_name: "main.c",
        extensions: C,
        build: "mkdir -p {BUILD_OUTPUT} && clang -O2 -std=c99 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.c",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "c11-gcc",
        label: "C11 (GCC)",
        family: Family::C,
        image: GCC,
        source_name: "main.c",
        extensions: C,
        build: "mkdir -p {BUILD_OUTPUT} && gcc -O2 -std=c11 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.c",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "c11-clang",
        label: "C11 (Clang)",
        family: Family::C,
        image: CLANG,
        source_name: "main.c",
        extensions: C,
        build: "mkdir -p {BUILD_OUTPUT} && clang -O2 -std=c11 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.c",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "c23-gcc",
        label: "C23 (GCC)",
        family: Family::C,
        image: GCC,
        source_name: "main.c",
        extensions: C,
        build: "mkdir -p {BUILD_OUTPUT} && gcc -O2 -std=c23 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.c",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "c23-clang",
        label: "C23 (Clang)",
        family: Family::C,
        image: CLANG,
        source_name: "main.c",
        extensions: C,
        build: "mkdir -p {BUILD_OUTPUT} && clang -O2 -std=c23 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.c",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "cpp11-gcc",
        label: "C++11 (GCC)",
        family: Family::Cpp,
        image: GCC,
        source_name: "main.cpp",
        extensions: CPP,
        build: "mkdir -p {BUILD_OUTPUT} && g++ -O2 -std=c++11 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.cpp",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "cpp11-clang",
        label: "C++11 (Clang)",
        family: Family::Cpp,
        image: CLANG,
        source_name: "main.cpp",
        extensions: CPP,
        build: "mkdir -p {BUILD_OUTPUT} && clang++ -O2 -std=c++11 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.cpp",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "cpp17-gcc",
        label: "C++17 (GCC)",
        family: Family::Cpp,
        image: GCC,
        source_name: "main.cpp",
        extensions: CPP,
        build: "mkdir -p {BUILD_OUTPUT} && g++ -O2 -std=c++17 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.cpp",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "cpp17-clang",
        label: "C++17 (Clang)",
        family: Family::Cpp,
        image: CLANG,
        source_name: "main.cpp",
        extensions: CPP,
        build: "mkdir -p {BUILD_OUTPUT} && clang++ -O2 -std=c++17 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.cpp",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "cpp20-gcc",
        label: "C++20 (GCC)",
        family: Family::Cpp,
        image: GCC,
        source_name: "main.cpp",
        extensions: CPP,
        build: "mkdir -p {BUILD_OUTPUT} && g++ -O2 -std=c++20 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.cpp",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "cpp20-clang",
        label: "C++20 (Clang)",
        family: Family::Cpp,
        image: CLANG,
        source_name: "main.cpp",
        extensions: CPP,
        build: "mkdir -p {BUILD_OUTPUT} && clang++ -O2 -std=c++20 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.cpp",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "cpp23-gcc",
        label: "C++23 (GCC)",
        family: Family::Cpp,
        image: GCC,
        source_name: "main.cpp",
        extensions: CPP,
        build: "mkdir -p {BUILD_OUTPUT} && g++ -O2 -std=c++23 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.cpp",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "cpp23-clang",
        label: "C++23 (Clang)",
        family: Family::Cpp,
        image: CLANG,
        source_name: "main.cpp",
        extensions: CPP,
        build: "mkdir -p {BUILD_OUTPUT} && clang++ -O2 -std=c++23 -static -o {BUILD_OUTPUT}/program {SOURCE}/main.cpp",
        artefact: "program",
        start: &["{PROGRAM}/program"],
    },
    Entry {
        id: "python3",
        label: "Python 3 (CPython)",
        family: Family::Python,
        image: CPYTHON,
        source_name: "main.py",
        extensions: PY,
        // Not a compilation, but it is the step that turns a syntax error into
        // one failure a participant can read rather than into every test
        // failing with the same traceback.
        build: "cp {SOURCE}/main.py {BUILD_OUTPUT}/program.py && python3 -m py_compile {BUILD_OUTPUT}/program.py",
        artefact: "program.py",
        start: &["python3", "{PROGRAM}/program.py"],
    },
    Entry {
        id: "pypy3",
        label: "Python 3 (PyPy)",
        family: Family::Python,
        image: PYPY,
        source_name: "main.py",
        extensions: PY,
        build: "cp {SOURCE}/main.py {BUILD_OUTPUT}/program.py && pypy3 -m py_compile {BUILD_OUTPUT}/program.py",
        artefact: "program.py",
        start: &["pypy3", "{PROGRAM}/program.py"],
    },
];

/// The ids that were the whole catalogue until 2026-08-22, and what they mean now.
///
/// **Kept, and not out of kindness to old submissions.** `config.yml` names the
/// checker's language and a model solution's language, `PACKAGE_FORMAT.md`
/// documents both as `cpp` or `python`, and every package written against that
/// document would stop building its own checker the moment these ids went away
/// — a package the Runner cannot read is an infrastructure failure on every
/// submission to it, not a message anybody can act on.
///
/// `cpp` is C++20 because that is the `-std` the single C++ entry carried before
/// the catalogue existed, so a package judged yesterday is judged the same way
/// today. They resolve, and they are not offered: `catalogue()` returns the
/// eighteen.
const ALIASES: &[(&str, &str)] = &[("cpp", "cpp20-gcc"), ("python", "python3")];

/// Every toolchain a submission may name, in the order a form should offer them.
pub fn catalogue(images: &Images) -> Vec<Language> {
    CATALOGUE.iter().filter_map(|e| built(e, images)).collect()
}

pub fn for_id(id: &str, images: &Images) -> Option<Language> {
    let resolved = ALIASES
        .iter()
        .find(|(alias, _)| *alias == id)
        .map(|(_, to)| *to)
        .unwrap_or(id);

    CATALOGUE
        .iter()
        .find(|e| e.id == resolved)
        .and_then(|e| built(e, images))
}

fn built(entry: &Entry, images: &Images) -> Option<Language> {
    Some(Language {
        id: entry.id,
        label: entry.label,
        family: entry.family,
        image: images.named(entry.image)?.to_owned(),
        source_name: entry.source_name,
        extensions: entry.extensions,
        build: Some(shell(&places(entry.build))),
        artefact: entry.artefact,
        start: entry.start.iter().map(|part| places(part)).collect(),
    })
}

/// Substitutes the four container paths into a command the table states.
fn places(command: &str) -> String {
    command
        .replace("{BUILD_OUTPUT}", BUILD_OUTPUT)
        .replace("{SOURCE}", SOURCE)
        .replace("{PROGRAM}", PROGRAM)
        .replace("{INPUT}", INPUT)
}

fn shell(script: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

/// A POSIX single-quoted word. The only thing that cannot appear inside single
/// quotes is a single quote, so one is written by leaving and re-entering them.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Wraps a start command so the test's input arrives on standard input, through
/// the measuring shim where the image has one.
///
/// **`exec` in both arms, and that is what makes the test free.** The shell
/// replaces itself either way, so it is not a second process in the accounting
/// and not a second entry against the process limit; what it spends deciding is
/// inside the cgroup's total and outside the shim's report, which is the right
/// side of each. Without a shim this is exactly what it has always been: the
/// program as PID 1 with its input redirected.
///
/// The shim is not probed for. An image may be an operator's own -- the
/// catalogue lets a toolchain name one -- so the absence has to be handled where
/// the command is built rather than by a capability the Runner remembers.
pub fn with_input(start: &[String], test: &str, output: &str) -> Vec<String> {
    let program = start
        .iter()
        .map(|part| quoted(part))
        .collect::<Vec<_>>()
        .join(" ");
    let input = quoted(&format!("{INPUT}/{test}.in"));
    let output = quoted(output);
    // **The two branches differ in where stdout goes, and the difference is
    // legible afterwards.** With the shim it is a file the Runner reads back —
    // which keeps a flooding submission out of the daemon's log, where it was
    // measured writing 76 MB against a 64 MiB cap. Without one, stdout stays on
    // the container's stream and the collector counts it exactly as it always
    // did. Which happened is not guessed at: the shim creates the file even for
    // a program that printed nothing, so the file's existence is the answer.
    shell(&format!(
        "if [ -x {SHIM} ]; then exec {SHIM} {input} {output} {program}; \
         else exec {program} < {input}; fi"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_is_the_eighteen_the_specification_names() {
        let offered: Vec<&str> = catalogue(&Images::default()).iter().map(|l| l.id).collect();

        assert_eq!(
            offered,
            vec![
                "c89-gcc",
                "c89-clang",
                "c99-gcc",
                "c99-clang",
                "c11-gcc",
                "c11-clang",
                "c23-gcc",
                "c23-clang",
                "cpp11-gcc",
                "cpp11-clang",
                "cpp17-gcc",
                "cpp17-clang",
                "cpp20-gcc",
                "cpp20-clang",
                "cpp23-gcc",
                "cpp23-clang",
                "python3",
                "pypy3",
            ]
        );
    }

    /// Every row has to resolve to an image, or the entry is a toolchain that
    /// exists in the table and cannot be run — which `for_id` reports as "this
    /// Runner does not evaluate it", the least helpful possible message about a
    /// missing default.
    #[test]
    fn every_entry_names_an_image_that_has_a_default() {
        for entry in CATALOGUE {
            assert!(
                built_in_image(entry.image).is_some(),
                "{} names the image key {:?}, which has no default",
                entry.id,
                entry.image,
            );
        }
        assert_eq!(Images::default().all().len(), 4, "eighteen share four");
    }

    #[test]
    fn an_id_that_is_not_in_the_catalogue_is_not_a_language() {
        let images = Images::default();

        // Java was considered and left out: the JVM reserves address space
        // rather than allocating it, so `memoryBytes` would measure something
        // different for it than for everything else.
        assert!(for_id("java", &images).is_none());
        assert!(for_id("rust", &images).is_none());
        assert!(for_id("", &images).is_none());
        // The standard without the toolchain. The submit form offers ids, and
        // guessing which compiler somebody meant is exactly what two levels
        // exist to stop.
        assert!(for_id("cpp17", &images).is_none());
    }

    /// The two ids every package written against `PACKAGE_FORMAT.md` uses.
    #[test]
    fn the_ids_packages_were_written_with_still_resolve() {
        let images = Images::default();

        assert_eq!(for_id("cpp", &images).unwrap().id, "cpp20-gcc");
        assert_eq!(for_id("python", &images).unwrap().id, "python3");

        assert!(
            !catalogue(&images).iter().any(|l| l.id == "cpp"),
            "an alias resolves; it is not offered",
        );
    }

    /// The trap this whole field exists for: a dictionary or an `overrideLimits`
    /// asked about `cpp17-gcc` alone finds nothing and says so silently.
    #[test]
    fn a_toolchain_carries_its_family_as_well_as_itself() {
        let images = Images::default();

        assert_eq!(
            for_id("cpp17-gcc", &images).unwrap().keys(),
            ["cpp", "cpp17-gcc"]
        );
        assert_eq!(
            for_id("pypy3", &images).unwrap().keys(),
            ["python", "pypy3"]
        );
        assert_eq!(
            for_id("c99-clang", &images).unwrap().keys(),
            ["c", "c99-clang"]
        );

        // Least specific first, because the caller applies them in order and
        // the toolchain's own entry has to win.
        for language in catalogue(&images) {
            assert_eq!(language.keys()[0], language.family.as_str());
            assert_eq!(language.keys()[1], language.id);
        }
    }

    #[test]
    fn a_file_is_accepted_by_extension_whatever_its_case() {
        let images = Images::default();
        let cpp = for_id("cpp17-gcc", &images).unwrap();

        for named in ["main.cpp", "Main.CPP", "a.cc", "a.cxx", "a.c++"] {
            assert!(cpp.accepts(named), "{named} was refused");
        }
        for named in ["main.c", "main.py", "main", "cpp"] {
            assert!(!cpp.accepts(named), "{named} was accepted");
        }

        assert!(for_id("c11-gcc", &images).unwrap().accepts("main.c"));
        assert!(!for_id("c11-gcc", &images).unwrap().accepts("main.cpp"));
        assert!(for_id("pypy3", &images).unwrap().accepts("solution.py"));
    }

    /// An operator republishes one image; the rest stay where they were.
    #[test]
    fn a_named_image_overrides_one_default_and_not_the_others() {
        let images = Images::default().with(GCC, "ghcr.io/algojudge/lang-gcc:1.2.3");

        assert_eq!(
            for_id("cpp17-gcc", &images).unwrap().image,
            "ghcr.io/algojudge/lang-gcc:1.2.3"
        );
        assert_eq!(
            for_id("cpp17-clang", &images).unwrap().image,
            "algojudge/lang-clang:local"
        );
    }

    #[test]
    fn the_shim_is_given_the_input_and_the_fallback_redirects_it() {
        let wrapped = with_input(
            &["python3".into(), "/program/program.py".into()],
            "1a",
            "/aj-out/stdout",
        );
        let script = wrapped.last().unwrap();

        // Through the shim both files are arguments, because the shim opens
        // both -- the input to read and the output to write. **Their order is
        // the assertion**: swapped, the shim would truncate the test's input
        // and feed the submission its own empty output, which is a wrong answer
        // rather than an error anybody would see.
        let through_the_shim = concat!(
            "exec /usr/local/bin/aj-shim ",
            "'/in/1a.in' '/aj-out/stdout' 'python3' '/program/program.py'",
        );
        assert!(script.contains(through_the_shim), "got {script}");
        // Without one the input is a redirect and the output stays on the
        // container's stream, which is what it has always been.
        assert!(
            script.contains("exec 'python3' '/program/program.py' < '/in/1a.in'"),
            "got {script}"
        );
    }

    /// **Both arms, or the shell is a process in the accounting.** It would also
    /// be a second entry against a process limit set at sixteen, and a signal
    /// aimed at the submission would reach the shell instead.
    #[test]
    fn neither_arm_leaves_a_shell_behind() {
        let script = with_input(&["/program/program".into()], "0a", "/aj-out/stdout")
            .pop()
            .unwrap();

        assert_eq!(script.matches("exec ").count(), 2, "got {script}");
        for arm in script.split("; ") {
            let arm = arm
                .trim()
                .trim_start_matches("then ")
                .trim_start_matches("else ");
            if arm.starts_with("if ") || arm == "fi" {
                continue;
            }
            assert!(arm.starts_with("exec "), "an arm that does not exec: {arm}");
        }
    }

    /// A quote in a path would otherwise end the quoting and hand the rest of
    /// the word to the shell as syntax.
    #[test]
    fn a_word_carrying_a_quote_stays_one_word() {
        assert_eq!(quoted("plain"), "'plain'");
        assert_eq!(quoted("a'b"), "'a'\\''b'");
    }

    /// Every compiled submission is judged against an optimised, statically
    /// linked build, because the run container mounts the binary and nothing
    /// else. Asserted over the whole table rather than one row: this is the
    /// property a new row is most likely to be copied without.
    #[test]
    fn every_compiled_row_is_static_and_optimised() {
        for language in catalogue(&Images::default()) {
            let script = language.build.clone().unwrap().last().unwrap().clone();

            assert!(
                script.contains(&format!("{BUILD_OUTPUT}/{}", language.artefact)),
                "{}: {script}",
                language.id,
            );

            if language.family == Family::Python {
                assert!(script.contains("py_compile"), "{}: {script}", language.id);
                assert!(
                    script.starts_with(&format!("cp {SOURCE}/main.py")),
                    "{}: the copy before the compile is what makes a read-only \
                     mount work: {script}",
                    language.id,
                );
            } else {
                assert!(script.contains("-static"), "{}: {script}", language.id);
                assert!(script.contains("-O2"), "{}: {script}", language.id);
            }
        }
    }

    /// The placeholders are substituted, and no row keeps one by accident.
    #[test]
    fn no_command_reaches_a_container_with_a_placeholder_left_in_it() {
        for language in catalogue(&Images::default()) {
            let commands: Vec<String> = language
                .build
                .clone()
                .unwrap_or_default()
                .into_iter()
                .chain(language.start.clone())
                .collect();

            for command in commands {
                assert!(!command.contains('{'), "{}: {command}", language.id);
            }
        }
    }
}
