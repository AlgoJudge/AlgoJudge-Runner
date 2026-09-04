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
use std::time::{Duration, Instant};

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
    /// Whether each image carries the measuring shim, asked once.
    ///
    /// **It has to be known before the container is made**, because it decides
    /// who the container starts as, and that is fixed at creation. An image with
    /// no shim runs unprivileged and measures from the cgroup alone; one that
    /// has it starts as root so the shim can drop.
    shims: tokio::sync::Mutex<HashMap<String, bool>>,
}

impl Docker {
    /// `instance` says whose the containers started here are — see the field.
    pub fn connect(instance: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: bollard::Docker::connect_with_local_defaults()?,
            instance: instance.into(),
            cgroups: std::sync::OnceLock::new(),
            shims: tokio::sync::Mutex::new(HashMap::new()),
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
    /// Whether an image carries the shim, asked once per image and remembered.
    ///
    /// **Read out of the image rather than run.** A container is created and
    /// never started, and the path is fetched through the archive endpoint --
    /// the same one a build's artefacts come back through. Starting one to run
    /// `test -x` would cost a container start per image and would be the only
    /// place the Runner executes something in an image before it has decided how
    /// to confine it.
    ///
    /// Any failure answers `false`, which is the safe direction: the run falls
    /// back to the shell, the container stays unprivileged, and the measurement
    /// is the cgroup's alone.
    async fn image_has_shim(&self, image: &str) -> bool {
        if let Some(known) = self.shims.lock().await.get(image) {
            return *known;
        }

        let name = format!("algojudge-{}-shimprobe", self.instance);
        self.take_nothing(&name).await;
        let created = self
            .client
            .create_container(
                Some(CreateContainerOptionsBuilder::default().name(&name).build()),
                ContainerCreateBody {
                    image: Some(image.to_owned()),
                    labels: Some(self.labels()),
                    ..Default::default()
                },
            )
            .await;

        let found = match created {
            Err(e) => {
                tracing::debug!(image, %e, "could not probe for the shim");
                false
            }
            Ok(_) => {
                let read = self.take(&name, crate::SHIM, 8 * 1024 * 1024).await;
                matches!(read, Ok(Some(_)))
            }
        };
        self.remove(&name).await;

        tracing::info!(image, shim = found, "measured runs in this image");
        self.shims.lock().await.insert(image.to_owned(), found);
        found
    }

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

        // **After the containers, and not before.** A run's cgroup has the
        // container's own as a child while the container exists, and a cgroup
        // with a child cannot be removed.
        let abandoned = self
            .cgroups()
            .map_or(0, |cgroups| cgroups.abandoned(&self.instance));
        if abandoned > 0 {
            tracing::warn!(
                abandoned,
                "cgroups of runs that were cut short were removed"
            );
        }

        Ok(swept)
    }

