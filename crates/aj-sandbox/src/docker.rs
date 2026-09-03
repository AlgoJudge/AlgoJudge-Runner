//! Sibling containers.
//!
//! The Runner is trusted and holds the runtime's socket; **nothing it starts
//! ever does**. Privileged Docker-in-Docker and passing the socket into a
//! submission container are both rejected (D-5, 2026-08-06), for different
//! reasons: in the first, the privilege the infrastructure needs is the same
//! privilege anybody escaping the inner sandbox inherits — which is exactly how
//! the Judge0 CVE chain turned a file-write bug into host compromise. The second
//! hands untrusted code the host outright.
//!
//! **What this does not buy, stated plainly.** A compromised Runner is still
//! root-equivalent on the host, because anything that can call the runtime API
//! can start a privileged container. This arrangement is not safe; it is
//! *reducible* — the path runs through an API that can be proxied, scoped, moved
//! to rootless Podman or pushed onto a separate host. The boundary that holds is
//! the host itself, which is why it is treated as compromised by assumption.
//! Mounting the socket read-only restricts nothing that matters: the flag
//! applies to the socket file, not to the API spoken over it.

use std::collections::HashMap;
use std::time::Instant;

use bollard::container::LogOutput;
use bollard::models::SystemInfoCgroupVersionEnum;
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, DownloadFromContainerOptionsBuilder,
    InspectContainerOptions, KillContainerOptions, ListContainersOptionsBuilder,
    LogsOptionsBuilder, RemoveContainerOptionsBuilder, StartContainerOptions, WaitContainerOptions,
};
use futures_util::StreamExt as _;

use crate::cgroups::{self, Cgroups};
use crate::profile::{Outcome, Profile, Stopped};
use crate::{Error, Result, Sandbox};

pub struct Docker {
    client: bollard::Docker,
    /// Whose containers these are.
    ///
    /// **Several Runners share one host and one daemon**, and [`Self::sweep`]
    /// force-removes what it finds — so a label saying only "a sandbox" hands
    /// every Runner on the host the power to kill every other Runner's running
    /// evaluations, and the isolation and judging suites the same power over
    /// all of them.
    ///
    /// **Stable across a restart**, because that is the case the sweep exists
    /// for: a Runner's orphans have to carry the id it will have again. A pid
    /// cannot do it — the sweep is run by a *new* process clearing up after the
    /// old one. So the caller passes something that outlives the process; the
    /// Runner passes its key fingerprint, which is on disk and which the
    /// product already requires to be one Runner's alone.
    instance: String,
    /// How this Runner measures processor time and peak memory here.
    ///
    /// **There is no other way to take either.** Measured 2026-08-09: the
    /// runtime API does not report a peak on cgroup v2 — `memory_stats` carries
    /// `limit`, `usage` and the contents of `memory.stat`, none of which is a
    /// maximum — and **a container's own cgroup is destroyed the moment it
    /// exits**, so reading it afterwards finds nothing.
    ///
    /// So the sandbox is started *under* a cgroup that outlives it, and that
    /// cgroup is read once the child is gone. What it is depends on the
    /// daemon's cgroup driver; [`crate::cgroups`] is both arrangements and the
    /// reasons they differ.
    ///
    /// `None` means measurement is off, and **that must never stop a container
    /// from starting**: the limits are applied by the runtime and hold either
    /// way. Absent is then the honest answer rather than a guess.
    ///
    /// Resolved by [`Self::preflight`], because deciding it needs the daemon.
    cgroups: std::sync::OnceLock<Option<Cgroups>>,
}

