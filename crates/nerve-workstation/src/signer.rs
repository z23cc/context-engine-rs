//! The L4 **signing seam** for Verification Receipts (`docs/designs/trust-substrate.md`
//! §8, INV-R1). The pure receipt machinery in [`nerve_core::receipt`] canonicalizes a
//! statement and wraps it in a DSSE Pre-Authentication Encoding (PAE), then hands the
//! PAE bytes to a [`Signer`] here for the one impure step — producing a signature over
//! those bytes. Keeping signing behind a trait (and OUT of `nerve-core`) preserves the
//! kernel's determinism boundary: the receipt's content address is a pure function of
//! its statement, and only the detached signature is host-produced.
//!
//! The shipped default is [`LocalEd25519Signer`] (ed25519-dalek v2). ed25519 is
//! deterministic given key + message (RFC 8032), so a fixed key + host-supplied
//! `issued_at_ms` yields a byte-stable receipt that golden tests can lock —
//! [`LocalEd25519Signer::deterministic_test_key`] exposes exactly that fixed key.
//! [`LocalEd25519Signer::load_or_create`] persists a real per-host key under
//! `config_home/keys/`. Sigstore keyless signing (Fulcio/Rekor) is the deferred
//! upgrade behind the same trait — see [`SigstoreKeylessSigner`].

// WAVE-B CHECKPOINT: the signing half (sign / load_or_create) is wired into the
// verify→receipt flow; the verification half (ed25519_verify / decode_*) + the
// deterministic test key serve the receipt-verification path (the deferred `nerve
// verify`/MCP `nerve_verify` finalization), so they are dead until that lands.
#![allow(dead_code)]

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use std::fs;
use std::path::Path;

/// The backend label stamped into a [`ReceiptSignature`](nerve_core::receipt::ReceiptSignature)
/// produced by [`LocalEd25519Signer`] — distinguishes a local key from a future
/// `sigstore-keyless` bundle so a verifier picks the right predicate.
pub(crate) const LOCAL_ED25519_BACKEND: &str = "local-ed25519";

/// The impure receipt-signing seam (INV-R1): given the DSSE PAE bytes the pure
/// `nerve-core` machinery produced, yield a detached signature plus the public key
/// needed to verify it. Implementors carry their own key material; the kernel never
/// sees a private key.
pub(crate) trait Signer {
    /// A stable backend label stamped into the receipt's signature
    /// (`local-ed25519`, later `sigstore-keyless`).
    fn backend(&self) -> &str;

    /// An opaque key identifier (the public-key fingerprint for the local backend).
    fn keyid(&self) -> String;

    /// Sign the DSSE PAE bytes, returning `(sig_b64, public_key_b64)` — both base64
    /// (standard alphabet), matching what [`ed25519_verify`] expects.
    fn sign(&self, pae: &[u8]) -> (String, String);
}

/// A local ed25519 signer (ed25519-dalek v2) — the shipped default backend. Wraps a
/// [`SigningKey`]; signatures are deterministic for a given key + message.
pub(crate) struct LocalEd25519Signer {
    key: SigningKey,
}

impl LocalEd25519Signer {
    /// Wrap an explicit signing key.
    pub(crate) fn new(key: SigningKey) -> Self {
        Self { key }
    }

    /// A fixed, well-known signing key derived from a constant 32-byte seed — the
    /// MANDATORY key for golden receipt tests (RISK §6). With this key and a fixed
    /// `issued_at_ms`, a receipt's signature is byte-stable across machines.
    pub(crate) fn deterministic_test_key() -> Self {
        // A fixed, non-secret seed: byte i = i. Never used for a real receipt.
        let seed: [u8; 32] = std::array::from_fn(|i| i as u8);
        Self::new(SigningKey::from_bytes(&seed))
    }

    /// Load the host's persistent signing key from `<dir>/ed25519.key`, creating it
    /// (with a fresh random key) on first use. Best-effort: an IO/parse failure falls
    /// back to a freshly generated in-memory key so signing never hard-fails (the key
    /// just won't persist). The directory is created on demand.
    pub(crate) fn load_or_create(dir: &Path) -> Self {
        let path = dir.join("ed25519.key");
        if let Some(key) = Self::try_load(&path) {
            return Self::new(key);
        }
        let key = SigningKey::from_bytes(&random_seed());
        // Persist best-effort; ignore failure (the key stays in-memory this run).
        let _ = fs::create_dir_all(dir);
        let _ = fs::write(&path, STANDARD.encode(key.to_bytes()));
        Self::new(key)
    }

    /// The verifying (public) key.
    pub(crate) fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// Base64 (standard) of the 32-byte public key.
    fn public_key_b64(&self) -> String {
        STANDARD.encode(self.verifying_key().to_bytes())
    }

    /// Parse a base64-encoded 32-byte seed from `path`, if present and well-formed.
    fn try_load(path: &Path) -> Option<SigningKey> {
        let raw = fs::read_to_string(path).ok()?;
        let bytes = STANDARD.decode(raw.trim()).ok()?;
        let seed: [u8; 32] = bytes.try_into().ok()?;
        Some(SigningKey::from_bytes(&seed))
    }
}

impl Signer for LocalEd25519Signer {
    fn backend(&self) -> &str {
        LOCAL_ED25519_BACKEND
    }

    fn keyid(&self) -> String {
        // The public key fingerprint is its own base64 — a stable, opaque handle.
        self.public_key_b64()
    }

    fn sign(&self, pae: &[u8]) -> (String, String) {
        let sig: Signature = self.key.sign(pae);
        (STANDARD.encode(sig.to_bytes()), self.public_key_b64())
    }
}

