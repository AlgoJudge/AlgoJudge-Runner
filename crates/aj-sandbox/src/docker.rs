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
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, InspectContainerOptions,
    KillContainerOptions, ListContainersOptionsBuilder, LogsOptionsBuilder,
    RemoveContainerOptionsBuilder, StartContainerOptions, WaitContainerOptions,
};
use futures_util::StreamExt as _;

use crate::profile::{Outcome, Profile, Stopped};
use crate::{Error, Result, Sandbox};

pub struct Docker {
    client: bollard::Docker,
}

impl Docker {
    pub fn connect() -> Result<Self> {
        Ok(Self {
            client: bollard::Docker::connect_with_local_defaults()?,
        })
    }

    /// Every container this Runner has ever started carries these, so an orphan
    /// from a previous incarnation can be found and removed. Job containers
    /// outlive the Runner — that is the one real cost of the sibling model, and
    /// without a sweep a crash-loop fills the evaluation host with dead
    /// sandboxes.
    fn labels(&self) -> HashMap<String, String> {
        HashMap::from([("algojudge.sandbox".to_owned(), "1".to_owned())])
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

    /// Removes every sandbox container this Runner is responsible for, and says
    /// how many there were.
    ///
    /// **Run at start.** Job containers are siblings, so they outlive the
    /// process that made them — that is the one real cost of not using nested
    /// containers. Without this sweep a crash-loop fills the evaluation host
    /// with dead sandboxes until it runs out of disk.
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

    fn host_config(&self, profile: &Profile) -> HostConfig {
        let memory = (profile.memory_kib * 1024) as i64;

        HostConfig {
            // No route anywhere. The first line of the adversarial suite.
            network_mode: Some("none".to_owned()),
            cap_drop: Some(vec!["ALL".to_owned()]),
            security_opt: Some(vec!["no-new-privileges".to_owned()]),
            readonly_rootfs: Some(true),

            memory: Some(memory),
            // Equal to `memory`, not absent. Without this the limit means
            // nothing: the process swaps instead of being killed, and a memory
            // limit that can be evaded is not a limit.
            memory_swap: Some(memory),
            pids_limit: Some(profile.pids),
            nano_cpus: Some((profile.cpus * 1_000_000_000.0) as i64),

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

            tmpfs: profile.tmpfs_kib.map(|kib| {
                HashMap::from([(
                    "/tmp".to_owned(),
                    // Writable and **not executable**: a directory a submission
                    // can write to and then execute from is the shortest route
                    // from "produced output" to "ran something we did not
                    // compile".
                    format!("rw,noexec,nosuid,size={kib}k"),
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
        let info = self.client.info().await?;
        match info.cgroup_version {
            Some(SystemInfoCgroupVersionEnum::_2) => Ok(()),
            other => Err(Error::Refused(format!(
                "this host reports cgroup version {other:?}, and the Runner requires v2 — \
                 memory limits and process-tree accounting are not reliable on v1",
            ))),
        }
    }

    async fn run(&self, profile: &Profile) -> Result<Outcome> {
        let name = format!(
            "algojudge-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos() as u64 ^ rand_suffix(),
        );

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
            host_config: Some(self.host_config(profile)),
            ..Default::default()
        };

        self.client
            .create_container(
                Some(CreateContainerOptionsBuilder::default().name(&name).build()),
                config,
            )
            .await?;

        // From here on the container exists, so every path out removes it.
        let outcome = self.supervise(&name, profile).await;
        self.remove(&name).await;
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
        let (overflow_tx, mut overflow_rx) = tokio::sync::oneshot::channel::<()>();
        let collector = tokio::spawn(collect(
            self.client.clone(),
            name.to_owned(),
            profile.max_output_bytes,
            overflow_tx,
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
            _ = &mut overflow_rx => {
                tracing::debug!(container = name, "killed for output");
                self.kill(name).await;
                Stopped::Output
            }
        };

        let wall_time = started.elapsed();
        let (stdout, stderr) = collector
            .await
            .map_err(|e| Error::Refused(format!("the output collector did not finish: {e}")))??;

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
            // Not guessed. See `Outcome`.
            peak_memory_kib: None,
            cpu_time: None,
        })
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
    overflow: tokio::sync::oneshot::Sender<()>,
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

    let mut overflow = Some(overflow);

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
                if let Some(tx) = overflow.take() {
                    let _ = tx.send(());
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