impl Docker {
    /// `instance` says whose the containers started here are — see the field.
    pub fn connect(instance: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: bollard::Docker::connect_with_local_defaults()?,
            instance: instance.into(),
            cgroups: std::sync::OnceLock::new(),
        })
    }

    /// What [`Self::preflight`] decided, for a log and for a suite that has to
    /// know which host it is on before it can assert anything about a number.
    pub fn cgroups(&self) -> Option<&Cgroups> {
        self.cgroups.get()?.as_ref()
    }

    /// Two labels: one saying a container is ours at all, one saying **whose**.
    ///
    /// The first is the product-wide handle — `docker ps --filter
    /// label=algojudge.sandbox=1` finds every sandbox on a host, which is what
    /// an operator reaches for and what the CI cleanup step runs. The second is
    /// what lets [`Self::sweep`] leave another Runner's evaluation alone, and
    /// it is why an orphan from a previous incarnation can still be found: both
    /// carry the id its Runner has again after a restart.
    fn labels(&self) -> HashMap<String, String> {
        HashMap::from([
            ("algojudge.sandbox".to_owned(), "1".to_owned()),
            ("algojudge.instance".to_owned(), self.instance.clone()),
        ])
    }

    /// Pulls an image if it is not here yet.
    ///
    /// Language images are pinned by the caller. This exists so a first
    /// evaluation on a fresh host does not fail with "no such image", which
    /// would be reported as an infrastructure failure and be entirely correct
    /// and entirely unhelpful.
    pub async fn ensure_image(&self, image: &str) -> Result<()> {
        if self.client.inspect_image(image).await.is_ok() {
            return Ok(());
        }

        tracing::info!(image, "pulling");
        let mut pull = self.client.create_image(
            Some(
                CreateImageOptionsBuilder::default()
                    .from_image(image)
                    .build(),
            ),
            None,
            None,
        );
        while let Some(step) = pull.next().await {
            step?;
        }
        Ok(())
    }

    /// Removes the sandbox containers **this Runner** is responsible for, and
    /// says how many there were.
    ///
    /// **Run at start.** Job containers are siblings, so they outlive the
    /// process that made them — that is the one real cost of not using nested
    /// containers. Without this sweep a crash-loop fills the evaluation host
    /// with dead sandboxes until it runs out of disk.
    ///
    /// **It removes by force, so whose they are decides whether it is correct.**
    /// The filter was the constant `algojudge.sandbox=1` until 2026-08-31, while
    /// this comment already claimed the containers were this Runner's — which
    /// that label cannot express. A second Runner starting on the host killed
    /// the first one's running builds and tests; so did the isolation and
    /// judging suites, which call this for a clean slate on every case and then
    /// assert the count is zero, failing over somebody else's live container as
    /// though the sandbox had leaked a process.
    ///
    /// A container carrying **no** instance label is taken as ours. It can only
    /// have come from a Runner older than that change, so it belongs to nobody
    /// now, and leaving it is leaving the disk to fill. Docker's filters cannot
    /// ask for an absent label, so the listing is by the sandbox label and the
    /// decision is made here.
    pub async fn sweep(&self) -> Result<usize> {
        let listed = self
            .client
            .list_containers(Some(
                ListContainersOptionsBuilder::default()
                    .all(true)
                    .filters(&HashMap::from([("label", vec!["algojudge.sandbox=1"])]))
                    .build(),
            ))
            .await?;

        let mut swept = 0;
        for container in listed {
            let whose = container
                .labels
                .as_ref()
                .and_then(|labels| labels.get("algojudge.instance"));
            if whose.is_some_and(|whose| whose != &self.instance) {
                continue;
            }
            if let Some(id) = container.id {
                self.remove(&id).await;
                swept += 1;
            }
        }

        if swept > 0 {
            tracing::warn!(swept, "sandbox containers from a previous run were removed");
        }
        Ok(swept)
    }

    fn host_config(&self, profile: &Profile, cgroup_parent: Option<&str>) -> HostConfig {
        let memory = profile.memory_bytes as i64;

        HostConfig {
            // Under the Runner's own cgroup when there is one, so that a peak
            // survives the container that produced it. Absent leaves the daemon
            // to place the container wherever it normally would.
            cgroup_parent: cgroup_parent.map(str::to_owned),

            // No route anywhere. The first line of the adversarial suite.
            network_mode: Some("none".to_owned()),
            cap_drop: Some(vec!["ALL".to_owned()]),
            security_opt: Some(vec!["no-new-privileges".to_owned()]),
            readonly_rootfs: Some(!profile.writable_root),

            memory: Some(memory),
            // Equal to `memory`, not absent. Without this the limit means
            // nothing: the process swaps instead of being killed, and a memory
            // limit that can be evaded is not a limit.
            memory_swap: Some(memory),
            pids_limit: Some(profile.pids),
            nano_cpus: Some((profile.cpus * 1_000_000_000.0) as i64),
            cpuset_cpus: profile.cpuset.clone(),

            // Soft and hard set to the same number: a program that raises its
            // own soft limit to the hard one has raised nothing.
            ulimits: Some(vec![
                bollard::models::ResourcesUlimits {
                    name: Some("nofile".to_owned()),
                    soft: Some(profile.max_open_files),
                    hard: Some(profile.max_open_files),
                },
                bollard::models::ResourcesUlimits {
                    name: Some("fsize".to_owned()),
                    soft: Some(profile.max_file_bytes),
                    hard: Some(profile.max_file_bytes),
                },
            ]),

            binds: Some(
                profile
                    .mounts
                    .iter()
                    .map(|m| {
                        format!(
                            "{}:{}:{}",
                            m.from.display(),
                            m.to,
                            if m.writable { "rw" } else { "ro" }
                        )
                    })
                    .collect(),
            ),

            tmpfs: profile.tmpfs_bytes.map(|bytes| {
                HashMap::from([(
                    "/tmp".to_owned(),
                    // Writable and **not executable**: a directory a submission
                    // can write to and then execute from is the shortest route
                    // from "produced output" to "ran something we did not
                    // compile".
                    format!("rw,noexec,nosuid,size={bytes}"),
                )])
            }),

            ..Default::default()
        }
    }
}

