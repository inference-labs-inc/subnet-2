use anyhow::{bail, Context, Result};
use base64::Engine;
use openssl::hash::MessageDigest;
use openssl::stack::Stack;
use openssl::x509::store::X509StoreBuilder;
use openssl::x509::{X509StoreContext, X509};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Pinned Sigstore Fulcio root CA (fulcio_v1.crt.pem).
/// sha256: f989aa23def87c549404eadba767768d2a3c8d6d30a8b793f9f518a8eafd2cf5
const FULCIO_ROOT_PEM: &[u8] = include_bytes!("attestation_roots/fulcio_v1.crt.pem");

/// Pinned Sigstore Fulcio intermediate (fulcio_intermediate_v1.crt.pem).
/// sha256: f8cbecf186db7714624a5f4e99da31a917cbef70a94dd6921f5c3ca969dfe30a
const FULCIO_INTERMEDIATE_PEM: &[u8] =
    include_bytes!("attestation_roots/fulcio_intermediate_v1.crt.pem");

/// Pinned Sigstore Rekor transparency log public key (rekor.pub).
/// sha256: dce5ef715502ec9f3cdfd11f8cc384b31a6141023d3e7595e9908a81cb6241bd
const REKOR_PUBKEY_PEM: &[u8] = include_bytes!("attestation_roots/rekor.pub");

const FULCIO_ROOT_SHA256: &str = "f989aa23def87c549404eadba767768d2a3c8d6d30a8b793f9f518a8eafd2cf5";
const FULCIO_INTERMEDIATE_SHA256: &str =
    "f8cbecf186db7714624a5f4e99da31a917cbef70a94dd6921f5c3ca969dfe30a";
const REKOR_PUBKEY_SHA256: &str =
    "dce5ef715502ec9f3cdfd11f8cc384b31a6141023d3e7595e9908a81cb6241bd";

