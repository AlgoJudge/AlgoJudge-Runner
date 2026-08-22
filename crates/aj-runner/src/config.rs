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

    /// Starts anyway on a host that reports cgroup v1.
    ///
    /// **Off by default, and loud when on.** The limits are enforced on v1, but
    /// peak memory and CPU time cannot be measured honestly there — and those
    /// numbers are shown to a participant beside their verdict. This exists
    /// because a developer machine is often Docker Desktop, which may still
    /// report v1, and the alternative is a development stack that cannot start.
    pub allow_cgroup_v1: bool,
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

            allow_cgroup_v1: var("Sandbox__AllowCgroupV1").is_some(),

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