#[async_trait::async_trait]
impl Sandbox for Docker {
    fn name(&self) -> &'static str {
        "docker"
    }

    /// Reachable runtime, and a kernel that can enforce what is asked of it.
    ///
    /// Checked at start and failing loudly, because a sandbox that silently does
    /// not enforce a limit does not produce errors — it produces wrong verdicts,
    /// which look like somebody's solution being wrong.
    async fn preflight(&self) -> Result<()> {
        let version = self.client.version().await?;
        tracing::info!(
            engine = version.version.as_deref().unwrap_or("?"),
            kernel = version.kernel_version.as_deref().unwrap_or("?"),
            "container runtime reached",
        );

        // cgroup v2 is a hard requirement, and the runtime is the honest place
        // to ask: reading `/sys/fs/cgroup` from inside the Runner's own
        // container answers a different question.
        //
        // **What v1 does and does not do**, measured rather than assumed
        // (2026-08-09, Docker Desktop 24.0.7 on a WSL2 kernel): it *enforces*
        // the limits. A container over its memory limit is OOM-killed and
        // `OOMKilled` is reported, and the whole adversarial suite passes. What
        // it does not give is honest **measurement** — `memory.peak` and
        // `cpu.stat` are v2 interfaces, and `isolate` 2.x dropped v1 outright.
        // So the refusal below is about the number shown beside a verdict, not
        // about whether the sandbox holds.
        let info = self.client.info().await?;

        // **Version first, and the order is not arbitrary.** On a v1 host the
        // root is a tmpfs of controller directories, so `create_dir_all` there
        // frequently *succeeds* — a writability check run first would pass and
        // the refusal below would then have to explain a second-order symptom
        // rather than the cause.
        if info.cgroup_version != Some(SystemInfoCgroupVersionEnum::_2) {
            return Err(cgroups::unsupported_version(&format!(
                "{:?}",
                info.cgroup_version
            )));
        }

        // **Refused rather than degraded, since 2026-09-02.** This used to give
        // up with one `info` line and judge anyway, which was defensible while
        // the reading was only reported beside a verdict. It is what the verdict
        // is now made of, so a Runner that cannot take it cannot judge, and the
        // honest thing is to say so at start rather than to fail every job it
        // later claims.
        //
        // **The driver chooses a backend rather than deciding this**, since
        // 2026-09-03. Requiring `cgroupfs` meant requiring every systemd host to
        // reconfigure its daemon before it could judge anything.
        //
        // Cached because it needs the daemon's answer and cannot change while
        // the daemon is the same one.
        let driver = info
            .cgroup_driver
            .map(|d| d.to_string())
            .unwrap_or_default();
        let decided = cgroups::root_from_environment()
            .and_then(|root| Cgroups::resolve(&driver, root, &self.instance))
            .and_then(|chosen| chosen.prepare().map(|()| chosen));

        if let Ok(chosen) = &decided {
            tracing::info!(
                driver = chosen.driver(),
                home = %chosen.home().display(),
                "processor time and peak memory are read from here",
            );
            // Not a refusal: a verdict is made of processor time, which is
            // unaffected. Said once, at start, because the alternative is a
            // participant wondering why one installation prints a number and
            // another does not.
            if let Some(why) = chosen.without_peak_memory() {
                tracing::warn!("peak memory will not be reported: {why}");
            }
        }
        let _ = self.cgroups.set(decided.as_ref().ok().cloned());
        decided.map(|_| ())
    }

    async fn run(&self, profile: &Profile) -> Result<Outcome> {
        let name = format!(
            "algojudge-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos() as u64 ^ rand_suffix(),
        );

        // Opened before the container, because the container is started *into*
        // it and because under the systemd backend the reading is a difference
        // that has to have a beginning. Failure here is not an error: the run
        // proceeds unmeasured.
        let cgroup = match self.cgroups() {
            Some(cgroups) => cgroups.begin(&name).await,
            None => None,
        };

        let config = ContainerCreateBody {
            image: Some(profile.image.clone()),
            cmd: Some(profile.command.clone()),
            working_dir: Some(profile.working_directory.clone()),
            // Nobody. The image's own user is not to be trusted to be
            // unprivileged.
            user: Some("65534:65534".to_owned()),
            labels: Some(self.labels()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            network_disabled: Some(true),
            host_config: Some(
                self.host_config(profile, cgroup.as_ref().map(|(_, parent)| parent.as_str())),
            ),
            ..Default::default()
        };

        if let Err(e) = self
            .client
            .create_container(
                Some(CreateContainerOptionsBuilder::default().name(&name).build()),
                config,
            )
            .await
        {
            // No container was made, so nothing will remove the cgroup that was
            // opened for it — and under systemd nothing would release the gate.
            if let Some((measuring, _)) = cgroup {
                measuring.finish();
            }
            return Err(e.into());
        }

        // From here on the container exists, so every path out removes it.
        let mut outcome = self.supervise(&name, profile).await;
        self.remove(&name).await;

        // Read after the container is gone: the child's own cgroup goes with it,
        // and the numbers are final the moment the child stops.
        if let Some((measuring, _)) = cgroup {
            let (peak, cpu) = measuring.finish();
            if let Ok(outcome) = outcome.as_mut() {
                outcome.peak_memory_bytes = peak;
                outcome.cpu_time = cpu;
            }
        }
        outcome
    }
}

