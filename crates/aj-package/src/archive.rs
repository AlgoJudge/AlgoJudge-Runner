//! Getting a package onto disk without letting it choose where.
//!
//! Every check here refuses; none repairs. A package that trips one of them is
//! an **infrastructure failure** — the submission was never judged, and the
//! author needs to know which rule they broke, not a shrug.
//!
//! The load-bearing one is the last: **the declared size is not believed.** A
//! ZIP entry's header states its uncompressed size and an attacker writes that
//! header, so the cap is enforced on the bytes actually written. A checker that
//! reads `size()` and decides it is fine has checked the attacker's opinion.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

pub struct Limits {
    pub max_entries: usize,
    pub max_entry_bytes: u64,
    pub max_total_bytes: u64,
    /// Total uncompressed over total compressed. A cheap early signal, not the
    /// real defence — the absolute caps are. Set generously, because test output
    /// is legitimately very compressible and a problem with a megabyte of
    /// repeated digits is an ordinary problem, not an attack.
    pub max_ratio: u64,
    pub max_path_length: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            max_entry_bytes: 256 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
            max_ratio: 1_000,
            max_path_length: 255,
        }
    }
}

/// Unpacks the archive at `archive` into `into`, which must not already exist.
///
/// Returns how many files were written.
pub fn extract(archive: &Path, into: &Path, limits: &Limits) -> Result<usize> {
    if into.exists() {
        return Err(Error::refused(format!(
            "{} already exists; a package is unpacked into a fresh directory so \
             nothing from a previous job can survive into this one",
            into.display()
        )));
    }

    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))?;

    if zip.len() > limits.max_entries {
        return Err(Error::refused(format!(
            "the archive holds {} entries and the limit is {}",
            zip.len(),
            limits.max_entries
        )));
    }

    std::fs::create_dir_all(into)?;

    let mut written = 0usize;
    let mut total_uncompressed = 0u64;
    let mut total_compressed = 0u64;

    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let name = entry.name().to_owned();

        // A symlink is how an archive writes outside its own directory without
        // ever containing a `..`: unpack the link, then unpack a file "through"
        // it. Refused outright — a package has no use for one.
        if let Some(mode) = entry.unix_mode() {
            if mode & 0o170000 == 0o120000 {
                return Err(Error::refused(format!("{name} is a symbolic link")));
            }
        }

        let relative = safe_path(&name, limits.max_path_length)?;
        let destination = into.join(&relative);

        if entry.is_dir() {
            std::fs::create_dir_all(&destination)?;
            continue;
        }

        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Belt and braces: the path was checked before joining, and this checks
        // the result. Cheap, and the two catch different mistakes — the first
        // catches a hostile archive, the second catches a bug here.
        if !destination.starts_with(into) {
            return Err(Error::refused(format!(
                "{name} resolves outside the package"
            )));
        }

        let mut sink = std::fs::File::create(&destination)?;
        let copied =
            copy_bounded(&mut entry, &mut sink, limits.max_entry_bytes).inspect_err(|_| {
                // A half-written entry is a corrupt one. It goes before the
                // refusal is returned, so nothing later reads it as a test.
                let _ = std::fs::remove_file(&destination);
            })?;
        sink.flush()?;

        total_uncompressed += copied;
        total_compressed += entry.compressed_size();
        written += 1;

        if total_uncompressed > limits.max_total_bytes {
            return Err(Error::refused(format!(
                "the archive unpacks to more than {} bytes",
                limits.max_total_bytes
            )));
        }
    }

    // Checked at the end, on what was actually written rather than on what the
    // headers claimed. A compressed total of zero is an archive of empty files,
    // which is odd but not a bomb.
    if total_compressed > 0 && total_uncompressed / total_compressed > limits.max_ratio {
        return Err(Error::refused(format!(
            "the archive expands {}× and the limit is {}×",
            total_uncompressed / total_compressed,
            limits.max_ratio
        )));
    }

    tracing::debug!(
        files = written,
        bytes = total_uncompressed,
        "package unpacked"
    );
    Ok(written)
}

/// The entry's path, if it is one a package is allowed to contain.
///
/// Rejects absolute paths, `..`, drive prefixes, and anything outside a
/// deliberately narrow character set. The allow-list is the part that ages well:
/// a deny-list of "dangerous" names needs updating every time somebody finds a
/// new one.
fn safe_path(name: &str, max_length: usize) -> Result<PathBuf> {
    if name.is_empty() {
        return Err(Error::refused("an entry has no name"));
    }
    if name.len() > max_length {
        return Err(Error::refused(format!(
            "an entry name is {} characters and the limit is {max_length}",
            name.len()
        )));
    }
    if name.contains('\0') {
        return Err(Error::refused("an entry name contains a null byte"));
    }

    let bad = |c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/'));
    if let Some(c) = name.chars().find(|&c| bad(c)) {
        return Err(Error::refused(format!(
            "{name} contains {c:?}, and a package's names are letters, digits, \
             dot, dash, underscore and slash"
        )));
    }

    let path = Path::new(name);
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => safe.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(Error::refused(format!("{name} climbs out with \"..\"")));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::refused(format!("{name} is an absolute path")));
            }
        }
    }

    if safe.as_os_str().is_empty() {
        return Err(Error::refused(format!("{name} names nothing")));
    }
    Ok(safe)
}

