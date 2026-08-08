//! The one secret this Runner has.
//!
//! Generated once, at first start, and **never rotated** — that is a decision,
//! not an omission. Revocation is permanent, so a leaked key means a new
//! configuration, a new key, a new registration and a new approval. Making it
//! immutable is what gives revocation its meaning: there is no way to quietly
//! replace the key behind a fingerprint an administrator already trusted.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use rand_core::OsRng;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

const SECRET_LEN: usize = 32;

pub struct Identity {
    key: SigningKey,
    path: PathBuf,
}

/// Written by hand rather than derived, and that is the point.
///
/// `#[derive(Debug)]` here would put the **private key** into any log line,
/// panic message or `assert_eq!` failure that touched an `Identity`. The
/// fingerprint identifies the Runner without disclosing anything: it is what
/// the Server publishes in its own panel.
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint())
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// Reads the key, or makes one and writes it down.
    ///
    /// The two cases are deliberately not distinguished to the caller: a Runner
    /// starting for the first time and one restarting behave identically from
    /// here on, and the Server tells them apart by whether the fingerprint is
    /// already approved.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        if path.exists() {
            let bytes = std::fs::read(&path).map_err(at(&path, "read"))?;
            let secret: [u8; SECRET_LEN] =
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::MalformedKey {
                        path: path.display().to_string(),
                        actual: bytes.len(),
                    })?;
            tracing::debug!(path = %path.display(), "identity read");
            return Ok(Self {
                key: SigningKey::from_bytes(&secret),
                path,
            });
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(at(parent, "created"))?;
        }

        let key = SigningKey::generate(&mut OsRng);

        // Written under a temporary name and renamed, so an interrupted first
        // start leaves either no key or a whole one. A half-written key would
        // be read as malformed on the next start and the Runner would be stuck
        // behind an identity nobody can approve.
        let temporary = path.with_extension("partial");
        std::fs::write(&temporary, key.to_bytes()).map_err(at(&temporary, "written"))?;
        restrict(&temporary)?;
        std::fs::rename(&temporary, &path).map_err(at(&path, "written"))?;

        tracing::info!(path = %path.display(), "identity generated");
        Ok(Self { key, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The raw 32 bytes, base64 — what the Server stores and hashes.
    pub fn public_key(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.key.verifying_key().to_bytes())
    }

    /// SHA-256 of those same raw bytes, lowercase hex.
    ///
    /// Computed here as well as by the Server so that registration can be
    /// checked rather than believed: if the two disagree, something re-encoded
    /// the key on the way and every later signature would fail for a reason
    /// nothing would explain.
    pub fn fingerprint(&self) -> String {
        hex::encode(Sha256::digest(self.key.verifying_key().to_bytes()))
    }

    /// Ed25519 over the **UTF-8 bytes of the nonce**, base64.
    ///
    /// Not over a hash of it, not over a JSON document containing it, and not
    /// over the base64 text decoded first. The nonce arrives as a string and is
    /// signed as that string's bytes.
    pub fn sign(&self, nonce: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.key.sign(nonce.as_bytes()).to_bytes())
    }
}

/// Names the file an I/O failure was about.
fn at(path: &Path, doing: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.display().to_string();
    move |source| Error::Storage {
        path,
        doing,
        source,
    }
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// No-op away from Unix. The Runner is a Linux component; this exists so the
/// crate still compiles for anyone reading it on another machine, and it is not
/// a claim that the key is protected there.
#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("aj-identity-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path.push("identity.key");
        path
    }

    #[test]
    fn a_key_survives_a_restart() {
        let path = scratch("restart");
        let first = Identity::load_or_create(&path).unwrap();
        let second = Identity::load_or_create(&path).unwrap();

        assert_eq!(first.public_key(), second.public_key());
        assert_eq!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn the_fingerprint_is_the_hash_of_the_key_the_server_is_given() {
        let identity = Identity::load_or_create(scratch("fingerprint")).unwrap();

        let sent = base64::engine::general_purpose::STANDARD
            .decode(identity.public_key())
            .unwrap();
        assert_eq!(sent.len(), 32, "an SPKI wrapper would be 44 and be refused");
        assert_eq!(identity.fingerprint(), hex::encode(Sha256::digest(&sent)));
    }

    #[test]
    fn a_signature_verifies_against_the_key_that_was_published() {
        use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};

        let identity = Identity::load_or_create(scratch("signature")).unwrap();
        let nonce = "PT5A6oWQ0vJ0kQ2Y3n1a4Z9x8W7v6U5t4S3r2Q1p0O8=";

        let published: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(identity.public_key())
            .unwrap()
            .try_into()
            .unwrap();
        let signature: [u8; 64] = base64::engine::general_purpose::STANDARD
            .decode(identity.sign(nonce))
            .unwrap()
            .try_into()
            .unwrap();

        VerifyingKey::from_bytes(&published)
            .unwrap()
            .verify(nonce.as_bytes(), &Signature::from_bytes(&signature))
            .expect("the Server verifies exactly this, over exactly these bytes");
    }

    #[test]
    fn a_truncated_key_is_refused_rather_than_padded() {
        let path = scratch("truncated");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, [7u8; 16]).unwrap();

        let error = Identity::load_or_create(&path).unwrap_err();
        assert!(matches!(error, Error::MalformedKey { actual: 16, .. }));
    }
}
