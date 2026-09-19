//! Proving this host can judge, before this Runner offers to.
//!
//! **A Runner that cannot judge must not register.** It has no way to say so:
//! `MachineDto` is a closed shape, so a Runner that registers and then claims
//! nothing is a green row in the manager's panel against a queue that never
//! drains — the quieter cousin of the outcome `main` already refuses, where a
//! Runner "registers and answers the protocol and then fails every job it
//! claims".
//!
//! **This is the moment the download is free.** Inside a claimed job it would
//! be spent from a participant's lease, and a failure there reports through
//! `ReportResult::failed`, which ends the attempt rather than handing it back.
//!
//! The bug this exists to close shipped in 0.1.0: nothing on the judging path
//! ever pulled an image. A comment said the first judged run would — a true
//! sentence about `docker run`, and this Runner speaks the daemon's API, where
//! a missing image is a 404 nobody catches. An installation that was never
//! updated therefore had no toolchain at all and failed every submission.

use aj_sandbox::{Docker, Sandbox as _, ShimFeatures};
use aj_standard_io::language::Wanted;
use aj_standard_io::Images;

/// What the gate needs of a sandbox.
///
/// **Three methods, so the decision table below can be exercised without a
/// daemon.** Every branch of it is a judgment call — a name of ours is never
/// asked of a registry, a pull that failed over a copy that is here is not a
/// refusal — and a rule that can only be reached through Docker is a rule
/// nobody reaches.
#[async_trait::async_trait]
pub trait Obtains {
    /// The id of an image already on this host, if it is.
    async fn here(&self, image: &str) -> Option<String>;

    /// Asks the registry whatever is here already. The image's id, or what went
    /// wrong in the daemon's own words.
    async fn pull(&self, image: &str) -> Result<String, String>;

    /// What the image's measuring shim can do, if it carries one at all.
    async fn shim(&self, image: &str) -> Option<ShimFeatures>;
}

#[async_trait::async_trait]
impl Obtains for Docker {
    async fn here(&self, image: &str) -> Option<String> {
        self.image_id(image).await.ok()
    }

    async fn pull(&self, image: &str) -> Result<String, String> {
        self.pull_image(image).await.map_err(|e| e.to_string())
    }

    async fn shim(&self, image: &str) -> Option<ShimFeatures> {
        self.image_shim(image).await
    }
}

/// Puts every image this Runner judges with on this host, and proves each can
/// judge.
///
/// `settings` maps an image key to the variable an operator writes for it,
/// without the `AJ_` prefix, so a refusal names the thing they would change.
///
/// **Every problem is collected before any is reported.** A wrong `REGISTRY` is
/// four problems, and an operator should not learn them one restart at a time.
pub async fn ready<O: Obtains + ?Sized>(
    sandbox: &O,
    images: &Images,
    settings: &[(&str, &str)],
) -> anyhow::Result<()> {
    let mut refused: Vec<String> = Vec::new();

    for wanted in images.wanted() {
        if !obtained(sandbox, &wanted, settings, &mut refused).await {
            continue;
        }

        // **The half a pull cannot reach.** An image that is present and wrong
        // — one from before the shim existed, or an operator's own — satisfies
        // every check of presence and then fails every judged run in a sentence
        // indistinguishable from the image being absent. That ambiguity is why
        // the last incident of this kind read as a broken host.
        match sandbox.shim(&wanted.image).await {
            None => refused.push(format!(
                "{} carries no {}, so a judged run in it would produce neither \
                 output nor a measurement. It is the wrong image rather than a \
                 missing one{}",
                wanted.image,
                aj_sandbox::SHIM,
                ours(&wanted, settings),
            )),
            // **A warning and not a refusal**, deliberately. `socket_input` is
            // asked for only by problems carrying an interactor, so refusing the
            // whole Runner over it would stop a fleet from judging everything else.
            Some(features) if !features.socket_input => tracing::warn!(
                image = wanted.image,
                "this image's shim predates the input arriving as a descriptor, \
                 so problems with an interactor will be refused one at a time"
            ),
            Some(_) => {}
        }
    }

    if refused.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "this Runner cannot judge and will not register, so that nothing is \
         claimed that it would fail:\n  - {}",
        refused.join("\n  - "),
    )
}

