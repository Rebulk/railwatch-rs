//! Verifies a release's `SHA256SUMS.sigstore.json` in-process, so `railwatch
//! upgrade` no longer needs cosign (issue #7). The same checks as
//! `cosign verify-blob` with the flags in SECURITY.md:
//!
//! - the signing certificate chains to Fulcio in Sigstore's public-good
//!   trusted root (the snapshot sigstore-trust-root embeds), and was valid
//!   at the time the transparency log and the timestamp authority vouch for;
//! - its SCT verifies against the root's CT log keys;
//! - the Rekor entry's inclusion proof and signed entry timestamp verify,
//!   and the entry is this signature over this SHA256SUMS;
//! - the signature verifies over SHA256SUMS;
//! - the SAN is exactly `https://github.com/<repo>/.github/workflows/
//!   release.yml@refs/tags/<tag>`, the OIDC issuer GitHub Actions', the
//!   workflow trigger `push` and the repository `<repo>`.
//!
//! Anything else fails. The crate's verifier is sigstore-verify (sigstore's
//! own Rust implementation); its default crypto backend is aws-lc-rs, which
//! would add a C/CMake build to every target, so this module plugs in the
//! signature algorithms the trust root uses from ring (already in the tree)
//! and, for ECDSA over a precomputed digest, which ring cannot do, the
//! RustCrypto p256/p384 crates.

use sigstore_crypto::{CryptoProvider, HashContext, SigningScheme};
use sigstore_types::{Bundle, HashAlgorithm};
use sigstore_verify::{VerificationPolicy, Verifier};

/// GitHub Actions' OIDC issuer.
pub const ISSUER: &str = "https://token.actions.githubusercontent.com";

/// Checks that `bundle` is a valid Sigstore signature of `sums` by `repo`'s
/// release workflow for `tag`, pushed (not a manual or PR run).
pub fn verify(sums: &[u8], bundle: &[u8], repo: &str, tag: &str) -> Result<(), String> {
    install_provider();
    let bundle: Bundle = serde_json::from_slice(bundle).map_err(|e| format!("SHA256SUMS.sigstore.json is not a Sigstore bundle: {e}"))?;
    let root = sigstore_trust_root::SigstoreInstance::PublicGood.embedded_trusted_root().map_err(|e| format!("Sigstore trusted root: {e}"))?;
    let identity = format!("https://github.com/{repo}/.github/workflows/release.yml@refs/tags/{tag}");
    let policy = VerificationPolicy::new(sigstore_verify::IdentityMatcher::Uri(identity.clone()), ISSUER);
    let verifier = Verifier::new(&root).map_err(|e| format!("Sigstore verifier: {e}"))?;
    let result = verifier.verify(sums, &bundle, &policy).map_err(|e| format!("SHA256SUMS signature does not verify for {identity}: {e}"))?;
    // Every check above, not a relaxed subset.
    if !(result.certificate_verified() && result.sct_verified() && result.tlog_verified() && result.identity_policy_checked()) {
        return Err("SHA256SUMS signature: verification was incomplete".into());
    }
    check_claims(&result.certificate().ok_or("SHA256SUMS signature: no certificate")?.ci_claims, repo)
}

/// cosign's --certificate-github-workflow-trigger and -repository: the
/// deprecated GitHub extensions (1.3.6.1.4.1.57264.1.2 and .1.5) and the
/// provider-neutral ones that replaced them (.1.20 and .1.12) must all say
/// a push to `repo`.
fn check_claims(claims: &sigstore_crypto::FulcioCiClaims, repo: &str) -> Result<(), String> {
    let github = &claims.deprecated_github;
    let repo_url = format!("https://github.com/{repo}");
    let expect = [
        ("workflow trigger", github.workflow_trigger.as_deref(), "push"),
        ("workflow repository", github.workflow_repository.as_deref(), repo),
        ("build trigger", claims.build_trigger.as_deref(), "push"),
        ("source repository", claims.source_repository_uri.as_deref(), repo_url.as_str()),
    ];
    for (what, got, want) in expect {
        if got != Some(want) {
            return Err(format!("SHA256SUMS signature: certificate {what} is {got:?}, expected {want:?}"));
        }
    }
    Ok(())
}

fn install_provider() {
    // Err: already installed (by an earlier call), which is this one.
    let _ = sigstore_crypto::install_default_provider(&RING);
}

static RING: RingProvider = RingProvider;

/// sigstore-crypto's backend from ring, p256 and p384: verification only.
#[derive(Debug)]
struct RingProvider;

struct Hasher(ring::digest::Context);

impl HashContext for Hasher {
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finalize(self: Box<Self>) -> Vec<u8> {
        self.0.finish().as_ref().to_vec()
    }
}

fn unsupported(scheme: SigningScheme) -> sigstore_crypto::Error {
    sigstore_crypto::Error::UnsupportedAlgorithm(format!("{} is not supported here", scheme.name()))
}

fn invalid(scheme: SigningScheme) -> sigstore_crypto::Error {
    sigstore_crypto::Error::Verification(format!("{} signature invalid", scheme.name()))
}

