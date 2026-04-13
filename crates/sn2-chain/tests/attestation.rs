use sn2_chain::attestation::verify_attestation_response_json;

const REAL_ATTESTATION: &[u8] = include_bytes!("fixtures/attestation_14.6.0.json");
const REAL_ARTIFACT_SHA256: &str =
    "42006a7ab79bff73657d83c52bd2461bee54004a0a9c4ea16a65d029a9f62755";
const REAL_EXPECTED_SAN: &str =
    "https://github.com/inference-labs-inc/subnet-2/.github/workflows/release.yml@refs/tags/14.6.0";

#[test]
fn real_attestation_verifies_under_pinned_roots() {
    verify_attestation_response_json(REAL_ATTESTATION, REAL_ARTIFACT_SHA256, REAL_EXPECTED_SAN)
        .expect("real 14.6.0 attestation should verify against pinned Sigstore roots");
}

#[test]
fn verification_rejects_artifact_digest_substitution() {
    let tampered_digest = "0000000000000000000000000000000000000000000000000000000000000000";
    let err =
        verify_attestation_response_json(REAL_ATTESTATION, tampered_digest, REAL_EXPECTED_SAN)
            .expect_err("different artifact digest must not verify");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("subjects") || msg.contains("in-toto"),
        "unexpected error surface: {msg}"
    );
}

#[test]
fn verification_rejects_wrong_workflow_identity() {
    // Swap the tag so the SAN no longer matches the issued Fulcio cert.
    let wrong_san = "https://github.com/inference-labs-inc/subnet-2/.github/workflows/release.yml@refs/tags/99.99.99";
    let err = verify_attestation_response_json(REAL_ATTESTATION, REAL_ARTIFACT_SHA256, wrong_san)
        .expect_err("mismatched SAN must not verify");
    let msg = format!("{err:#}");
    assert!(msg.contains("SAN"), "unexpected error surface: {msg}");
}

#[test]
fn verification_rejects_dsse_signature_tampering() {
    // Flip one byte inside every base64 DSSE signature field so neither
    // attestation in the bundle can verify.
    let corrupted = tamper_all(REAL_ATTESTATION, br#""sig":""#);
    let err = verify_attestation_response_json(&corrupted, REAL_ARTIFACT_SHA256, REAL_EXPECTED_SAN)
        .expect_err("DSSE signature tampering must not verify");
    let msg = format!("{err:#}");
    assert!(
        msg.to_lowercase().contains("dsse") || msg.to_lowercase().contains("signature"),
        "unexpected error surface: {msg}"
    );
}

#[test]
fn verification_rejects_rekor_set_tampering() {
    let corrupted = tamper_all(REAL_ATTESTATION, br#""signedEntryTimestamp":""#);
    let err = verify_attestation_response_json(&corrupted, REAL_ARTIFACT_SHA256, REAL_EXPECTED_SAN)
        .expect_err("Rekor SET tampering must not verify");
    let msg = format!("{err:#}");
    assert!(
        msg.to_lowercase().contains("rekor") || msg.to_lowercase().contains("set"),
        "unexpected error surface: {msg}"
    );
}

fn tamper_all(haystack: &[u8], needle: &[u8]) -> Vec<u8> {
    let mut out = haystack.to_vec();
    let mut offset = 0;
    while let Some(pos) = out[offset..]
        .windows(needle.len())
        .position(|w| w == needle)
    {
        let target = offset + pos + needle.len();
        if target < out.len() {
            let original = out[target];
            out[target] = if original == b'A' { b'B' } else { b'A' };
        }
        offset = offset + pos + needle.len();
    }
    out
}