/// Gets one image onto this host. Answers whether there is anything to probe.
async fn obtained<O: Obtains + ?Sized>(
    sandbox: &O,
    wanted: &Wanted,
    settings: &[(&str, &str)],
    refused: &mut Vec<String>,
) -> bool {
    // **A name nobody published is never asked of a registry.** The compiled-in
    // defaults are `algojudge/lang-*:local`, build products this repository
    // publishes nowhere, and an unqualified name resolves against Docker Hub —
    // where `algojudge` is not us. A *successful* pull would replace a locally
    // built image on a host that runs untrusted code.
    if !wanted.operators {
        if sandbox.here(&wanted.image).await.is_none() {
            refused.push(format!(
                "{} is not on this host{}",
                wanted.image,
                ours(wanted, settings),
            ));
            return false;
        }
        return true;
    }

    // Asked before the pull so that a tag which has moved can be said out loud.
    // It is the only way anybody learns that it moved, and it is what turns
    // "judging broke after the update" into a two-second diagnosis.
    let before = sandbox.here(&wanted.image).await;

    match sandbox.pull(&wanted.image).await {
        Ok(after) => {
            if before.is_some_and(|before| before != after) {
                tracing::warn!(
                    image = wanted.image,
                    "this tag now names a different image than it did"
                );
            }
            true
        }
        // **A copy is here and the registry is not reachable: judge anyway.**
        // This is the installation with no route out, the side-loaded one, the
        // private registry an anonymous pull cannot authenticate to, and the
        // thirty seconds the network was down. None of them is a reason to stop
        // judging with a compiler that is right here.
        Err(e) if before.is_some() => {
            tracing::warn!(
                image = wanted.image,
                e,
                "could not be pulled, so the copy already on this host is what judges"
            );
            true
        }
        Err(e) => {
            refused.push(format!(
                "{} could not be pulled and is not on this host: {e}. Set {} to an \
                 image this host can reach, or put it here yourself with \
                 `docker pull {}`",
                wanted.image,
                settings_for(&wanted.keys, settings),
                wanted.image,
            ));
            false
        }
    }
}

/// What to do about an image of **ours** that is missing or cannot judge, named
/// for *this* key rather than for whichever happened to be first.
fn ours(wanted: &Wanted, settings: &[(&str, &str)]) -> String {
    if wanted.operators {
        return String::new();
    }
    // Derived from the name rather than from a table of four, which would be a
    // fifth place that knows the keys and a fifth place to drift.
    let build = wanted
        .image
        .rsplit('/')
        .next()
        .and_then(|last| last.split(':').next())
        .and_then(|name| name.strip_prefix("lang-"))
        .map(|dir| format!("build it from `images/{dir}/`"))
        .unwrap_or_else(|| "build it".to_owned());
    format!(
        ". This is the compiled-in development name, which is published \
         nowhere: {build}, or set {} to a published image",
        settings_for(&wanted.keys, settings),
    )
}

