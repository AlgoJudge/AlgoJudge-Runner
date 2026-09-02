//! Configuration, in the shape the rest of the product already uses.
//!
//! Prefix `AJ_`, `__` between sections — the same convention the Server reads
//! (`builder.Configuration.AddEnvironmentVariables(prefix: "AJ_")`). One
//! convention across two components means an operator writing a Compose file
//! does not have to remember which service spells it which way.

use std::path::PathBuf;
use std::time::Duration;

pub struct Config {
    /// **Including `/api/v1`.** The Server serves nothing outside that prefix.
    pub base_url: String,
    pub name: String,
    pub key_path: PathBuf,
    pub problem_types: Vec<String>,

    /// Which pools this Runner belongs to, from `AJ_Runner__Tags`.
    ///
    /// **Read at the first registration and never again.** It exists so a room
    /// of machines can be deployed from one Compose file rather than tagged one
    /// at a time in the panel; changing it afterwards changes nothing, because
    /// the operator owns the value from then on.
    pub tags: Vec<String>,

    pub poll_min: Duration,
    pub poll_max: Duration,
    pub heartbeat: Duration,

    /// A request. The Server clamps it to `[60, 3600]` and answers with the
    /// deadline it actually granted.
    pub lease_seconds: u32,

    pub cache_path: PathBuf,
    pub cache_max_bytes: u64,

    /// Scratch for jobs, in both the views a bind mount needs.
    ///
    /// **`AJ_Work__HostPath` is not decoration.** A bind mount is resolved by
    /// the container runtime's daemon, so when the Runner is itself in a
    /// container the path it sees is not the path the daemon can open — and a
    /// path the daemon cannot open produces an **empty directory** rather than
    /// an error, which means tests silently run against nothing. Where the
    /// Runner is not containerised the two are the same and this can be left
    /// alone.
    pub work_path: PathBuf,
    pub work_host_path: PathBuf,

    pub images: aj_standard_io::Images,

    /// Starts anyway on a host this Runner cannot measure on.
    ///
    /// **Off by default, and loud when on. It does not make judging work — it
    /// makes starting work.** A time limit is decided on processor time read
    /// from the run's own cgroup, so a Runner that cannot read one fails every
    /// job it claims with an infrastructure error. What this buys is a process
    /// that registers, answers the protocol and can be talked to, which is what
    /// the conformance suite needs and all it needs.
    ///
    /// **Renamed from `AJ_Sandbox__AllowCgroupV1` on 2026-09-02**, which is
    /// still honoured: cgroup v1 was one of three conditions even then, and
    /// after the refusal became about the verdict rather than about a number
    /// beside it, the old name described none of them.
    pub allow_unmeasured: bool,
}

impl Config {
    pub fn from_environment() -> anyhow::Result<Self> {
        let base_url = var("Server__BaseUrl").ok_or_else(|| {
            anyhow::anyhow!(
                "AJ_Server__BaseUrl is required, and must include /api/v1 \
                 (for example http://server:8080/api/v1)"
            )
        })?;

        if !base_url.contains("/api/") {
            // Said plainly at start rather than discovered as a wall of 404s:
            // the Server's path guard answers with an empty body, so a base URL
            // missing the prefix fails with nothing to read.
            anyhow::bail!("AJ_Server__BaseUrl is {base_url:?}, which has no /api/v1 prefix");
        }

        let work = var("Work__Path").unwrap_or_else(|| "/var/lib/algojudge-runner/work".into());

        Ok(Self {
            base_url,
            name: var("Runner__Name").unwrap_or_else(default_name),
            key_path: var("Runner__KeyPath")
                .unwrap_or_else(|| "/var/lib/algojudge-runner/identity.key".into())
                .into(),
            problem_types: var("Runner__ProblemTypes")
                .unwrap_or_else(|| "standard-io@1".into())
                .split(',')
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty())
                .collect(),
            // Empty by default, which the Server reads as the general pool —
            // the same pool an untagged activity is in, so a Runner that names
            // nothing behaves exactly as every Runner did before tags existed.
            tags: var("Runner__Tags")
                .unwrap_or_default()
                .split(',')
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                .collect(),

            poll_min: Duration::from_secs(number("Poll__MinSeconds", 1)),
            poll_max: Duration::from_secs(number("Poll__MaxSeconds", 30)),
            heartbeat: Duration::from_secs(number("Heartbeat__Seconds", 60)),

            lease_seconds: number("Lease__RequestSeconds", 600) as u32,

            cache_path: var("Cache__Path")
                .unwrap_or_else(|| "/var/cache/algojudge-runner".into())
                .into(),
            cache_max_bytes: number("Cache__MaxBytes", 10 * 1024 * 1024 * 1024),

            work_path: work.clone().into(),
            work_host_path: var("Work__HostPath").unwrap_or(work).into(),

            // Either name. The old one is not deprecated so much as narrower
            // than what it always did, and a development `.env` that has it
            // should keep working.
            allow_unmeasured: var("Sandbox__AllowUnmeasured")
                .or_else(|| var("Sandbox__AllowCgroupV1"))
                .is_some(),

            // **Four images, eighteen toolchains.** Named one at a time, and
            // anything not named keeps the compiled-in default — so an
            // operator republishing the GCC image alone says so in one
            // variable instead of restating the other three.
            //
            // `Sandbox__Image__Cpp` was the old name for the GCC image and is
            // still read, because it is what every deployment and every
            // compose file in this repository sets today. It is the old name
            // for one of the four, not a fifth image.
            images: [
                // The old name first, so the new one wins when both are set.
                (aj_standard_io::language::GCC, "Sandbox__Image__Cpp"),
                (aj_standard_io::language::GCC, "Sandbox__Image__Gcc"),
                (aj_standard_io::language::CLANG, "Sandbox__Image__Clang"),
                (aj_standard_io::language::CPYTHON, "Sandbox__Image__Python"),
                (aj_standard_io::language::PYPY, "Sandbox__Image__Pypy"),
            ]
            .into_iter()
            .fold(
                aj_standard_io::Images::default(),
                |images, (key, name)| match var(name) {
                    Some(image) => images.with(key, image),
                    None => images,
                },
            ),
        })
    }

    /// The lease the Server will actually grant, which is not always the one
    /// asked for.
    ///
    /// **The clamp is the contract's, not a guess.** §5 of the accepted
    /// Server–Runner API states `[60, 3600]`, and `wire::ClaimedJob` repeats it
    /// where the answer is read. Anything counting locally on `lease_seconds`
    /// is counting on a deadline the Server never agreed to: configure two
    /// hours, be granted one, and a local bound of two hours outlives the lease
    /// it was meant to track. That was true of the report loop until 2026-08-16.
    ///
    /// **`leaseExpiresAt` in the Server's answer is the authoritative value, and
    /// it is deliberately not used here.** It is an absolute instant from the
    /// Server's clock; comparing it against this host's would make every local
    /// deadline a function of how well two machines agree about the time, and a
    /// Runner is explicitly allowed to be a machine nobody administers closely.
    /// A duration is immune to skew, an instant is not.
    ///
    /// Renewal needs neither: it runs on a timer and does no deadline
    /// arithmetic at all, so the Server's answer stays the only deadline in
    /// existence. This is for **local waiting**, which has to end somewhere.
    pub fn lease_granted(&self) -> Duration {
        Duration::from_secs(u64::from(
            self.lease_seconds.clamp(LEASE_FLOOR, LEASE_CEILING),
        ))
    }
}

