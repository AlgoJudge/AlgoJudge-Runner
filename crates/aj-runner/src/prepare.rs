//! Getting a package ready to judge, once for every submission to it.
//!
//! **Unpacking an archive and compiling the judge it declares depend on the
//! package and on nothing else**, so doing either per submission is the same
//! work done again for every person who solves the problem. Both now happen
//! here, inside the cache entry the archive already sits in, under the lock
//! that entry carries — so several Runners sharing a volume do it once between
//! them rather than once each.
//!
//! What a job is left with is three paths into a directory it may read and must
//! not write: the unpacked package, the built judge, and the archive they came
//! from.

use aj_protocol::cache::Entry;
use aj_protocol::stopping::Stopping;
use aj_standard_io::{Judge, Pipeline, Places};

/// What a package is unpacked into, inside its cache entry.
const EXTRACTED: &str = "_extracted";

/// Where judges built from it go, one directory per key.
const BUILDS: &str = "_build";

/// A package ready to be judged against.
pub struct Prepared {
    /// The unpacked package, in the cache, in both the views a bind mount
    /// needs.
    pub package: Places,
    /// The package's own configuration with the assignment's laid over it.
    pub config: aj_package::Config,
    pub tests: aj_package::TestSet,
    /// The checker or interactor it declares, built. `None` where it declares
    /// neither.
    pub judge: Option<Judge>,
}

/// Unpacks the package and builds its judge, or finds that somebody already
/// has.
///
/// `None` means the word came while waiting for another Runner to finish. There
/// is nothing to report: this Runner is stopping, and the job goes back to the
/// queue the way every stopped job does.
///
/// **Everything up to the lock being dropped is preparation; judging happens
/// after it.** Holding it through a judged run would serialize every Runner
/// sharing the cache on one submission, and none of what judging reads is
/// written by anybody: what protects the entry then is the holding marker the
/// `Entry` carries.
pub async fn prepare(
    entry: &Entry,
    pipeline: &Pipeline<aj_sandbox::Docker>,
    format: &str,
    overlay: Option<&serde_json::Value>,
    stopping: &Stopping,
) -> Result<Option<Prepared>, String> {
    let locked = match entry.lock(stopping).await.map_err(|e| e.to_string())? {
        Some(locked) => locked,
        None => return Ok(None),
    };

    // ── the package, unpacked ───────────────────────────────────────────────
    let at = match locked.published(EXTRACTED) {
        Some(at) => {
            tracing::debug!(package = %entry.path().display(), "already unpacked");
            at
        }
        None => {
            let filling = locked.filling(EXTRACTED).map_err(|e| e.to_string())?;
            let into = filling.at().to_path_buf();
            let archive = entry.path().to_path_buf();
            // Off the thread that is also driving containers: a package may
            // unpack to a gigabyte, which `ArchiveLimits` is what bounds.
            let written = tokio::task::spawn_blocking(move || {
                aj_package::extract(&archive, &into, &aj_package::ArchiveLimits::default())
            })
            .await
            .map_err(|e| format!("the package could not be unpacked: {e}"))?
            .map_err(|e| e.to_string())?;

            // **`tests/` may legitimately not be there**, since an interactive
            // package may name its tests by a count and ship no files at all —
            // and a judge's container mounts the directory whole. Made here, so
            // that nothing writes into the cache outside this lock.
            std::fs::create_dir_all(filling.at().join("tests"))
                .map_err(|e| format!("the package's tests/ could not be made: {e}"))?;

            filling
                .publish(serde_json::json!({ "files": written }))
                .map_err(|e| e.to_string())?
        }
    };
    let package = Places {
        on_host: entry.on_host(&at),
        here: at,
    };

    // ── what it says about itself ───────────────────────────────────────────
    let declared = std::fs::read_to_string(package.here.join("config.yml"))
        .map_err(|e| format!("config.yml could not be read: {e}"))?;
    let config = aj_package::Config::parse_as(&declared, format)
        .and_then(|c| c.overlaid(overlay))
        .map_err(|e| e.to_string())?;
    let tests = aj_package::TestSet::read(&package.here, &config).map_err(|e| e.to_string())?;

    // ── the judge, built ────────────────────────────────────────────────────
    //
    // **Keyed by the source it is built from, the language it is built in and
    // the image that did the building.** The first two because an assignment's
    // configuration may repoint either — `Config::overlaid` merges `checker`
    // like any other member — and the third because a language image
    // republished under the same tag is a different compiler, and a program
    // built by the old one would go on being served for ever.
    let judge = match config.checker.as_ref().or(config.interactor.as_ref()) {
        None => None,
        Some(declares) => {
            let (image, start) = pipeline.judge_in(declares)?;
            let image_id = pipeline.image_id(&image).await?;
            let key = format!(
                "{BUILDS}/{}",
                keyed(&[&declares.source, &declares.language, &image_id]),
            );

            let at = match locked.published(&key) {
                Some(at) => {
                    tracing::debug!(key, "the judge this package declares is already built");
                    at
                }
                None => {
                    let filling = locked.filling(&key).map_err(|e| e.to_string())?;
                    let into = Places {
                        on_host: entry.on_host(filling.at()),
                        here: filling.at().to_path_buf(),
                    };
                    pipeline.build_judge(declares, &package, &into).await?;
                    let at = filling
                        .publish(serde_json::json!({
                            "source": declares.source,
                            "language": declares.language,
                            "image": image,
                            "imageId": image_id,
                        }))
                        .map_err(|e| e.to_string())?;

                    // **What this one supersedes goes.** Every republished
                    // language image would otherwise leave its predecessor
                    // behind, in every entry of every package that declares a
                    // judge, and a cache that only ever hits would grow without
                    // anything ever choosing to keep them.
                    for (name, record) in locked.products() {
                        let same = record["source"] == serde_json::json!(declares.source)
                            && record["language"] == serde_json::json!(declares.language);
                        if name != key && name.starts_with(BUILDS) && same {
                            tracing::info!(name, "a judge built against an older image is dropped");
                            locked.forget(&name).map_err(|e| e.to_string())?;
                        }
                    }
                    at
                }
            };

            // **Where it was published, not where it was built.** `build_judge`
            // assembles in a directory of this Runner's own, which the rename
            // into place leaves behind.
            Some(Judge {
                at: Places {
                    on_host: entry.on_host(&at.join("out")),
                    here: at.join("out"),
                },
                image,
                start,
            })
        }
    };

    // **After the lock, not before it.** What was just published counts toward
    // the ceiling, and an entry that grows after it arrives would otherwise
    // never be weighed: eviction used to run only where something was
    // downloaded.
    drop(locked);
    entry.evict_to_fit();

    Ok(Some(Prepared {
        package,
        config,
        tests,
        judge,
    }))
}