    fn host_config(
        &self,
        profile: &Profile,
        cgroup_parent: Option<&str>,
        shim: bool,
    ) -> HostConfig {
        let memory = profile.memory_bytes as i64;

        HostConfig {
            // Under the Runner's own cgroup when there is one, so that a peak
            // survives the container that produced it. Absent leaves the daemon
            // to place the container wherever it normally would.
            cgroup_parent: cgroup_parent.map(str::to_owned),

            // No route anywhere. The first line of the adversarial suite.
            network_mode: Some("none".to_owned()),
            cap_drop: Some(vec!["ALL".to_owned()]),
            // **Two, and only where a shim will use them to give privilege up.**
            // Dropping to another user is the one thing `cap_drop: ALL` takes
            // away that the shim needs; `no-new-privileges` below still forbids
            // gaining any, and the submission itself never holds either.
            cap_add: shim.then(|| vec!["SETUID".to_owned(), "SETGID".to_owned()]),
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
            .and_then(|chosen| chosen.prepare(&self.instance).map(|()| chosen));

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
        // Asked before anything is created: the user a container starts as
        // cannot be changed afterwards, and an image with no shim must not be
        // handed a root one.
        let shim = profile.measured && self.image_has_shim(&profile.image).await;
        let nonce = shim.then(|| format!("{:016x}{:016x}", rand_suffix(), rand_suffix()));

        let name = format!(
            "{}{:016x}",
            cgroups::run_prefix(&self.instance),
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
            // Nobody, unless the shim is there to put the submission back to
            // nobody itself. The image's own user is not to be trusted to be
            // unprivileged either way, so it is always stated here.
            user: Some(container_user(profile.measured, shim).to_owned()),
            // The nonce reaches the shim and nothing else: it scrubs the bytes
            // before forking, and the submission runs as a different user that
            // cannot read `/proc/1/environ` in any case.
            env: nonce
                .as_ref()
                .map(|nonce| vec![format!("AJ_SHIM_NONCE={nonce}")]),
            labels: Some(self.labels()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            network_disabled: Some(true),
            host_config: Some(self.host_config(
                profile,
                cgroup.as_ref().map(|(_, parent)| parent.as_str()),
                shim,
            )),
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
        let mut outcome = self
            .supervise(
                &name,
                profile,
                cgroup.as_ref().map(|(measuring, _)| measuring),
            )
            .await;
        self.remove(&name).await;

        // Read after the container is gone: the child's own cgroup goes with it,
        // and the numbers are final the moment the child stops.
        if let Some((measuring, _)) = cgroup {
            let reading = measuring.finish();
            if let Ok(outcome) = outcome.as_mut() {
                outcome.peak_memory_bytes = reading.peak_memory_bytes;
                outcome.cpu_time = reading.cpu_time;
                // **The verdict for memory stays the cgroup's.** A limit is
                // enforced by the kernel, so what the report claims about memory
                // changes nothing about whether the kernel killed it.
                outcome.stopped = memory_kill(outcome.stopped, &reading);
            }
        }

        if let (Some(nonce), Ok(outcome)) = (nonce.as_deref(), outcome.as_mut()) {
            if let Some(said) = take_report(&mut outcome.stderr, nonce) {
                let whole = outcome.cpu_time;
                let charged = measured_time(said.cpu, whole);

                // **Both instruments, side by side, and nothing else records
                // them.** What a run is charged is a `max` of the two, so a run
                // charged more than it spent was charged by exactly one of them
                // and the gap says which. The report is the program alone; the
                // reading is the program plus the container it started in; and
                // [`UNEXPLAINED_GAP`] is what a container may honestly cost
                // container, measured on a host with nothing else to do. On a
                // loaded one that constant is the likeliest thing to be wrong,
                // and this line is how anybody would find out.
                tracing::debug!(
                    container = name,
                    reported_us = said.cpu.as_micros() as u64,
                    cgroup_us = whole.map(|whole| whole.as_micros() as u64),
                    unexplained_gap_us = UNEXPLAINED_GAP.as_micros() as u64,
                    charged_us = charged.as_micros() as u64,
                    floored = charged > said.cpu,
                    // **What the program did, against what was done for it.** A
                    // total that grew under load says nothing about which of the
                    // two grew, and they have different causes and different
                    // cures: one is the program, the other is the host's memory
                    // and its filesystem.
                    user_us = said.user.as_micros() as u64,
                    system_us = said.system.as_micros() as u64,
                    "reconciled the program's own time with the container's",
                );

                if charged != said.cpu {
                    // **Loud, because it is one of two things and both matter.**
                    // Either this host's containers cost more than any measured
                    // -- and correct programs are now being charged for them --
                    // or a report was forged, which takes getting past the
                    // nonce. Neither is a thing to find out from a verdict.
                    tracing::warn!(
                        container = name,
                        reported_us = said.cpu.as_micros() as u64,
                        cgroup_us = whole.map(|whole| whole.as_micros() as u64),
                        charged_us = charged.as_micros() as u64,
                        "the container's reading is too far above the program's own time to believe it",
                    );
                }
                outcome.cpu_time = Some(charged);
                // Memory needs no floor: understating it buys nothing, because
                // the kernel and not this number decides the memory verdict. The
                // shim's figure is the better one -- the program's own resident
                // peak, where the cgroup also carries the page cache of whatever
                // the container read.
                outcome.peak_memory_bytes = Some(said.peak_memory_bytes);
            }
        }
        outcome
    }
}

impl Docker {
    /// Waits for a run to stop being worth waiting for, and stops it.
    ///
    /// **A program spending processor time is never reaped**, however long it
    /// takes in wall clock. That is the whole point: on a busy host a program
    /// that is computing may be descheduled for most of its wall clock, and a
    /// deadline that cannot tell that from a program doing nothing turns
    /// contention into `Time limit exceeded` on work that was inside its limit.
    async fn reap(
        &self,
        name: &str,
        profile: &Profile,
        measuring: Option<&crate::cgroups::Measuring>,
    ) -> Stopped {
        let started = Instant::now();
        let mut reaper = Reaper::new(profile);
        loop {
            tokio::time::sleep(REAP_POLL).await;
            let cpu = measuring.and_then(|measuring| measuring.so_far());
            // Read together, from the same cgroup, in the same look: the
            // decision is about what happened *between* two looks, and two
            // readings taken at different moments would not describe one
            // interval.
            let stalled = measuring.and_then(|measuring| measuring.stalled());
            if let Some(stopped) = reaper.tick(REAP_POLL, started.elapsed(), cpu, stalled) {
                tracing::debug!(
                    container = name,
                    ?stopped,
                    cpu_us = cpu.map(|cpu| cpu.as_micros() as u64),
                    // **Beside the reason, because it is half of it.** A
                    // `WallClock` with pressure that never moved is a program
                    // that was not trying to run; one with pressure climbing
                    // would be a starved program this rule failed to protect,
                    // and that is the shape to look for if this ever goes wrong.
                    stalled_us = stalled.map(|stalled| stalled.as_micros() as u64),
                    waited_ms = started.elapsed().as_millis() as u64,
                    "stopped waiting for a run",
                );
                // **The kill belongs to the caller, not here.** `select!` polls
                // every branch until one *completes*; awaiting inside this one
                // leaves the other still able to win, and the container exiting
                // under our own kill is exactly what makes it win. The run then
                // reads as having finished on its own.
                return stopped;
            }
        }
    }

    async fn supervise(
        &self,
        name: &str,
        profile: &Profile,
        measuring: Option<&crate::cgroups::Measuring>,
    ) -> Result<Outcome> {
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
            // Once this completes the branch is chosen and its block runs to the
            // end, so the kill cannot hand the decision back to the waiter.
            stopped = self.reap(name, profile, measuring) => {
                self.kill(name).await;
                stopped
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

        // How the difference between "ran out of memory" and "exited non-zero"
        // is told. **The runtime's answer, and it is not the only one**: this
        // said "the only reliable one" until 2026-09-03, when CI caught it
        // reporting `false` for a container that exited 137 after 117 ms. The
        // kernel's own counter is consulted in `run`, from the cgroup, and
        // either of them saying so is enough.
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
        if let Err(e) = self.take_away(name).await {
            // Worth a warning rather than a shrug: an evaluation host that
            // accumulates these runs out of disk on the busiest day of the year.
            tracing::warn!(container = name, %e, "a sandbox container was left behind");
        }
    }

    /// The same removal where **there is usually nothing to remove**, and the
    /// absence is the ordinary case rather than a leak.
    ///
    /// Only the shim probe, which clears a name it is about to reuse in case a
    /// killed Runner left one behind. Going through [`Self::remove`] there
    /// reported `404: no such container` as a container left behind, on every
    /// probe of every image — an operator being taught that this warning means
    /// nothing, which is the whole of what it costs.
    async fn take_nothing(&self, name: &str) {
        if let Err(e) = self.take_away(name).await {
            tracing::debug!(container = name, %e, "nothing to clear before the probe");
        }
    }

    async fn take_away(&self, name: &str) -> std::result::Result<(), bollard::errors::Error> {
        let options = RemoveContainerOptionsBuilder::default()
            .force(true)
            .v(true)
            .build();
        self.client.remove_container(name, Some(options)).await
    }
}

/// Accumulates output up to the cap, then says so and stops reading.
///
/// Refuses rather than truncates silently: the caller turns the overflow into a
/// verdict a participant can read, and a quietly shortened output would instead
/// look like a wrong answer.
/// What the run was stopped by, once the kernel has been asked as well.
///
/// **Either answer is enough, and neither is checked against the other.** The
/// runtime reports `OOMKilled` on the container; the kernel counts kills in
/// `memory.events`. On 2026-09-03 CI caught the first reporting `false` for a
/// container that exited 137 after 117 ms on a systemd-driver host — a memory
/// limit told to a participant as a runtime error, which is a wrong verdict
/// rather than a missing number. Once in 25 local attempts it did not recur, so
/// this is a rare race and not a broken flag; a second opinion is the cheap
/// answer to a rare one.
///
/// **The kernel's half is two counters and needs both**, because each alone says
/// the wrong thing. `oom_kill` counts kills *by any kind of OOM killer*, so a
/// host out of memory would be reported as a submission over its limit; `oom`
/// counts this cgroup reaching **its own** limit, and moves without anything
/// dying. Together they are a program killed for exceeding the limit it was
/// given.
///
/// **Memory outranks everything**, which is what the runtime's flag already did
/// here: a program stopped at the reaping deadline or over the output cap
/// *while also* being OOM-killed ran out of memory, and that is the useful
/// thing to tell somebody.
fn memory_kill(stopped: Stopped, reading: &cgroups::Reading) -> Stopped {
    if reading.oom_kills > 0 && reading.over_limit > 0 {
        Stopped::Memory
    } else {
        stopped
    }
}

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
/// Who a container starts as.
///
/// **Root only where a shim will hand the privilege straight back.** Every other
/// container -- a build, a checker, and a measured run in an image with no shim
/// -- starts as nobody, which is what they have always done. A step that gained
/// root because someone marked the wrong profile would be a far worse bargain
/// than a coarser number, so the two conditions are named here together rather
/// than spread across the call site.
fn container_user(measured: bool, shim: bool) -> &'static str {
    if measured && shim {
        "0:0"
    } else {
        "65534:65534"
    }
}

/// A run's processor time, from the precise instrument and the coarse one.
///
/// **The report is charged, and the reading is used to disbelieve it.** The two
/// measure different things: the report is the program alone, the reading is the
/// program plus the container it started in. Their difference is therefore the
/// container's own cost, and a rule that charges the larger of the two charges
/// the participant for that difference whenever it exceeds whatever constant
/// stands in for it.
///
/// **Measured, and that is why this is the way round it is.** Across 7077 runs
/// under load, 2026-09-04, the difference ran to a median of 77 ms and a
/// maximum of 619 ms -- so a 120 ms allowance charged 5% of honest runs for work
/// they did not do, by up to half a second, and six correct submissions in a
/// hundred and fifty were failed for it. Above [`UNEXPLAINED_GAP`] the reading
/// is no longer explicable as a container start and the report is not believed;
/// below it, the precise instrument wins outright.
fn measured_time(reported: Duration, whole: Option<Duration>) -> Duration {
    match whole {
        None => reported,
        Some(whole) if whole.saturating_sub(reported) <= UNEXPLAINED_GAP => reported,
        // Not the report and not the reading: the reading less everything a
        // container could honestly have cost, which is the least this run can
        // be shown to have spent.
        Some(whole) => whole.saturating_sub(UNEXPLAINED_GAP),
    }
}

/// How far the container's reading may stand above the program's own report
/// before the report stops being believable.
///
/// **What it costs and what it buys, both stated.** A submission that could
/// forge a report may understate by up to this much and be believed. What that
/// takes is the nonce, which is scrubbed from the environment before the fork
/// and sits in a process owned by root while the submission runs as nobody --
/// so a forgery is downstream of a privilege escalation inside the container,
/// and this bounds the damage rather than being the defence.
///
/// Against that: **every value below the measured maximum fails honest runs.**
/// The largest difference across 7077 runs under load was 619 ms, so anything
/// tighter than that charges correct programs for their container. A second is
/// above it with room for a host slower than the one measured, and still
/// catches the cheating worth doing -- a program spending seconds past its
/// limit, which is what a limit of 100 to 600 ms makes worth attempting.
const UNEXPLAINED_GAP: Duration = Duration::from_secs(1);

/// How often a run's processor time is looked at while it runs.
///
/// One small file read per run per tick — a dozen Runners make a few dozen
/// reads a second — and fine enough that progress is seen long before any
/// deadline below could pass.
const REAP_POLL: Duration = Duration::from_millis(250);

/// How far past its limit a program may get before there is no point waiting.
///
/// **Generous on purpose, because being early here is the expensive mistake.**
/// The verdict is decided afterwards on the precise measurement; the reading
/// this is compared against is the cgroup's, which carries the container's own
/// start as well as the program. Twice the limit plus the shim's allowance is
/// far outside anything a program inside its budget produces, and still stops a
/// runaway in a fraction of the wall clock it used to take.
fn cpu_ceiling(limit: Duration) -> Duration {
    limit * 2 + UNEXPLAINED_GAP
}

/// The longest a run may take however little processor time it spends.
///
/// Nothing else bounds a program that wakes for a millisecond every quarter of
/// a second: it never stalls and never approaches its limit. Ten times the
/// no-progress window is far outside anything contention produces, and still
/// bounds what one submission can take from the queue.
fn absolute_cap(window: Duration) -> Duration {
    window * 10
}

/// When a run stops being worth waiting for.
///
/// **Progress, not elapsed time.** Every field is a duration rather than an
/// instant so the decision is a function of its arguments: the deadline this
/// implements is the one thing in the sandbox that must be provably right about
/// a program that is running slowly rather than not running.
struct Reaper {
    /// How long without the processor time growing before giving up.
    window: Duration,
    /// Where "plainly past its budget" starts, when the step has a budget.
    ceiling: Option<Duration>,
    /// The end of it, whatever the program is doing.
    cap: Duration,
    best: Duration,
    idle: Duration,
    /// The last pressure reading, to take a difference against. `None` until
    /// the first look, and on a kernel that carries no PSI.
    stalled: Option<Duration>,
}

impl Reaper {
    fn new(profile: &Profile) -> Self {
        Self {
            window: profile.wall_clock,
            ceiling: profile.cpu_limit.map(cpu_ceiling),
            cap: absolute_cap(profile.wall_clock),
            best: Duration::ZERO,
            idle: Duration::ZERO,
            stalled: None,
        }
    }

    /// Whether something was **runnable and waiting for a processor** since the
    /// last look, and remembers this look for the next one.
    ///
    /// Always called, whatever the processor time did, so the baseline cannot
    /// go stale across a burst of progress.
    ///
    /// `false` without a reading, which is the answer that changes nothing: a
    /// kernel with no PSI reaps exactly as it did before this existed.
    fn waited_for_a_processor(&mut self, now: Option<Duration>) -> bool {
        let Some(now) = now else { return false };
        let grew = self.stalled.is_some_and(|before| now > before);
        self.stalled = Some(now);
        grew
    }

    /// One look. `None` means keep waiting.
    fn tick(
        &mut self,
        since: Duration,
        elapsed: Duration,
        cpu: Option<Duration>,
        stalled: Option<Duration>,
    ) -> Option<Stopped> {
        let waiting = self.waited_for_a_processor(stalled);

        match cpu {
            // **Progress only counts against a limit.** A step nobody is timed
            // on — a build, a checker — has a cgroup like any other, so there is
            // a reading; what there is not is anything for it to mean. Letting
            // it hold the deadline open would give a build ten times the minute
            // it is allowed, and an infinite loop in one would be reaped ten
            // minutes late rather than at its deadline.
            //
            // The same arm takes a measured run on a host that could not be
            // measured. Both keep the plain wall clock they always had.
            Some(cpu) if self.ceiling.is_some() => {
                if self.ceiling.is_some_and(|ceiling| cpu > ceiling) {
                    return Some(Stopped::TimeLimit);
                }
                if cpu > self.best {
                    self.best = cpu;
                    self.idle = Duration::ZERO;
                } else if !waiting {
                    // **Not scheduled is not idle**, so the window only runs
                    // while nothing was waiting for a core.
                    //
                    // Counting processor time was meant to tell a program that
                    // computes slowly from one that does not compute at all,
                    // and it does — until the host is loaded enough that a
                    // program gets *no* processor between two looks. Its
                    // reading then does not grow, and it is indistinguishable
                    // from a program asleep.
                    //
                    // Measured 2026-09-04, twelve Runners over eight physical
                    // cores: three correct submissions in a hundred and fifty
                    // were reported `Time limit exceeded` after using between a
                    // fifth and a third of their limit, each carrying the note
                    // `no processor time for 1.6 s`. The host's own pressure was
                    // a quarter of every window.
                    //
                    // What the deadline is still for is untouched. A program in
                    // an uninterruptible call is not runnable, and a program
                    // waiting on input is not runnable, so neither raises
                    // pressure and both are still reaped. And a program starved
                    // for ever is bounded by `cap` below, which nothing resets.
                    self.idle += since;
                }
            }
            _ => self.idle += since,
        }

        (self.idle >= self.window || elapsed >= self.cap).then_some(Stopped::WallClock)
    }
}

/// What the shim said about the one process it was there to watch.
struct Reported {
    cpu: Duration,
    peak_memory_bytes: u64,
    /// The same total, split into the program's own work and the kernel's work
    /// on its behalf.
    user: Duration,
    system: Duration,
}

/// Takes the shim's report out of the standard error it travelled on.
///
/// **The last one wins, and every one of them is removed.** A submission can
/// write a line in the same shape -- it shares the descriptor -- but the shim
/// kills every other process in the namespace before writing, so nothing is
/// alive to write after it. Removing the others as well keeps a forged line out
/// of what is stored against the submission.
fn take_report(stderr: &mut Vec<u8>, nonce: &str) -> Option<Reported> {
    let marker = format!("{nonce} aj-shim1 ").into_bytes();

    // **Bytes throughout.** Reading this as text to find the marker and
    // writing the text back would replace every invalid sequence with a
    // replacement character, so a program that printed a buffer would have it
    // rewritten by the act of measuring it.
    let mut found: Vec<(usize, usize)> = Vec::new();
    let mut at = 0;
    while at + marker.len() <= stderr.len() {
        if stderr[at..].starts_with(&marker) {
            let line = stderr[at..].iter().position(|byte| *byte == b'\n');
            // **A line with no newline after it was cut short** -- by the
            // output cap, or by the container dying mid-write. Half a report
            // parses into a smaller number than the truth, so it is not one.
            let ends = match line {
                Some(n) => at + n + 1,
                None => break,
            };
            found.push((at, ends));
            at = ends;
        } else {
            at += 1;
        }
    }

    let (last, ends) = *found.last()?;
    let said = String::from_utf8_lossy(&stderr[last + marker.len()..ends])
        .trim_end()
        .to_owned();

    // Every one of them, so a forged line is not stored against the submission
    // either. Backwards, so the earlier offsets stay valid.
    for (from, to) in found.into_iter().rev() {
        stderr.drain(from..to);
    }

    let mut fields = said.strip_prefix("ok ")?.split_whitespace().skip(2);
    let cpu_us: u64 = fields.next()?.parse().ok()?;
    let peak: u64 = fields.next()?.parse().ok()?;
    // The wall clock sits between them and is skipped: the sandbox keeps its
    // own and does not read the shim's.
    let user_us: u64 = fields.nth(1)?.parse().ok()?;
    let system_us: u64 = fields.next()?.parse().ok()?;
    Some(Reported {
        cpu: Duration::from_micros(cpu_us),
        peak_memory_bytes: peak,
        user: Duration::from_micros(user_us),
        system: Duration::from_micros(system_us),
    })
}

fn rand_suffix() -> u64 {
    use std::hash::{BuildHasher as _, RandomState};
    RandomState::new().hash_one(std::time::SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "0123456789abcdef";

    #[test]
    fn only_a_measured_run_in_an_image_with_a_shim_starts_as_root() {
        assert_eq!(container_user(true, true), "0:0");

        for (measured, shim, because) in [
            (true, false, "a measured run in an image with no shim"),
            (false, true, "a build in an image that happens to carry one"),
            (false, false, "everything else"),
        ] {
            assert_eq!(container_user(measured, shim), "65534:65534", "{because}");
        }
    }

    #[test]
    fn an_honest_report_is_taken_exactly() {
        // The numbers the shim and the cgroup gave for one real run.
        let time = measured_time(
            Duration::from_micros(221_812),
            Some(Duration::from_micros(271_044)),
        );
        assert_eq!(time, Duration::from_micros(221_812));
    }

    /// **A gap no container could have cost is not believed.** Whatever the
    /// report claims, the answer cannot then fall below the reading less
    /// everything a container start could honestly account for.
    #[test]
    fn a_report_the_reading_cannot_explain_is_not_believed() {
        let whole = Duration::from_secs(5);
        for claimed in [0, 1, 10, 379] {
            let time = measured_time(Duration::from_millis(claimed), Some(whole));
            assert_eq!(time, whole - UNEXPLAINED_GAP, "claiming {claimed} ms");
        }
    }

    /// **And the cost of that, said out loud rather than left to be discovered.**
    /// A forged report inside the gap is charged as it stands. Nothing here
    /// stops it; what stops it is the nonce, and this bounds what getting past
    /// the nonce is worth.
    #[test]
    fn a_lie_smaller_than_a_containers_own_cost_is_charged_as_it_stands() {
        let time = measured_time(Duration::ZERO, Some(Duration::from_millis(900)));
        assert_eq!(time, Duration::ZERO);
    }

    /// A run shorter than the gap has nothing to subtract from, and a floor
    /// below zero is zero rather than a panic.
    #[test]
    fn a_run_shorter_than_the_gap_keeps_what_the_shim_said() {
        let time = measured_time(Duration::from_millis(5), Some(Duration::from_millis(60)));
        assert_eq!(time, Duration::from_millis(5));
    }

    /// **The measured population, and none of it moves the charge.** These are
    /// the percentiles of the difference between the two instruments across
    /// 7077 runs under load, 2026-09-04: what a container cost on top of the
    /// program, from the median to the worst single run. Every one of them is
    /// explicable, so every one of them leaves the program charged with its own
    /// time -- which under the previous rule it was not, for the 5% above
    /// 120 ms.
    #[test]
    fn no_container_cost_ever_measured_moves_what_the_program_is_charged() {
        let reported = Duration::from_millis(150);
        for gap in [77u64, 98, 120, 219, 275, 476, 619] {
            let whole = reported + Duration::from_millis(gap);
            assert_eq!(
                measured_time(reported, Some(whole)),
                reported,
                "a container costing {gap} ms",
            );
        }
    }

    /// The worst difference in the measured population, still believed. Below
    /// this the constant would be failing honest runs again.
    #[test]
    fn the_largest_container_cost_ever_measured_is_still_explicable() {
        let reported = Duration::from_millis(100);
        let whole = reported + Duration::from_millis(619);
        assert_eq!(measured_time(reported, Some(whole)), reported);
    }

    /// Nothing to floor it with: a host the Runner could not read a cgroup on
    /// is one that refuses to judge, but the arithmetic still has to answer.
    #[test]
    fn without_a_cgroup_the_report_stands_alone() {
        assert_eq!(
            measured_time(Duration::from_millis(222), None),
            Duration::from_millis(222)
        );
    }

    /// The report is the *program's* time and the reading is the container's, so
    /// this cannot happen -- and if it ever does, the larger number is the safe
    /// one to charge.
    #[test]
    fn a_report_larger_than_the_whole_container_is_taken() {
        let time = measured_time(Duration::from_millis(300), Some(Duration::from_millis(271)));
        assert_eq!(time, Duration::from_millis(300));
    }

    /// **The split is read where a shim writes it**, and the total stays what
    /// it always was rather than being recomputed from the halves: the total is
    /// what a participant is judged on, and two numbers that must agree are two
    /// chances to disagree.
    #[test]
    fn a_report_carrying_the_split_gives_both_halves() {
        let mut stderr =
            format!("{NONCE} aj-shim1 ok 0 0 221812 9342976 232932 51000 170812\n").into_bytes();
        let said = take_report(&mut stderr, NONCE).expect("a report");
        assert_eq!(said.cpu, Duration::from_micros(221_812));
        assert_eq!(said.user, Duration::from_micros(51_000));
        assert_eq!(said.system, Duration::from_micros(170_812));
    }

    fn stderr_of(text: &str) -> Vec<u8> {
        text.as_bytes().to_vec()
    }

    /// **A program's standard error comes back byte for byte, or not at all.**
    /// Reading it as text to find the report and writing the text back replaces
    /// every invalid sequence with a replacement character -- so a program that
    /// printed a buffer, an image, or any other bytes would have them rewritten
    /// by the act of measuring it.
    /// **Half a report is not a report.** The output collector kills a container
    /// that passes the cap mid-write, and a line cut through its numbers parses
    /// into a smaller figure than the truth -- which is the one direction that
    /// must never be accepted on trust.
    #[test]
    fn a_report_cut_short_is_not_read() {
        let mut stderr = format!("{NONCE} aj-shim1 ok 0 0 2218").into_bytes();
        let before = stderr.clone();

        assert!(take_report(&mut stderr, NONCE).is_none());
        assert_eq!(stderr, before, "and it is left where it was");
    }

    #[test]
    fn a_report_with_nothing_before_it_is_read() {
        let mut stderr = format!("{NONCE} aj-shim1 ok 0 0 5 6 7 2 3\n").into_bytes();
        assert_eq!(
            take_report(&mut stderr, NONCE).unwrap().cpu,
            Duration::from_micros(5)
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn no_output_at_all_is_no_report() {
        let mut stderr = Vec::new();
        assert!(take_report(&mut stderr, NONCE).is_none());
    }

    /// Three forged lines and the real one. The shim writes after killing
    /// everything else, so the real one is last however many precede it.
    #[test]
    fn every_forgery_is_removed_however_many_there_are() {
        let real = format!("{NONCE} aj-shim1 ok 0 0 99 98 97 40 59\n");
        let forged = |us: u64| format!("{NONCE} aj-shim1 ok 0 0 {us} {us} {us} {us} {us}\n");
        let mut stderr = format!(
            "{}first\n{}second\n{}{}",
            forged(1),
            forged(2),
            forged(3),
            real
        )
        .into_bytes();

        let said = take_report(&mut stderr, NONCE).expect("a report");
        assert_eq!(said.cpu, Duration::from_micros(99));
        assert_eq!(said.peak_memory_bytes, 98);
        assert_eq!(String::from_utf8(stderr).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn output_that_is_not_utf8_survives_being_read() {
        let mut stderr = Vec::new();
        stderr.extend_from_slice(&[0xff, 0xfe, b'a', 0x80, b'B', 10]);
        let untouched = stderr.clone();
        stderr.extend_from_slice(format!("{NONCE} aj-shim1 ok 0 0 5 6 7 2 3\n").as_bytes());

        let said = take_report(&mut stderr, NONCE).expect("a report");
        assert_eq!(said.cpu, Duration::from_micros(5));
        assert_eq!(
            stderr, untouched,
            "the bytes before the report were rewritten"
        );
    }

    #[test]
    fn a_report_is_read_and_taken_out_of_what_the_participant_wrote() {
        let mut stderr = stderr_of(&format!(
            "a warning the program printed\n{NONCE} aj-shim1 ok 0 0 221812 9342976 232932 51000 170812\n"
        ));
        let said = take_report(&mut stderr, NONCE).expect("a report");

        assert_eq!(said.cpu, Duration::from_micros(221_812));
        assert_eq!(said.peak_memory_bytes, 9_342_976);
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "a warning the program printed\n"
        );
    }

    /// The submission shares the descriptor, so it can write a line in the same
    /// shape. The shim writes after killing everything else, so its line is the
    /// last one -- and none of them is left in what is stored.
    #[test]
    fn the_last_report_wins_and_a_forged_one_is_removed() {
        let mut stderr = stderr_of(&format!(
            "{NONCE} aj-shim1 ok 0 0 1 1 1 1 0\noutput\n{NONCE} aj-shim1 ok 0 11 45174 14446592 59615 20000 25174\n"
        ));
        let said = take_report(&mut stderr, NONCE).expect("a report");

        assert_eq!(said.cpu, Duration::from_micros(45_174));
        assert_eq!(String::from_utf8(stderr).unwrap(), "output\n");
    }

    #[test]
    fn a_line_carrying_someone_elses_nonce_is_not_a_report() {
        let mut stderr = stderr_of("ffff aj-shim1 ok 0 0 1 1 1 1 0\n");
        assert!(take_report(&mut stderr, NONCE).is_none());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ffff aj-shim1 ok 0 0 1 1 1 1 0\n"
        );
    }

    #[test]
    fn a_shim_that_failed_reports_no_measurement() {
        let mut stderr = stderr_of(&format!(
            "{NONCE} aj-shim1 failed setuid: Operation not permitted\n"
        ));
        assert!(take_report(&mut stderr, NONCE).is_none());
        assert!(stderr.is_empty(), "the line is still removed");
    }

    fn reading(oom_kills: u64, over_limit: u64) -> cgroups::Reading {
        cgroups::Reading {
            oom_kills,
            over_limit,
            ..Default::default()
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A timed run: a 200 ms limit and the deadline the pipeline sets with it.
    fn timed() -> Reaper {
        Reaper::new(
            &Profile::new("image", vec!["run".to_owned()])
                .wall_clock(ms(1600))
                .cpu_limit(ms(200)),
        )
    }

    /// **The case the whole thing exists for.** A program that keeps spending
    /// processor time is never reaped, however long the host makes it wait: it
    /// is descheduled, not stuck, and the two were indistinguishable while the
    /// deadline counted wall clock alone.
    #[test]
    fn a_program_that_keeps_computing_is_never_reaped_for_being_slow() {
        let mut reaper = timed();
        let mut cpu = ms(0);
        // A 200 ms budget spent over ten seconds -- a fiftieth of a processor,
        // which is what a badly oversubscribed host leaves a program. Every
        // look is progress, so the 1.6 s window never starts.
        for tick in 1..=40u64 {
            cpu += ms(5);
            assert_eq!(
                reaper.tick(ms(250), ms(250 * tick), Some(cpu), None),
                None,
                "tick {tick}, {cpu:?} of processor time"
            );
        }
        assert_eq!(cpu, ms(200), "it spent its whole limit and was left alone");
    }

    /// **The case this was measured into existence by.** A program the kernel
    /// gives *nothing* to between two looks spends no processor time, and until
    /// pressure was read it was indistinguishable from a program asleep.
    ///
    /// Measured 2026-09-04 on twelve Runners over eight physical cores: three
    /// correct submissions in a hundred and fifty came back `Time limit
    /// exceeded` after a fifth to a third of their limit, each noting `no
    /// processor time for 1.6 s`.
    #[test]
    fn a_program_starved_of_a_processor_is_not_reaped() {
        let mut reaper = timed();
        // It computed once, and then got no processor at all -- while something
        // in its cgroup was runnable and waiting the whole time.
        let mut stalled = ms(0);
        assert_eq!(
            reaper.tick(ms(250), ms(250), Some(ms(30)), Some(stalled)),
            None
        );
        for tick in 2..=40u64 {
            stalled += ms(200);
            assert_eq!(
                reaper.tick(ms(250), ms(250 * tick), Some(ms(30)), Some(stalled)),
                None,
                "tick {tick}: it was waiting for a core, not idle",
            );
        }
    }

    /// **And the deadline still ends it.** A program starved for ever is not
    /// left running for ever: nothing resets the absolute cap, so the run stops
    /// at ten windows whatever the pressure says.
    #[test]
    fn a_program_starved_for_ever_still_meets_the_absolute_cap() {
        let mut reaper = timed();
        let mut stalled = ms(0);
        let mut verdict = None;
        for tick in 1..=80u64 {
            stalled += ms(200);
            verdict = reaper.tick(ms(250), ms(250 * tick), Some(ms(30)), Some(stalled));
            if verdict.is_some() {
                // Ten times the 1.6 s window.
                assert_eq!(ms(250 * tick), ms(16000), "it stopped at the cap");
                break;
            }
        }
        assert_eq!(verdict, Some(Stopped::WallClock));
    }

    /// **Asleep is still asleep.** Nothing runnable means no pressure, so a
    /// program waiting on input or wedged in an uninterruptible call is reaped
    /// exactly as it was -- which is the whole of what the deadline is for.
    #[test]
    fn a_program_that_is_only_asleep_is_reaped_with_the_instrument_present() {
        let mut reaper = timed();
        // A reading that is present and never moves: nothing was waiting.
        let quiet = Some(ms(7));
        assert_eq!(reaper.tick(ms(250), ms(250), Some(ms(30)), quiet), None);
        for tick in 2..=7u64 {
            assert_eq!(
                reaper.tick(ms(250), ms(250 * tick), Some(ms(30)), quiet),
                None,
                "tick {tick}",
            );
        }
        assert_eq!(
            reaper.tick(ms(250), ms(2000), Some(ms(30)), quiet),
            Some(Stopped::WallClock),
        );
    }

    /// **A baseline that cannot go stale.** Pressure is recorded on every look,
    /// including the ones where the program made progress -- otherwise a burst
    /// of work would leave the comparison pointing at a reading from before it,
    /// and the first quiet look after it would read as starvation.
    #[test]
    fn progress_does_not_leave_the_pressure_baseline_behind() {
        let mut reaper = timed();
        // Four looks of real progress, with pressure climbing alongside.
        let mut cpu = ms(0);
        let mut stalled = ms(0);
        for tick in 1..=4u64 {
            cpu += ms(20);
            stalled += ms(100);
            assert_eq!(
                reaper.tick(ms(250), ms(250 * tick), Some(cpu), Some(stalled)),
                None,
            );
        }
        // Then it stops, and so does the waiting: it is asleep, not starved.
        // Six quiet looks are 1.5 s, just inside the 1.6 s window.
        for tick in 5..=10u64 {
            assert_eq!(
                reaper.tick(ms(250), ms(250 * tick), Some(cpu), Some(stalled)),
                None,
                "tick {tick}",
            );
        }
        assert_eq!(
            reaper.tick(ms(250), ms(3000), Some(cpu), Some(stalled)),
            Some(Stopped::WallClock),
            "the window ran out on a program that was doing nothing",
        );
    }

    /// And one that stops spending it is reaped, which is what the deadline was
    /// always for: waiting, or wedged in an uninterruptible call.
    #[test]
    fn a_program_that_stops_computing_is_reaped_after_the_window() {
        let mut reaper = timed();
        // It runs for a moment, and then spends nothing at all.
        assert_eq!(reaper.tick(ms(250), ms(250), Some(ms(30)), None), None);
        for tick in 2..=7u64 {
            assert_eq!(
                reaper.tick(ms(250), ms(250 * tick), Some(ms(30)), None),
                None,
                "tick {tick}",
            );
        }
        // The seventh quiet look puts the idle time past the 1.6 s window.
        assert_eq!(
            reaper.tick(ms(250), ms(2000), Some(ms(30)), None),
            Some(Stopped::WallClock),
        );
    }

    /// **Progress puts the window back**, so a program that computes in bursts
    /// is not reaped for the pauses between them.
    #[test]
    fn every_step_forward_starts_the_window_again() {
        let mut reaper = timed();
        for round in 0..4u64 {
            // Quiet for six ticks — 1.5 s, just inside the window.
            for tick in 0..6 {
                assert_eq!(
                    reaper.tick(ms(250), ms(250 * (round * 7 + tick)), Some(ms(10)), None),
                    None,
                );
            }
            // Then a step forward, which must clear it.
            assert_eq!(
                reaper.tick(
                    ms(250),
                    ms(250 * (round * 7 + 6)),
                    Some(ms(11 + round)),
                    None
                ),
                None,
            );
        }
    }

    /// Plainly past its budget is stopped rather than left to run.
    ///
    /// **The ceiling carries the container's own cost too**, because the
    /// reading it is compared against is the cgroup's: twice the limit plus
    /// [`UNEXPLAINED_GAP`], so 1.4 s for a 200 ms limit. Anything tighter would
    /// stop a correct program whose container was expensive -- the same mistake
    /// the reconciliation above exists to avoid, made at the other end.
    #[test]
    fn a_runaway_is_stopped_well_past_its_limit() {
        assert_eq!(
            timed().tick(ms(250), ms(250), Some(ms(1401)), None),
            Some(Stopped::TimeLimit)
        );
    }

    /// And the case that makes the ceiling generous: a program inside its limit
    /// whose container cost the worst ever measured is nowhere near it.
    #[test]
    fn a_correct_program_in_an_expensive_container_is_not_stopped() {
        assert_eq!(
            timed().tick(ms(250), ms(250), Some(ms(180 + 619)), None),
            None
        );
    }

    /// **And a program inside its budget is not**, which is the mistake that
    /// would cost a correct submission its verdict. The reading is the cgroup's
    /// and carries the container's own start, so a program at its limit reads
    /// well above it and must still be left alone.
    #[test]
    fn a_program_at_its_limit_is_left_alone() {
        for cpu in [200u64, 250, 274, 320] {
            assert_eq!(
                timed().tick(ms(250), ms(250), Some(ms(cpu)), None),
                None,
                "{cpu} ms"
            );
        }
    }

    /// A step nobody times keeps the deadline it always had.
    #[test]
    fn a_run_with_nothing_to_measure_keeps_the_plain_wall_clock() {
        let mut reaper =
            Reaper::new(&Profile::new("image", vec!["build".to_owned()]).wall_clock(ms(1000)));
        for tick in 1..4u64 {
            assert_eq!(
                reaper.tick(ms(250), ms(250 * tick), None, None),
                None,
                "tick {tick}"
            );
        }
        assert_eq!(
            reaper.tick(ms(250), ms(1000), None, None),
            Some(Stopped::WallClock)
        );
    }

    /// **A step nobody is timed on is reaped at its deadline while computing.**
    /// The case CI caught: a build has a cgroup like every other container, so
    /// the reading is there — and taking it as progress gave an infinite loop
    /// in a build ten times the minute a build is allowed. The reading is not
    /// the question; having a limit for it to mean something is.
    #[test]
    fn a_step_with_no_limit_is_reaped_at_its_deadline_even_while_it_computes() {
        let mut reaper =
            Reaper::new(&Profile::new("image", vec!["build".to_owned()]).wall_clock(ms(1000)));
        let mut cpu = ms(0);
        for tick in 1..4u64 {
            cpu += ms(250);
            assert_eq!(
                reaper.tick(ms(250), ms(250 * tick), Some(cpu), None),
                None,
                "tick {tick}",
            );
        }
        cpu += ms(250);
        assert_eq!(
            reaper.tick(ms(250), ms(1000), Some(cpu), None),
            Some(Stopped::WallClock),
            "a build that spins is stopped at a minute, not at ten",
        );
    }

    /// The end of it, however busy the program looks. Without this a program
    /// waking for a millisecond every quarter-second never stalls and never
    /// approaches its limit, and holds a Runner for ever.
    #[test]
    fn a_trickle_still_ends_at_the_cap() {
        let mut reaper = timed();
        let mut cpu = ms(0);
        let mut verdict = None;
        for tick in 1..=100u64 {
            cpu += ms(1);
            verdict = reaper.tick(ms(250), ms(250 * tick), Some(cpu), None);
            if verdict.is_some() {
                assert!(
                    ms(250 * tick) >= ms(16_000),
                    "ended at {tick} ticks, before the cap"
                );
                break;
            }
        }
        assert_eq!(verdict, Some(Stopped::WallClock));
    }

    /// Nothing the kernel did not count changes what a run was stopped by.
    #[test]
    fn no_kill_leaves_the_runtimes_answer_alone() {
        for stopped in [
            Stopped::OnItsOwn,
            Stopped::TimeLimit,
            Stopped::WallClock,
            Stopped::Output,
            Stopped::Memory,
        ] {
            assert_eq!(memory_kill(stopped, &reading(0, 0)), stopped);
        }
    }

    /// The case CI caught: the runtime said the container stopped on its own,
    /// and the kernel had killed something in it for being over the limit.
    #[test]
    fn a_kill_the_runtime_did_not_report_is_still_a_memory_limit() {
        assert_eq!(
            memory_kill(Stopped::OnItsOwn, &reading(1, 1)),
            Stopped::Memory
        );
    }

    /// **A host out of memory is not a submission over its limit.** `oom_kill`
    /// counts kills by any OOM killer, the system one included, so alone it
    /// would blame a program for the machine it ran on.
    #[test]
    fn a_kill_this_cgroup_did_not_earn_is_not_a_memory_limit() {
        assert_eq!(
            memory_kill(Stopped::OnItsOwn, &reading(1, 0)),
            Stopped::OnItsOwn
        );
    }

    /// **And reaching the limit is not being killed by it.** `oom` moves
    /// without anything dying: measured, `oom 845` against `oom_kill 843`.
    #[test]
    fn reaching_the_limit_without_dying_is_not_a_memory_limit() {
        assert_eq!(
            memory_kill(Stopped::OnItsOwn, &reading(0, 1)),
            Stopped::OnItsOwn
        );
    }

    /// Memory outranks the deadline and the output cap, which is what the
    /// runtime's own flag already did at this point.
    #[test]
    fn a_memory_kill_outranks_what_else_may_have_stopped_it() {
        assert_eq!(
            memory_kill(Stopped::WallClock, &reading(1, 1)),
            Stopped::Memory
        );
        assert_eq!(
            memory_kill(Stopped::Output, &reading(3, 2)),
            Stopped::Memory
        );
    }
}