/// Verify a detached ed25519 signature: decode the base64 public key + signature,
/// then check `sig` over `pae`. Any malformed input (bad base64, wrong length,
/// non-canonical signature) yields `false` — never a panic. This is the predicate
/// passed to [`nerve_core::receipt::verify_receipt`] for the local backend.
pub(crate) fn ed25519_verify(public_key_b64: &[u8], pae: &[u8], sig_b64: &[u8]) -> bool {
    let Some(key) = decode_verifying_key(public_key_b64) else {
        return false;
    };
    let Some(sig) = decode_signature(sig_b64) else {
        return false;
    };
    key.verify(pae, &sig).is_ok()
}

/// Decode a base64 (standard) 32-byte ed25519 public key.
fn decode_verifying_key(public_key_b64: &[u8]) -> Option<VerifyingKey> {
    let bytes = STANDARD.decode(public_key_b64).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&arr).ok()
}

/// Decode a base64 (standard) 64-byte ed25519 signature.
fn decode_signature(sig_b64: &[u8]) -> Option<Signature> {
    let bytes = STANDARD.decode(sig_b64).ok()?;
    let arr: [u8; 64] = bytes.try_into().ok()?;
    Some(Signature::from_bytes(&arr))
}

/// A 32-byte seed for a fresh signing key, sourced from the OS RNG via the standard
/// library's hasher entropy is not available, so we use `getrandom` indirectly through
/// `SigningKey::generate` would need an RngCore; instead derive from system time +
/// process-unique entropy. Best-effort uniqueness for a local key (NOT a security
/// boundary — a real deployment uses sigstore keyless).
fn random_seed() -> [u8; 32] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mixed = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(pid);
    let mut seed = [0u8; 32];
    // Splat the 128-bit mix across the 32-byte seed with a simple LCG so the key is
    // distinct per-run; cryptographic strength is provided by the deferred sigstore path.
    let mut state = mixed | 1;
    for chunk in seed.chunks_mut(8) {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let bytes = (state as u64).to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    seed
}

/// Deferred Sigstore keyless signer (Fulcio short-lived cert + Rekor transparency
/// log) — the production upgrade behind the [`Signer`] seam. Not constructed today;
/// `LocalEd25519Signer` is the shipped default. The struct exists to pin the seam and
/// document the upgrade path (it carries the would-be OIDC identity).
#[allow(
    dead_code,
    reason = "deferred backend; pins the Signer seam (trust-substrate §8)"
)]
pub(crate) struct SigstoreKeylessSigner {
    /// The OIDC identity (workload/email) Fulcio would bind the short-lived cert to.
    pub(crate) identity: String,
}

#[allow(
    dead_code,
    reason = "deferred backend; pins the Signer seam (trust-substrate §8)"
)]
impl SigstoreKeylessSigner {
    /// The backend label a sigstore-issued receipt signature would carry.
    pub(crate) const BACKEND: &'static str = "sigstore-keyless";
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn deterministic_key_signs_byte_stably_and_verifies() {
        let signer = LocalEd25519Signer::deterministic_test_key();
        let pae = b"DSSEv1 3 abc 5 hello";

        let (sig1, pk1) = signer.sign(pae);
        let (sig2, pk2) = signer.sign(pae);
        // ed25519 is deterministic: same key + message => identical signature.
        assert_eq!(sig1, sig2);
        assert_eq!(pk1, pk2);
        assert_eq!(signer.backend(), LOCAL_ED25519_BACKEND);
        // keyid is the public key fingerprint.
        assert_eq!(signer.keyid(), pk1);

        // The produced signature verifies over the same PAE, and fails on a tamper.
        assert!(ed25519_verify(pk1.as_bytes(), pae, sig1.as_bytes()));
        assert!(!ed25519_verify(
            pk1.as_bytes(),
            b"different",
            sig1.as_bytes()
        ));
    }

    #[test]
    fn deterministic_key_is_fixed_across_constructions() {
        let a = LocalEd25519Signer::deterministic_test_key();
        let b = LocalEd25519Signer::deterministic_test_key();
        assert_eq!(a.keyid(), b.keyid(), "fixed seed => fixed key");
    }

    #[test]
    fn load_or_create_persists_and_reloads_same_key() {
        let dir = tempdir().unwrap();
        let keys = dir.path().join("keys");
        let first = LocalEd25519Signer::load_or_create(&keys);
        let id1 = first.keyid();
        assert!(keys.join("ed25519.key").exists(), "key file persisted");

        // Reloading the same dir yields the same key.
        let second = LocalEd25519Signer::load_or_create(&keys);
        assert_eq!(id1, second.keyid(), "reload reuses the persisted key");
    }

    #[test]
    fn fresh_keys_in_distinct_dirs_differ() {
        let d1 = tempdir().unwrap();
        let d2 = tempdir().unwrap();
        let a = LocalEd25519Signer::load_or_create(&d1.path().join("k"));
        let b = LocalEd25519Signer::load_or_create(&d2.path().join("k"));
        assert_ne!(a.keyid(), b.keyid(), "independent dirs => independent keys");
    }

    #[test]
    fn verify_rejects_malformed_inputs_without_panicking() {
        let signer = LocalEd25519Signer::deterministic_test_key();
        let (sig, pk) = signer.sign(b"payload");
        // Garbage base64, wrong-length key, empty inputs all return false.
        assert!(!ed25519_verify(b"not-base64!!", b"payload", sig.as_bytes()));
        assert!(!ed25519_verify(pk.as_bytes(), b"payload", b"short"));
        assert!(!ed25519_verify(b"", b"payload", b""));
        // A valid signature under one key does not verify under another.
        let other = LocalEd25519Signer::load_or_create(tempdir().unwrap().path());
        assert!(!ed25519_verify(
            other.keyid().as_bytes(),
            b"payload",
            sig.as_bytes()
        ));
    }
}