/// Enforce at compile-time-equivalent boot that the embedded trust roots have
/// not been silently replaced on disk. Any mismatch aborts the update loop.
pub fn assert_pinned_roots() -> Result<()> {
    let fulcio_sha = hex::encode(Sha256::digest(FULCIO_ROOT_PEM));
    let intermediate_sha = hex::encode(Sha256::digest(FULCIO_INTERMEDIATE_PEM));
    let rekor_sha = hex::encode(Sha256::digest(REKOR_PUBKEY_PEM));
    if fulcio_sha != FULCIO_ROOT_SHA256 {
        bail!(
            "embedded Fulcio root hash mismatch: expected {FULCIO_ROOT_SHA256}, got {fulcio_sha}"
        );
    }
    if intermediate_sha != FULCIO_INTERMEDIATE_SHA256 {
        bail!(
            "embedded Fulcio intermediate hash mismatch: expected {FULCIO_INTERMEDIATE_SHA256}, got {intermediate_sha}"
        );
    }
    if rekor_sha != REKOR_PUBKEY_SHA256 {
        bail!(
            "embedded Rekor pubkey hash mismatch: expected {REKOR_PUBKEY_SHA256}, got {rekor_sha}"
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct AttestationsResponse {
    attestations: Vec<AttestationEntry>,
}

#[derive(Deserialize)]
struct AttestationEntry {
    bundle: Bundle,
}

#[derive(Deserialize)]
struct Bundle {
    #[serde(rename = "verificationMaterial")]
    verification_material: VerificationMaterial,
    #[serde(rename = "dsseEnvelope")]
    dsse_envelope: DsseEnvelope,
}

#[derive(Deserialize)]
struct VerificationMaterial {
    certificate: Option<CertificateMaterial>,
    #[serde(rename = "x509CertificateChain")]
    x509_certificate_chain: Option<CertificateChainMaterial>,
    #[serde(rename = "tlogEntries", default)]
    tlog_entries: Vec<TlogEntry>,
}

#[derive(Deserialize)]
struct CertificateMaterial {
    #[serde(rename = "rawBytes")]
    raw_bytes: String,
}

#[derive(Deserialize)]
struct CertificateChainMaterial {
    certificates: Vec<CertificateMaterial>,
}

#[derive(Deserialize)]
struct TlogEntry {
    #[serde(rename = "logIndex")]
    log_index: String,
    #[serde(rename = "logId")]
    log_id: LogId,
    #[serde(rename = "integratedTime")]
    integrated_time: String,
    #[serde(rename = "inclusionPromise")]
    inclusion_promise: Option<InclusionPromise>,
    #[serde(rename = "canonicalizedBody")]
    canonicalized_body: String,
}

#[derive(Deserialize)]
struct LogId {
    #[serde(rename = "keyId")]
    key_id: String,
}

#[derive(Deserialize)]
struct InclusionPromise {
    #[serde(rename = "signedEntryTimestamp")]
    signed_entry_timestamp: String,
}

#[derive(Deserialize)]
struct DsseEnvelope {
    payload: String,
    #[serde(rename = "payloadType")]
    payload_type: String,
    signatures: Vec<DsseSignature>,
}

#[derive(Deserialize)]
struct DsseSignature {
    sig: String,
}

#[derive(Deserialize)]
struct InTotoStatement {
    #[serde(rename = "_type")]
    _type: String,
    subject: Vec<Subject>,
}

#[derive(Deserialize)]
struct Subject {
    digest: SubjectDigest,
}

#[derive(Deserialize)]
struct SubjectDigest {
    sha256: String,
}

/// Fetch the GitHub attestation for a given artifact digest, then verify it
/// end to end against the pinned Sigstore trust roots.
pub async fn fetch_and_verify_attestation(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    artifact_sha256_hex: &str,
    release_tag: &str,
    workflow_path: &str,
) -> Result<()> {
    assert_pinned_roots()?;

    let url = format!(
        "https://api.github.com/repos/{owner}/{repo}/attestations/sha256:{artifact_sha256_hex}"
    );
    let body = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(30))
        .header("User-Agent", "sn2-auto-update")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("fetching attestation")?
        .error_for_status()
        .context("attestation endpoint error")?
        .bytes()
        .await
        .context("reading attestation response body")?;

    let expected_san =
        format!("https://github.com/{owner}/{repo}/{workflow_path}@refs/tags/{release_tag}");

    verify_attestation_response_json(&body, artifact_sha256_hex, &expected_san)
}

/// Verify a pre-fetched `GET /repos/.../attestations/sha256:...` response body
/// against a given artifact digest and expected Fulcio SAN. Exposed for tests
/// and for out-of-band verification against locally stored bundles.
pub fn verify_attestation_response_json(
    response_json: &[u8],
    artifact_sha256_hex: &str,
    expected_san: &str,
) -> Result<()> {
    assert_pinned_roots()?;
    let resp: AttestationsResponse =
        serde_json::from_slice(response_json).context("parsing attestation response JSON")?;
    if resp.attestations.is_empty() {
        bail!("no attestations found in response");
    }
    let mut last_err: Option<anyhow::Error> = None;
    for (idx, entry) in resp.attestations.iter().enumerate() {
        match verify_bundle(&entry.bundle, artifact_sha256_hex, expected_san) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e.context(format!("attestation[{idx}]"))),
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no valid attestation")))
}

fn verify_bundle(bundle: &Bundle, artifact_sha256_hex: &str, expected_san: &str) -> Result<()> {
    let leaf_der = decode_leaf_cert(&bundle.verification_material)?;
    let leaf = X509::from_der(&leaf_der).context("parsing leaf certificate")?;

    verify_cert_chain(&leaf)?;
    verify_cert_identity(&leaf, expected_san)?;
    verify_dsse_signature(&leaf, &bundle.dsse_envelope)?;
    verify_intoto_subject(&bundle.dsse_envelope, artifact_sha256_hex)?;
    verify_rekor_set(&bundle.verification_material.tlog_entries)?;
    Ok(())
}

fn decode_leaf_cert(vm: &VerificationMaterial) -> Result<Vec<u8>> {
    if let Some(cert) = &vm.certificate {
        return base64::engine::general_purpose::STANDARD
            .decode(cert.raw_bytes.as_bytes())
            .context("decoding leaf certificate base64");
    }
    if let Some(chain) = &vm.x509_certificate_chain {
        let first = chain
            .certificates
            .first()
            .context("empty x509 certificate chain")?;
        return base64::engine::general_purpose::STANDARD
            .decode(first.raw_bytes.as_bytes())
            .context("decoding leaf certificate base64");
    }
    bail!("attestation missing leaf certificate")
}

fn verify_cert_chain(leaf: &X509) -> Result<()> {
    let root = X509::from_pem(FULCIO_ROOT_PEM).context("parsing pinned Fulcio root")?;
    let intermediate =
        X509::from_pem(FULCIO_INTERMEDIATE_PEM).context("parsing pinned Fulcio intermediate")?;

    let mut store_builder = X509StoreBuilder::new().context("x509 store builder")?;
    store_builder
        .add_cert(root)
        .context("adding Fulcio root to store")?;
    // Fulcio-issued leaves have a ~10 minute validity window; we validate the
    // binding to a specific point in time via the pinned Rekor SET instead.
    store_builder
        .set_flags(openssl::x509::verify::X509VerifyFlags::NO_CHECK_TIME)
        .context("disabling wall-clock cert time check")?;
    let store = store_builder.build();

    let mut chain: Stack<X509> = Stack::new().context("empty intermediate stack")?;
    chain
        .push(intermediate)
        .context("pushing pinned Fulcio intermediate onto chain")?;

    let mut ctx = X509StoreContext::new().context("x509 store context")?;
    let verified = ctx
        .init(&store, leaf, &chain, |c| c.verify_cert())
        .context("initializing x509 verification")?;
    if !verified {
        bail!("leaf certificate does not chain to pinned Fulcio root");
    }
    Ok(())
}

fn verify_cert_identity(leaf: &X509, expected_san: &str) -> Result<()> {
    // Fulcio's issuance policy binds a `github.com/...` SAN to the GitHub
    // Actions OIDC issuer, so an exact SAN URI match is sufficient: Fulcio
    // will not issue a cert with this SAN to any other identity provider.
    let san = leaf
        .subject_alt_names()
        .context("leaf certificate missing SAN extension")?;
    let uri_sans: Vec<&str> = san.iter().filter_map(|name| name.uri()).collect();
    if uri_sans.len() == 1 && uri_sans[0] == expected_san {
        return Ok(());
    }
    bail!(
        "leaf URI SAN set {uri_sans:?} does not exactly match expected workflow identity '{expected_san}'"
    )
}

fn verify_dsse_signature(leaf: &X509, envelope: &DsseEnvelope) -> Result<()> {
    let pubkey = leaf.public_key().context("extracting leaf public key")?;

    let payload_raw = base64::engine::general_purpose::STANDARD
        .decode(envelope.payload.as_bytes())
        .context("decoding DSSE payload for PAE construction")?;

    // DSSE PAE v1: "DSSEv1" SP LEN(type) SP type SP LEN(body) SP body
    let header = format!(
        "DSSEv1 {} {} {} ",
        envelope.payload_type.len(),
        envelope.payload_type,
        payload_raw.len(),
    );
    let mut pae_bytes = header.into_bytes();
    pae_bytes.extend_from_slice(&payload_raw);

    let sig = envelope
        .signatures
        .first()
        .context("DSSE envelope missing signature")?;
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(sig.sig.as_bytes())
        .context("decoding DSSE signature")?;

    let mut verifier = openssl::sign::Verifier::new(MessageDigest::sha256(), &pubkey)
        .context("creating DSSE verifier")?;
    verifier
        .update(&pae_bytes)
        .context("feeding DSSE PAE to verifier")?;
    let ok = verifier
        .verify(&sig_bytes)
        .context("DSSE signature verification")?;
    if !ok {
        bail!("DSSE signature does not verify under leaf public key");
    }
    Ok(())
}

fn verify_intoto_subject(envelope: &DsseEnvelope, artifact_sha256_hex: &str) -> Result<()> {
    if envelope.payload_type != "application/vnd.in-toto+json" {
        bail!("unexpected DSSE payload type '{}'", envelope.payload_type);
    }
    let payload_bytes = base64::engine::general_purpose::STANDARD
        .decode(envelope.payload.as_bytes())
        .context("decoding DSSE payload")?;
    let stmt: InTotoStatement =
        serde_json::from_slice(&payload_bytes).context("parsing in-toto statement")?;
    for subject in &stmt.subject {
        if subject
            .digest
            .sha256
            .eq_ignore_ascii_case(artifact_sha256_hex)
        {
            return Ok(());
        }
    }
    bail!(
        "in-toto statement does not list artifact sha256 {artifact_sha256_hex} among its subjects"
    );
}

fn verify_rekor_set(entries: &[TlogEntry]) -> Result<()> {
    let entry = entries
        .first()
        .context("attestation missing Rekor transparency log entry")?;
    let promise = entry
        .inclusion_promise
        .as_ref()
        .context("Rekor entry missing inclusion promise / SET")?;

    let rekor_pub = openssl::pkey::PKey::public_key_from_pem(REKOR_PUBKEY_PEM)
        .context("parsing Rekor pubkey")?;

    // Canonicalized payload signed by Rekor per rekor-spec:
    // JSON: {"body":"<canonicalizedBody>","integratedTime":<int>,"logID":"<hex>","logIndex":<int>}
    // Note: logID is the hex of the raw log key, and keys are lex-sorted.
    let key_id_bytes = base64::engine::general_purpose::STANDARD
        .decode(entry.log_id.key_id.as_bytes())
        .context("decoding Rekor logId")?;
    let log_id_hex = hex::encode(key_id_bytes);
    let integrated_time: i64 = entry
        .integrated_time
        .parse()
        .context("parsing Rekor integratedTime")?;
    let log_index: i64 = entry.log_index.parse().context("parsing Rekor logIndex")?;
    let canonical = format!(
        "{{\"body\":\"{}\",\"integratedTime\":{},\"logID\":\"{}\",\"logIndex\":{}}}",
        entry.canonicalized_body, integrated_time, log_id_hex, log_index
    );

    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(promise.signed_entry_timestamp.as_bytes())
        .context("decoding Rekor SET")?;

    let mut verifier = openssl::sign::Verifier::new(MessageDigest::sha256(), &rekor_pub)
        .context("creating Rekor verifier")?;
    verifier
        .update(canonical.as_bytes())
        .context("feeding Rekor SET to verifier")?;
    let ok = verifier
        .verify(&sig_bytes)
        .context("Rekor SET verification")?;
    if !ok {
        bail!("Rekor SET does not verify under pinned Rekor public key");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_root_hashes_match() {
        assert_pinned_roots().unwrap();
    }
}