/// A short, stable name for what a judge was built from.
///
/// Hex rather than anything readable: the parts are a path, a language id and an
/// image digest, and two of the three carry characters a directory name should
/// not.
fn keyed(parts: &[&str]) -> String {
    use sha2::Digest as _;
    let mut digest = sha2::Sha256::new();
    for part in parts {
        digest.update(part.as_bytes());
        digest.update(*b"\n");
    }
    hex::encode(&digest.finalize()[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key moves with every part of what it names, and with nothing else.
    ///
    /// **The image id is in it because a tag moves.** A language image
    /// republished under `lang-gcc:0` is a different compiler, and a checker
    /// built by its predecessor would otherwise be served for as long as the
    /// package stayed in the cache.
    #[test]
    fn a_judge_is_filed_under_what_it_was_built_from() {
        let one = keyed(&["checker/checker.cpp", "cpp17-gcc", "sha256:aa"]);

        assert_eq!(
            one,
            keyed(&["checker/checker.cpp", "cpp17-gcc", "sha256:aa"])
        );
        assert_ne!(one, keyed(&["checker/other.cpp", "cpp17-gcc", "sha256:aa"]));
        assert_ne!(
            one,
            keyed(&["checker/checker.cpp", "cpp17-clang", "sha256:aa"])
        );
        assert_ne!(
            one,
            keyed(&["checker/checker.cpp", "cpp17-gcc", "sha256:bb"])
        );
    }

    /// **The separator is part of the key**, or two different sets of parts
    /// would run together into the same string: a source of `a` with a language
    /// of `bc` and a source of `ab` with a language of `c`.
    #[test]
    fn two_sets_of_parts_do_not_run_together() {
        assert_ne!(keyed(&["a", "bc"]), keyed(&["ab", "c"]));
    }

    /// A name a directory can carry, whatever was hashed into it.
    #[test]
    fn the_key_is_a_name() {
        let key = keyed(&["checker/../checker.cpp", "cpp", "sha256:aa/bb"]);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()), "{key}");
    }
}
