//! What each language needs before and while it runs.
//!
//! Two today, by decision: C++ and Python. The set is deliberately small — the
//! MVP is scoped to one or two languages, and the toolchains are the thing that
//! costs, not the abstraction over them.

/// How a submission in one language is built and started.
#[derive(Debug, Clone)]
pub struct Language {
    pub id: &'static str,
    pub image: String,
    /// The submission's file name inside the build container.
    pub source_name: &'static str,
    /// Run in the build container. `None` for a language with nothing to build.
    pub build: Option<Vec<String>>,
    /// What the build leaves behind, and what the run container mounts.
    pub artefact: &'static str,
    /// Run in the test container. The caller wraps this to redirect input.
    pub start: Vec<String>,
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

pub fn for_id(id: &str, images: &Images) -> Option<Language> {
    match id {
        "cpp" => Some(Language {
            id: "cpp",
            image: images.cpp.clone(),
            source_name: "main.cpp",
            // Statically linked, because the run container mounts the binary
            // and **not** the toolchain image's libraries. `-O2` because a
            // limit is stated against optimised code, and judging a debug
            // build would make every limit a different limit.
            build: Some(shell(&format!(
                "mkdir -p {BUILD_OUTPUT} && g++ -O2 -std=c++20 -static                  -o {BUILD_OUTPUT}/program {SOURCE}/main.cpp"
            ))),
            artefact: "program",
            start: vec![format!("{PROGRAM}/program")],
        }),

        "python" => Some(Language {
            id: "python",
            image: images.python.clone(),
            source_name: "main.py",
            // Not a compilation, but it is the step that turns a syntax error
            // into one failure a participant can read rather than into every
            // test failing with the same traceback.
            //
            // Copied **before** it is compiled, and compiled where the copy
            // landed. `py_compile` writes a `__pycache__` beside the file it is
            // given, and the submission is mounted read-only — compiling it in
            // place failed with "Read-only file system", which reported every
            // correct Python solution as a compilation error.
            build: Some(shell(&format!(
                "cp {SOURCE}/main.py {BUILD_OUTPUT}/program.py \
                 && python3 -m py_compile {BUILD_OUTPUT}/program.py"
            ))),
            artefact: "program.py",
            start: vec![
                "python3".into(),
                format!("{PROGRAM}/program.py"),
            ],
        }),

        _ => None,
    }
}

/// The images each language runs in.
///
/// Pinned by the operator, reported as capabilities. A tag that moves means two
/// submissions to the same problem were judged by two compilers.
#[derive(Debug, Clone)]
pub struct Images {
    pub cpp: String,
    pub python: String,
}

impl Default for Images {
    fn default() -> Self {
        Self {
            cpp: "algojudge/lang-cpp:local".into(),
            python: "algojudge/lang-python:local".into(),
        }
    }
}

fn shell(script: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

/// Wraps a start command so the test's input arrives on standard input.
///
/// `exec` on purpose: the shell replaces itself, so the process limit counts
/// the program rather than the program plus a shell, and a signal reaches what
/// it was aimed at.
pub fn with_input(start: &[String], test: &str) -> Vec<String> {
    let quoted: Vec<String> = start.iter().map(|part| format!("'{part}'")).collect();
    shell(&format!("exec {} < {INPUT}/{test}.in", quoted.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_decided_languages_are_known_and_nothing_else_is() {
        let images = Images::default();
        assert!(for_id("cpp", &images).is_some());
        assert!(for_id("python", &images).is_some());

        // Java was considered and left out: the JVM reserves address space
        // rather than allocating it, so `memoryKib` would measure something
        // different for it than for everything else.
        assert!(for_id("java", &images).is_none());
        assert!(for_id("", &images).is_none());
    }

    #[test]
    fn input_is_redirected_by_the_shell_replacing_itself() {
        let wrapped = with_input(&["python3".into(), "/program/program.py".into()], "1a");
        let script = wrapped.last().unwrap();

        assert!(script.starts_with("exec "), "got {script}");
        assert!(script.contains("< /in/1a.in"), "got {script}");
        assert!(script.contains("'python3' '/program/program.py'"));
    }

    /// A C++ submission is judged against an optimised, statically linked
    /// build, because the run container mounts the binary and nothing else.
    #[test]
    fn cpp_is_built_static_and_optimised() {
        let built = for_id("cpp", &Images::default()).unwrap();
        let script = built.build.unwrap().last().unwrap().clone();

        assert!(script.contains("-static"));
        assert!(script.contains("-O2"));
        assert!(script.contains("/out/program"));
    }
}