impl Docker {
    async fn supervise(&self, name: &str, profile: &Profile) -> Result<Outcome> {
        let started = Instant::now();
        self.client
            .start_container(name, None::<StartContainerOptions>)
            .await?;

        // Read while it runs, not afterwards. A program flooding its output
        // would otherwise fill the host's disk with a log we then discard.
        //
        // The collector kills the container itself when the cap is passed and
        // raises a flag. It used to signal through a `oneshot`, and that was a
        // **race**: a oneshot resolves when its sender is *dropped* as well as
        // when it sends, and the sender is dropped every time the collector
        // finishes normally. Whichever of "the container exited" and "the log
        // stream ended" won the `select!` decided the verdict — so a fast,
        // silent program was sometimes reported as having flooded its output,
        // which on a real submission is a correct solution marked wrong.
        //
        // The flag is read below, **after the collector has finished**. After
        // the wait alone is not enough, and that was the other half of the same
        // race: a program that floods and then exits on its own leaves the
        // collector still draining the runtime's buffered stream, so the flag
        // is read before it is raised — and the truncated log is then scored as
        // the participant's wrong answer instead of an output-limit verdict.
        let flooded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let collector = tokio::spawn(collect(
            self.client.clone(),
            name.to_owned(),
            profile.max_output_bytes,
            std::sync::Arc::clone(&flooded),
        ));

        let mut waiter = self
            .client
            .wait_container(name, None::<WaitContainerOptions>);

        let stopped = tokio::select! {
            _ = waiter.next() => Stopped::OnItsOwn,
            _ = tokio::time::sleep(profile.wall_clock) => {
                tracing::debug!(container = name, "killed at the wall clock");
                self.kill(name).await;
                Stopped::WallClock
            }
        };

        let wall_time = started.elapsed();

        // Read back before the container is removed, and **before** the exit
        // code is even looked at: a build that failed still has a log worth
        // having, and the container is gone a moment later either way.
        let collected = match &profile.collect {
            None => None,
            Some(path) => self.take(name, path, profile.max_collected_bytes).await?,
        };
        let (stdout, stderr) = collector
            .await
            .map_err(|e| Error::Refused(format!("the output collector did not finish: {e}")))??;

        // Now that the collector is done, and not before: see above.
        let stopped = if flooded.load(std::sync::atomic::Ordering::SeqCst) {
            Stopped::Output
        } else {
            stopped
        };

        // The kernel's own answer, and the only reliable one: a container over
        // its memory limit is OOM-killed, and asking afterwards is how the
        // difference between "ran out of memory" and "exited non-zero" is told.
        let details = self
            .client
            .inspect_container(name, None::<InspectContainerOptions>)
            .await?;
        let state = details.state.unwrap_or_default();
        let exit_code = state.exit_code.unwrap_or(-1);
        let stopped = if state.oom_killed.unwrap_or(false) {
            Stopped::Memory
        } else {
            stopped
        };

        Ok(Outcome {
            exit_code,
            stdout,
            stderr,
            wall_time,
            stopped,
            // Filled by `run` from the cgroup, where there is one. Absent is
            // the honest answer on a host that gave the Runner nowhere to
            // measure from.
            peak_memory_bytes: None,
            cpu_time: None,
            collected,
        })
    }