/// Every variable that points at one image, because an operator who aimed two
/// of them at the same reference has two to change.
fn settings_for(keys: &[&str], settings: &[(&str, &str)]) -> String {
    let named: Vec<String> = keys
        .iter()
        .map(|key| {
            settings
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, name)| format!("AJ_{name}"))
                .unwrap_or_else(|| format!("the image setting for {key}"))
        })
        .collect();
    match named.split_last() {
        None => "its image setting".to_owned(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_standard_io::language::{CLANG, CPYTHON, GCC, PYPY};
    use std::collections::HashMap;
    use std::sync::Mutex;

    const SETTINGS: &[(&str, &str)] = &[
        (GCC, "Sandbox__Image__Gcc"),
        (CLANG, "Sandbox__Image__Clang"),
        (CPYTHON, "Sandbox__Image__Python"),
        (PYPY, "Sandbox__Image__Pypy"),
    ];

    /// A host whose answers are stated rather than found.
    #[derive(Default)]
    struct Host {
        /// image -> its id, for what is already here.
        here: HashMap<String, String>,
        /// image -> what a pull answers. Absent means the pull succeeds and
        /// leaves the id `pulled-<image>`.
        pulls: HashMap<String, Result<String, String>>,
        /// image -> its shim. Absent means it carries none.
        shims: HashMap<String, ShimFeatures>,
        asked: Mutex<Vec<String>>,
    }

    impl Host {
        /// Everything present, shimmed, and pulling cleanly.
        fn sound(images: &Images) -> Self {
            let mut host = Self::default();
            for wanted in images.wanted() {
                host.here
                    .insert(wanted.image.clone(), format!("id-{}", wanted.image));
                host.shims
                    .insert(wanted.image.clone(), ShimFeatures { socket_input: true });
            }
            host
        }

        fn pulled(&self) -> Vec<String> {
            self.asked.lock().expect("the record").clone()
        }
    }

    #[async_trait::async_trait]
    impl Obtains for Host {
        async fn here(&self, image: &str) -> Option<String> {
            self.here.get(image).cloned()
        }

        async fn pull(&self, image: &str) -> Result<String, String> {
            self.asked
                .lock()
                .expect("the record")
                .push(image.to_owned());
            self.pulls
                .get(image)
                .cloned()
                .unwrap_or_else(|| Ok(format!("pulled-{image}")))
        }

        async fn shim(&self, image: &str) -> Option<ShimFeatures> {
            self.shims.get(image).copied()
        }
    }

    fn all_named() -> Images {
        Images::default()
            .with(GCC, "ghcr.io/algojudge/lang-gcc:0")
            .with(CLANG, "ghcr.io/algojudge/lang-clang:0")
            .with(CPYTHON, "ghcr.io/algojudge/lang-python:0")
            .with(PYPY, "ghcr.io/algojudge/lang-pypy:0")
    }

    fn block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
            .block_on(f)
    }

    /// **The difference between this gate and the one-line fix.** An image that
    /// is already here is pulled anyway, because a moving tag answers "here"
    /// forever and the image it names may have moved underneath it.
    #[test]
    fn an_image_that_is_already_here_is_pulled_anyway() {
        let images = all_named();
        let host = Host::sound(&images);

        block(ready(&host, &images, SETTINGS)).expect("a sound host judges");

        assert_eq!(host.pulled().len(), 4, "every named image is asked for");
    }

    /// **The trap that would replace a developer's image with a stranger's.**
    /// `algojudge/lang-gcc:local` is unqualified, so a registry lookup goes to
    /// Docker Hub, where `algojudge` is somebody else.
    #[test]
    fn a_name_of_ours_is_never_asked_of_a_registry() {
        let images = Images::default();
        let host = Host::sound(&images);

        block(ready(&host, &images, SETTINGS)).expect("the built images judge");

        assert!(
            host.pulled().is_empty(),
            "asked a registry for a name we publish nowhere: {:?}",
            host.pulled()
        );
    }

    #[test]
    fn a_host_that_cannot_reach_the_registry_judges_with_what_it_has() {
        let images = all_named();
        let mut host = Host::sound(&images);
        for wanted in images.wanted() {
            host.pulls
                .insert(wanted.image.clone(), Err("no route to host".to_owned()));
        }

        block(ready(&host, &images, SETTINGS)).expect("a copy is here, so it judges");
    }

    #[test]
    fn a_pull_that_failed_over_nothing_refuses_and_names_the_setting() {
        let images = all_named();
        let mut host = Host::sound(&images);
        let gcc = images.named(GCC).expect("the gcc image").to_owned();
        host.here.remove(&gcc);
        host.pulls
            .insert(gcc.clone(), Err("manifest unknown".to_owned()));

        let said = block(ready(&host, &images, SETTINGS)).expect_err("nothing to judge with");
        let said = said.to_string();

        assert!(said.contains("AJ_Sandbox__Image__Gcc"), "got {said}");
        assert!(said.contains("manifest unknown"), "got {said}");
        assert!(said.contains("docker pull"), "got {said}");
    }

    /// The message must be about the key that is missing, not about whichever
    /// key the loop happened to reach first.
    #[test]
    fn a_name_of_ours_that_was_never_built_names_its_own_setting() {
        let images = Images::default();
        let mut host = Host::sound(&images);
        let clang = images.named(CLANG).expect("the clang image").to_owned();
        host.here.remove(&clang);

        let said = block(ready(&host, &images, SETTINGS))
            .expect_err("it is not here")
            .to_string();

        assert!(said.contains("AJ_Sandbox__Image__Clang"), "got {said}");
        assert!(said.contains("images/clang/"), "got {said}");
        assert!(!said.contains("AJ_Sandbox__Image__Gcc"), "got {said}");
    }

    /// **The half a pull cannot reach.** The image is here, and pulling it
    /// succeeded, and it still cannot judge.
    #[test]
    fn an_image_that_cannot_judge_is_refused_even_though_it_is_here() {
        let images = all_named();
        let mut host = Host::sound(&images);
        let gcc = images.named(GCC).expect("the gcc image").to_owned();
        host.shims.remove(&gcc);

        let said = block(ready(&host, &images, SETTINGS))
            .expect_err("it cannot judge")
            .to_string();

        assert!(said.contains(aj_sandbox::SHIM), "got {said}");
        assert!(said.contains(&gcc), "got {said}");
    }

    /// Refusing a whole Runner over a capability most problems never use would
    /// stop a fleet from judging everything else.
    #[test]
    fn a_shim_that_cannot_take_a_descriptor_is_a_warning_and_not_a_refusal() {
        let images = all_named();
        let mut host = Host::sound(&images);
        for wanted in images.wanted() {
            host.shims.insert(
                wanted.image.clone(),
                ShimFeatures {
                    socket_input: false,
                },
            );
        }

        block(ready(&host, &images, SETTINGS)).expect("it judges everything but interactors");
    }

    /// A wrong `REGISTRY` is four problems, and an operator should not learn
    /// them one restart at a time.
    #[test]
    fn every_problem_is_reported_at_once() {
        let images = all_named();
        let mut host = Host::sound(&images);
        for wanted in images.wanted() {
            host.here.remove(&wanted.image);
            host.pulls
                .insert(wanted.image.clone(), Err("manifest unknown".to_owned()));
        }

        let said = block(ready(&host, &images, SETTINGS))
            .expect_err("none of them is here")
            .to_string();

        assert_eq!(
            said.matches("manifest unknown").count(),
            4,
            "one restart per image is what this test exists to prevent: {said}"
        );
    }
}