/// Copies at most `limit` bytes, and refuses rather than truncating.
///
/// Truncating would be worse than refusing: a test file silently short by a
/// megabyte produces wrong verdicts that look like the participant's fault.
fn copy_bounded(from: &mut impl Read, to: &mut impl Write, limit: u64) -> Result<u64> {
    let mut reader = from.take(limit + 1);
    let copied = std::io::copy(&mut reader, to)?;
    if copied > limit {
        return Err(Error::refused(format!(
            "an entry unpacks to more than {limit} bytes"
        )));
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("aj-archive-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    /// Builds an archive from `(name, bytes)` pairs, writing the names verbatim
    /// so a test can put something hostile in one.
    fn archive_of(at: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
        std::fs::create_dir_all(at).unwrap();
        let path = at.join("package.zip");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        for (name, bytes) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    #[test]
    fn an_ordinary_package_unpacks() {
        let root = scratch("ordinary");
        let archive = archive_of(
            &root,
            &[
                ("config.yml", b"format: standard-io\n"),
                ("tests/1a.in", b"1 2\n"),
                ("tests/1a.out", b"3\n"),
                ("checker/checker.cpp", b"int main(){}\n"),
            ],
        );

        let into = root.join("unpacked");
        let written = extract(&archive, &into, &Limits::default()).unwrap();

        assert_eq!(written, 4);
        assert_eq!(std::fs::read(into.join("tests/1a.out")).unwrap(), b"3\n");
    }

    #[test]
    fn a_path_climbing_out_is_refused() {
        let root = scratch("traversal");
        let archive = archive_of(&root, &[("../../etc/passwd", b"root:x:0:0\n")]);

        let error = extract(&archive, &root.join("unpacked"), &Limits::default()).unwrap_err();

        assert!(matches!(error, Error::Refused(_)), "got {error}");
        assert!(!root.parent().unwrap().join("etc/passwd").exists());
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let root = scratch("absolute");
        let archive = archive_of(&root, &[("/etc/passwd", b"root\n")]);

        let error = extract(&archive, &root.join("unpacked"), &Limits::default()).unwrap_err();
        assert!(matches!(error, Error::Refused(_)), "got {error}");
    }

    #[test]
    fn a_name_outside_the_allow_list_is_refused() {
        let root = scratch("names");
        for hostile in [
            "tests/1a.in;rm -rf /",
            "tests/\u{202e}ni.a1",
            "te sts/1a.in",
        ] {
            let at = root.join(hostile.len().to_string());
            let archive = archive_of(&at, &[(hostile, b"x")]);
            let error = extract(&archive, &at.join("unpacked"), &Limits::default()).unwrap_err();
            assert!(matches!(error, Error::Refused(_)), "{hostile} gave {error}");
        }
    }

    /// The declared size is the attacker's opinion. This entry is small on
    /// paper and large in fact, and the cap is enforced on what arrives.
    #[test]
    fn an_entry_over_the_cap_is_refused_and_leaves_nothing_behind() {
        let root = scratch("bomb");
        let archive = archive_of(&root, &[("tests/1a.in", &vec![b'A'; 4 * 1024 * 1024])]);

        let into = root.join("unpacked");
        let error = extract(
            &archive,
            &into,
            &Limits {
                max_entry_bytes: 1024,
                ..Limits::default()
            },
        )
        .unwrap_err();

        assert!(matches!(error, Error::Refused(_)), "got {error}");
        assert!(
            !into.join("tests/1a.in").exists(),
            "a half-written entry is a corrupt one and must not survive the refusal",
        );
    }

    #[test]
    fn an_archive_that_expands_absurdly_is_refused() {
        let root = scratch("ratio");
        // Four megabytes of one byte value compresses to almost nothing.
        let archive = archive_of(&root, &[("tests/1a.in", &vec![0u8; 4 * 1024 * 1024])]);

        let error = extract(
            &archive,
            &root.join("unpacked"),
            &Limits {
                max_ratio: 10,
                ..Limits::default()
            },
        )
        .unwrap_err();

        assert!(matches!(error, Error::Refused(_)), "got {error}");
    }

    #[test]
    fn the_total_is_capped_even_when_each_entry_is_small() {
        let root = scratch("total");
        let entries: Vec<(String, Vec<u8>)> = (0..20)
            .map(|n| (format!("tests/{n}a.in"), vec![b'x'; 100 * 1024]))
            .collect();
        let borrowed: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let archive = archive_of(&root, &borrowed);

        let error = extract(
            &archive,
            &root.join("unpacked"),
            &Limits {
                max_total_bytes: 512 * 1024,
                max_ratio: u64::MAX,
                ..Limits::default()
            },
        )
        .unwrap_err();

        assert!(matches!(error, Error::Refused(_)), "got {error}");
    }

    #[test]
    fn unpacking_over_an_existing_directory_is_refused() {
        let root = scratch("existing");
        let archive = archive_of(&root, &[("config.yml", b"x")]);
        let into = root.join("unpacked");
        std::fs::create_dir_all(into.join("tests")).unwrap();
        std::fs::write(into.join("tests/9z.in"), b"left over from another job").unwrap();

        let error = extract(&archive, &into, &Limits::default()).unwrap_err();
        assert!(matches!(error, Error::Refused(_)), "got {error}");
    }

    #[test]
    fn too_many_entries_are_refused_before_anything_is_written() {
        let root = scratch("count");
        let entries: Vec<(String, Vec<u8>)> = (0..50)
            .map(|n| (format!("tests/{n}a.in"), vec![b'x']))
            .collect();
        let borrowed: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let archive = archive_of(&root, &borrowed);

        let into = root.join("unpacked");
        let error = extract(
            &archive,
            &into,
            &Limits {
                max_entries: 10,
                ..Limits::default()
            },
        )
        .unwrap_err();

        assert!(matches!(error, Error::Refused(_)), "got {error}");
        assert!(!into.exists(), "nothing should have been created");
    }
}