/// The Server's clamp on `leaseSeconds`, from §5 of the contract.
const LEASE_FLOOR: u32 = 60;
const LEASE_CEILING: u32 = 3600;

fn var(key: &str) -> Option<String> {
    std::env::var(format!("AJ_{key}"))
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// A malformed number is a refusal, not a silent default.
///
/// Falling back would mean an operator who typed `AJ_Poll__MaxSeconds=3O` — a
/// letter O — gets thirty seconds and never learns why their setting did
/// nothing.
fn number(key: &str, fallback: u64) -> u64 {
    match var(key) {
        None => fallback,
        Some(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("AJ_{key} is {value:?}, which is not a number")),
    }
}

fn default_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "algojudge-runner".into())
}

#[cfg(test)]
mod tests {
    /// **`.env.example` claims to list every variable, and nothing checked it.**
    ///
    /// This repository had no such file at all until 2026-08-31, and one reason
    /// it did not is that `.gitignore` silently swallowed it — `/.env.*` matches
    /// `.env.example`, so `git add` said nothing and added nothing. A list that
    /// cannot be committed and a list nobody checks fail the same way: an
    /// operator cannot configure what they cannot see, and a variable missing
    /// from the file is not undocumented but *invisible*, because there is
    /// nowhere to look for it.
    ///
    /// Both halves are read as text. The drift is between two files and no
    /// compiler sees either of them as configuration, so nothing else can catch
    /// it. A commented-out line counts as listed — that is how a switch is
    /// offered without being set, which several of them must be.
    ///
    /// **Only `AJ_`-prefixed keys are checked.** `RUST_LOG`, `NO_COLOR`,
    /// `DOCKER_HOST` and the proxy four are read by libraries this Runner links
    /// rather than by its own source, so no file here mentions them and the
    /// comparison would reject every one. They are listed by hand.
    #[test]
    fn every_variable_the_config_reads_is_in_the_example_and_no_others() {
        // This file names its keys without the prefix, `AJ_` being added by the
        // helper. The sandbox reaches for the environment directly and writes
        // the whole name, so both shapes are collected.
        let mut read: std::collections::BTreeSet<String> = include_str!("config.rs")
            .split('"')
            .filter(|piece| a_key(piece))
            .map(|piece| format!("AJ_{piece}"))
            .collect();
        read.extend(
            include_str!("../../aj-sandbox/src/docker.rs")
                .split('"')
                .filter(|piece| piece.starts_with("AJ_") && a_key(&piece[3..]))
                .map(|piece| piece.to_owned()),
        );

        let listed: std::collections::BTreeSet<String> = include_str!("../../../.env.example")
            .lines()
            .map(|line| line.trim_start_matches('#').trim())
            .filter_map(|line| line.split_once('='))
            .map(|(name, _)| name.trim().to_owned())
            .filter(|name| name.starts_with("AJ_"))
            .collect();

        let missing: Vec<_> = read.difference(&listed).collect();
        assert!(
            missing.is_empty(),
            "read by this Runner and absent from .env.example: {missing:?}"
        );

        let stale: Vec<_> = listed.difference(&read).collect();
        assert!(
            stale.is_empty(),
            "listed in .env.example and read by nothing: {stale:?}"
        );
    }

    /// A configuration key exactly, and not a sentence that mentions one.
    fn a_key(piece: &str) -> bool {
        let Some((section, rest)) = piece.split_once("__") else {
            return false;
        };
        matches!(
            section,
            "Server" | "Runner" | "Cache" | "Lease" | "Poll" | "Heartbeat" | "Work" | "Sandbox"
        ) && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    }
}
