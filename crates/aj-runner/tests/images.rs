//! The start-up gate, against a real daemon.
//!
//! The decision table is exercised without one in `images.rs` itself; these are
//! the two things a fake cannot answer — that a reference really is fetched,
//! and that a real image which pulls perfectly well is still refused when it
//! cannot judge.
//!
//! ```text
//! ./x test -p aj-runner --test images -- --include-ignored --test-threads=1
//! ```

use aj_runner::config::IMAGE_SETTINGS;
use aj_runner::images;
use aj_standard_io::language::{CLANG, CPYTHON, GCC, PYPY};
use aj_standard_io::Images;

fn sandbox() -> aj_sandbox::Docker {
    aj_sandbox::Docker::connect("test-images").expect("a container runtime")
}

/// Every key pointed at one reference, so a case is about the reference rather
/// than about which of the four happened to be reached first.
fn all(image: &str) -> Images {
    Images::default()
        .with(GCC, image)
        .with(CLANG, image)
        .with(CPYTHON, image)
        .with(PYPY, image)
}

/// **The reported bug, from the outside.** A host with none of the images is
/// told which setting to change rather than judging every submission against
/// nothing.
#[tokio::test]
#[ignore = "needs a container runtime and a route to a registry"]
async fn a_host_that_cannot_fetch_its_images_refuses_and_names_the_setting() {
    let images = all("ghcr.io/algojudge/lang-gcc:no-such-tag-9f3a2b");

    let said = images::ready(&sandbox(), &images, IMAGE_SETTINGS)
        .await
        .expect_err("there is no such tag, so there is nothing to judge with")
        .to_string();

    assert!(
        said.contains("AJ_Sandbox__Image__Gcc"),
        "an operator is told the variable they would change: {said}"
    );
    assert!(
        said.contains("will not register"),
        "and that nothing will be claimed that this Runner would fail: {said}"
    );
}

/// **The half a pull cannot reach, against a real image.** `alpine:3` fetches
/// perfectly and carries no shim — which is what an operator's own image looks
/// like to this Runner, and what a language image from before the shim existed
/// looks like too. Every check of presence passes and it still cannot judge.
#[tokio::test]
#[ignore = "needs a container runtime and a route to a registry"]
async fn an_image_that_fetches_but_cannot_judge_is_refused_for_that_reason() {
    let images = all("alpine:3");

    let said = images::ready(&sandbox(), &images, IMAGE_SETTINGS)
        .await
        .expect_err("it pulls, and it cannot judge")
        .to_string();

    assert!(
        said.contains(aj_sandbox::SHIM),
        "the reason has to be the shim and not the fetch: {said}"
    );
    assert!(
        !said.contains("could not be pulled"),
        "it pulled; saying otherwise would send an operator after the registry: {said}"
    );
}