impl CryptoProvider for RingProvider {
    fn hash(&self, algorithm: HashAlgorithm) -> sigstore_crypto::Result<Box<dyn HashContext>> {
        let algorithm = match algorithm {
            HashAlgorithm::Sha2256 => &ring::digest::SHA256,
            HashAlgorithm::Sha2384 => &ring::digest::SHA384,
            HashAlgorithm::Sha2512 => &ring::digest::SHA512,
            other => return Err(sigstore_crypto::Error::UnsupportedAlgorithm(format!("cannot compute {other} digests"))),
        };
        Ok(Box::new(Hasher(ring::digest::Context::new(algorithm))))
    }

    fn verify_raw(&self, scheme: SigningScheme, key: &[u8], data: &[u8], signature: &[u8]) -> sigstore_crypto::Result<()> {
        use ring::signature as s;
        let algorithm: &dyn s::VerificationAlgorithm = match scheme {
            SigningScheme::EcdsaP256Sha256 => &s::ECDSA_P256_SHA256_ASN1,
            SigningScheme::EcdsaP256Sha384 => &s::ECDSA_P256_SHA384_ASN1,
            SigningScheme::EcdsaP384Sha256 => &s::ECDSA_P384_SHA256_ASN1,
            SigningScheme::EcdsaP384Sha384 => &s::ECDSA_P384_SHA384_ASN1,
            SigningScheme::Ed25519 => &s::ED25519,
            SigningScheme::RsaPkcs1Sha256 => &s::RSA_PKCS1_2048_8192_SHA256,
            SigningScheme::RsaPkcs1Sha384 => &s::RSA_PKCS1_2048_8192_SHA384,
            SigningScheme::RsaPkcs1Sha512 => &s::RSA_PKCS1_2048_8192_SHA512,
            SigningScheme::RsaPssSha256 => &s::RSA_PSS_2048_8192_SHA256,
            SigningScheme::RsaPssSha384 => &s::RSA_PSS_2048_8192_SHA384,
            SigningScheme::RsaPssSha512 => &s::RSA_PSS_2048_8192_SHA512,
            other => return Err(unsupported(other)),
        };
        s::UnparsedPublicKey::new(algorithm, key).verify(data, signature).map_err(|_| invalid(scheme))
    }