    /// Copies a path out of a stopped container, as a tar archive.
    ///
    /// `None` rather than an error when it is not there: a build that produced
    /// nothing is a build that failed, and the caller already knows that from
    /// the exit code. Turning "no artefact" into an infrastructure failure
    /// would report a participant's unbuildable submission as the system being
    /// broken.
    /// **Bounded, and accumulated once.** It used to `try_collect()` every chunk
    /// into a `Vec<Bytes>` and then `concat()` them, so the whole artefact
    /// existed twice at the moment of joining, with nothing capping either copy
    /// — in the trusted process, on bytes a participant's compilation produced.
    ///
    /// Refused rather than truncated, for the reason the output collector gives
    /// above: a silently shortened artefact is a program that will not run, and
    /// nothing downstream could tell that from a build that never worked.
    async fn take(&self, name: &str, path: &str, cap: u64) -> Result<Option<Vec<u8>>> {
        let options = DownloadFromContainerOptionsBuilder::new()
            .path(path)
            .build();
        let mut stream = self.client.download_from_container(name, Some(options));

        let mut collected: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                // See the doc comment: not being there is the ordinary way a
                // failed build looks, and the caller reads the exit code.
                Err(e) => {
                    tracing::debug!(container = name, path, %e, "nothing to collect");
                    return Ok(None);
                }
            };
            if collected.len() as u64 + chunk.len() as u64 > cap {
                return Err(Error::Refused(format!(
                    "what the build produced is larger than {cap} bytes"
                )));
            }
            collected.extend_from_slice(&chunk);
        }
        Ok(Some(collected))
    }

    /// Best effort, and deliberately not an error.
    ///
    /// A container that has already exited cannot be killed, and a failure here
    /// must not turn a finished evaluation into an infrastructure failure.
    async fn kill(&self, name: &str) {
        if let Err(e) = self
            .client
            .kill_container(name, None::<KillContainerOptions>)
            .await
        {
            tracing::debug!(container = name, %e, "kill did not apply");
        }
    }

    async fn remove(&self, name: &str) {
        let options = RemoveContainerOptionsBuilder::default()
            .force(true)
            .v(true)
            .build();
        if let Err(e) = self.client.remove_container(name, Some(options)).await {
            // Worth a warning rather than a shrug: an evaluation host that
            // accumulates these runs out of disk on the busiest day of the year.
            tracing::warn!(container = name, %e, "a sandbox container was left behind");
        }
    }
}

/// Accumulates output up to the cap, then says so and stops reading.
///
/// Refuses rather than truncates silently: the caller turns the overflow into a
/// verdict a participant can read, and a quietly shortened output would instead
/// look like a wrong answer.
async fn collect(
    client: bollard::Docker,
    name: String,
    cap: u64,
    flooded: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut total = 0u64;
    let mut said = false;

    let mut logs = client.logs(
        &name,
        Some(
            LogsOptionsBuilder::default()
                .follow(true)
                .stdout(true)
                .stderr(true)
                .build(),
        ),
    );

    while let Some(chunk) = logs.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            // The stream ends abruptly when the container is removed under it,
            // which is the ordinary way this finishes.
            Err(e) => {
                tracing::trace!(%e, "the log stream ended");
                break;
            }
        };

        let (sink, bytes) = match chunk {
            LogOutput::StdOut { message } => (&mut stdout, message),
            LogOutput::StdErr { message } => (&mut stderr, message),
            LogOutput::Console { message } | LogOutput::StdIn { message } => (&mut stdout, message),
        };

        total += bytes.len() as u64;
        if total > cap {
            if !said {
                said = true;
                flooded.store(true, std::sync::atomic::Ordering::SeqCst);
                // Stopped here rather than by the supervisor, so there is no
                // second party to race with.
                if let Err(e) = client
                    .kill_container(&name, None::<KillContainerOptions>)
                    .await
                {
                    tracing::debug!(container = %name, %e, "kill for output did not apply");
                }
            }
            // Keep exactly the cap, so what is shown is a prefix of what was
            // produced rather than an arbitrary cut.
            let room = cap.saturating_sub(sink.len() as u64) as usize;
            sink.extend_from_slice(&bytes[..room.min(bytes.len())]);
            continue;
        }

        sink.extend_from_slice(&bytes);
    }

    Ok((stdout, stderr))
}

/// Enough to keep two containers started in the same nanosecond apart.
fn rand_suffix() -> u64 {
    use std::hash::{BuildHasher as _, RandomState};
    RandomState::new().hash_one(std::time::SystemTime::now())
}