    /// ECDSA over a digest computed elsewhere: how a hashedrekord signature
    /// over SHA256SUMS is checked. ring has no prehashed ECDSA verify.
    fn verify_prehashed(&self, scheme: SigningScheme, key: &[u8], digest: &[u8], signature: &[u8]) -> sigstore_crypto::Result<()> {
        use p256::ecdsa::signature::hazmat::PrehashVerifier;
        let ok = match scheme {
            SigningScheme::EcdsaP256Sha256 | SigningScheme::EcdsaP256Sha384 => {
                let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(key).map_err(|_| invalid(scheme))?;
                let sig = p256::ecdsa::Signature::from_der(signature).map_err(|_| invalid(scheme))?;
                key.verify_prehash(digest, &sig).is_ok()
            }
            SigningScheme::EcdsaP384Sha256 | SigningScheme::EcdsaP384Sha384 => {
                let key = p384::ecdsa::VerifyingKey::from_sec1_bytes(key).map_err(|_| invalid(scheme))?;
                let sig = p384::ecdsa::Signature::from_der(signature).map_err(|_| invalid(scheme))?;
                key.verify_prehash(digest, &sig).is_ok()
            }
            other => return Err(unsupported(other)),
        };
        if ok { Ok(()) } else { Err(invalid(scheme)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "Rebulk/railwatch-rs";

    fn fixture(version: &str) -> (Vec<u8>, Vec<u8>) {
        let dir = format!("{}/tests/fixtures/release-{version}", env!("CARGO_MANIFEST_DIR"));
        (std::fs::read(format!("{dir}/SHA256SUMS")).unwrap(), std::fs::read(format!("{dir}/SHA256SUMS.sigstore.json")).unwrap())
    }

    /// The published v0.1.0 and v0.1.1 releases verify against their own
    /// tags with no cosign installed.
    #[test]
    fn real_releases_verify() {
        for v in ["v0.1.0", "v0.1.1"] {
            let (sums, bundle) = fixture(v);
            verify(&sums, &bundle, REPO, v).unwrap_or_else(|e| panic!("{v}: {e}"));
        }
    }

    #[test]
    fn a_changed_sha256sums_is_rejected() {
        let (mut sums, bundle) = fixture("v0.1.1");
        sums[0] ^= 1;
        let e = verify(&sums, &bundle, REPO, "v0.1.1").unwrap_err();
        assert!(e.contains("does not verify"), "{e}");
    }

    /// The identity is the release workflow at exactly this tag of exactly
    /// this repository: another tag (a replayed older release), another
    /// repository or another workflow is rejected.
    #[test]
    fn the_wrong_identity_is_rejected() {
        let (sums, bundle) = fixture("v0.1.1");
        for (repo, tag) in [(REPO, "v0.1.0"), (REPO, "v0.1.2"), ("Rebulk/railwatch-rs-fork", "v0.1.1"), ("rebulk/railwatch-rs", "v0.1.1")] {
            let e = verify(&sums, &bundle, repo, tag).unwrap_err();
            assert!(e.contains("does not verify"), "{repo} {tag}: {e}");
        }
    }

    fn json(bundle: &[u8]) -> serde_json::Value {
        serde_json::from_slice(bundle).unwrap()
    }

    /// Tampering with any part of the bundle: the signature, the
    /// certificate, the transparency-log proof.
    #[test]
    fn a_tampered_bundle_is_rejected() {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        let (sums, bundle) = fixture("v0.1.1");
        let flip = |v: &mut serde_json::Value| {
            let mut bytes = b64.decode(v.as_str().unwrap()).unwrap();
            let i = bytes.len() / 2;
            bytes[i] ^= 1;
            *v = serde_json::Value::String(b64.encode(bytes));
        };
        type Edit = fn(&mut serde_json::Value) -> &mut serde_json::Value;
        let edits: [(&str, Edit); 4] = [
            ("signature", |b| &mut b["messageSignature"]["signature"]),
            ("certificate", |b| &mut b["verificationMaterial"]["certificate"]["rawBytes"]),
            ("inclusion proof", |b| &mut b["verificationMaterial"]["tlogEntries"][0]["inclusionProof"]["hashes"][0]),
            ("signed entry timestamp", |b| &mut b["verificationMaterial"]["tlogEntries"][0]["inclusionPromise"]["signedEntryTimestamp"]),
        ];
        for (what, at) in edits {
            let mut b = json(&bundle);
            flip(at(&mut b));
            assert!(verify(&sums, &serde_json::to_vec(&b).unwrap(), REPO, "v0.1.1").is_err(), "a changed {what} is rejected");
            // Without the RFC 3161 timestamp (Rekor's time stands in for it),
            // so the timestamp's binding to the signature is not what
            // catches a changed signature.
            b["verificationMaterial"]["timestampVerificationData"] = serde_json::json!({});
            let e = verify(&sums, &serde_json::to_vec(&b).unwrap(), REPO, "v0.1.1").unwrap_err();
            assert!(!e.contains("timestamp"), "{what}: {e}");
        }
        let mut b = json(&bundle);
        b["verificationMaterial"]["timestampVerificationData"] = serde_json::json!({});
        verify(&sums, &serde_json::to_vec(&b).unwrap(), REPO, "v0.1.1").expect("the log's integrated time suffices, as with cosign");
        let mut b = json(&bundle);
        b["verificationMaterial"]["tlogEntries"] = serde_json::json!([]);
        assert!(verify(&sums, &serde_json::to_vec(&b).unwrap(), REPO, "v0.1.1").is_err(), "a bundle with no log entry is rejected");
        assert!(verify(&sums, b"{}", REPO, "v0.1.1").is_err());
        assert!(verify(&sums, b"not json", REPO, "v0.1.1").is_err());
    }

    /// The workflow-trigger and repository extensions are checked on the
    /// verified certificate. A real Fulcio certificate with another trigger
    /// cannot be made here, so this drives the same comparison with the
    /// claims parsed from the v0.1.1 certificate, changed one at a time.
    #[test]
    fn the_wrong_trigger_or_repository_extension_is_rejected() {
        use base64::Engine;
        let (_, bundle) = fixture("v0.1.1");
        let der = base64::engine::general_purpose::STANDARD.decode(json(&bundle)["verificationMaterial"]["certificate"]["rawBytes"].as_str().unwrap()).unwrap();
        let info = sigstore_crypto::parse_certificate_info(&sigstore_types::DerCertificate::new(der)).unwrap();
        let claims = info.ci_claims;
        assert_eq!(claims.deprecated_github.workflow_trigger.as_deref(), Some("push"));
        assert_eq!(claims.deprecated_github.workflow_repository.as_deref(), Some(REPO));
        assert_eq!(claims.build_trigger.as_deref(), Some("push"));
        assert_eq!(claims.source_repository_uri.as_deref(), Some("https://github.com/Rebulk/railwatch-rs"));
        assert!(check_claims(&claims, REPO).is_ok());
        let mut c = claims.clone();
        c.deprecated_github.workflow_trigger = Some("workflow_dispatch".into());
        assert!(check_claims(&c, REPO).unwrap_err().contains("workflow trigger"));
        let mut c = claims.clone();
        c.build_trigger = Some("pull_request_target".into());
        assert!(check_claims(&c, REPO).unwrap_err().contains("build trigger"));
        let mut c = claims.clone();
        c.deprecated_github.workflow_repository = Some("Someone/else".into());
        assert!(check_claims(&c, REPO).unwrap_err().contains("workflow repository"));
        let mut c = claims.clone();
        c.source_repository_uri = None;
        assert!(check_claims(&c, REPO).unwrap_err().contains("source repository"), "a missing claim is a mismatch");
    }
}
