//! Mutation tests (GL #425 AC 1): a synthetic spec-conformant bundle
//! verifies; ANY single-byte manipulation — payload, chain, manifest —
//! is detected. The bundle is built here from the contract text alone,
//! independent of the engine's generator.

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use std::io::Write;

// The verifier crate is a binary; pull the core in via path include for
// testing (no library target on purpose — auditors get one executable).
#[path = "../src/verify.rs"]
mod verify;

use verify::{verify_bundle, verify_bundle_with_test_limits, StepStatus};

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct Fixture {
    bundle: Vec<u8>,
    pubkey_hex: String,
}

fn stored_zip(entries: impl IntoIterator<Item = (String, Vec<u8>)>) -> Vec<u8> {
    let mut bundle = Vec::new();
    let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(zip::DateTime::default());
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut bundle));
    for (path, bytes) in entries {
        zip.start_file(path, options).unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap();
    bundle
}

/// Build a minimal, fully spec-conformant bundle: 3 chained + signed audit
/// entries, one policy file, signed manifest, deterministic ZIP.
fn build_fixture() -> Fixture {
    let signing = SigningKey::from_bytes(&[7u8; 32]);
    let pubkey_hex = hex_encode(signing.verifying_key().as_bytes());

    // audit chain
    let mut entries = Vec::new();
    let mut prev = "genesis".to_string();
    for (i, tool) in ["ctx_read", "ctx_search", "ctx_shell"].iter().enumerate() {
        let data = serde_json::json!({
            "agent_id": "agent-1",
            "tool": tool,
            "action": null,
            "input_hash": sha256_hex(b"{}"),
            "output_tokens": i as u64,
            "role": "developer",
            "event_type": "tool_call",
        });
        let data_json = serde_json::to_string(&data).unwrap();
        let mut h = Sha256::new();
        h.update(prev.as_bytes());
        h.update(data_json.as_bytes());
        let entry_hash = format!("{:x}", h.finalize());
        let signature = hex_encode(&signing.sign(entry_hash.as_bytes()).to_bytes());

        let mut entry = data;
        entry["timestamp"] = serde_json::json!(format!("2026-06-0{}T00:00:00Z", i + 1));
        entry["prev_hash"] = serde_json::json!(prev);
        entry["entry_hash"] = serde_json::json!(entry_hash);
        entry["signature"] = serde_json::json!(signature);
        entries.push(serde_json::to_string(&entry).unwrap());
        prev = entry_hash;
    }
    let trail = format!("{}\n", entries.join("\n"));
    let policy = r#"{"name":"baseline","version":"1.0.0"}"#.to_string();

    let files: Vec<(&str, &[u8])> = vec![
        ("audit/trail.jsonl", trail.as_bytes()),
        ("policies/baseline.resolved.json", policy.as_bytes()),
    ];

    let mut manifest = serde_json::json!({
        "bundle": "evidence-bundle",
        "version": 1,
        "period": { "from": "2026-06-01T00:00:00Z", "to": "2026-06-03T00:00:00Z" },
        "subject": { "agent_id": "agent-1", "project": "fixture" },
        "framework": null,
        "files": files.iter().map(|(p, b)| serde_json::json!({
            "path": p, "sha256": sha256_hex(b)
        })).collect::<Vec<_>>(),
        "chain": { "entries": 3, "anchor_prev_hash": "genesis", "head_hash": prev },
        "signing": {
            "algorithm": "ed25519",
            "public_key": pubkey_hex,
            "signed_digest": "",
            "signature": "",
        }
    });
    let digest = sha256_hex(serde_json::to_string(&manifest).unwrap().as_bytes());
    let signature = hex_encode(&signing.sign(digest.as_bytes()).to_bytes());
    manifest["signing"]["signed_digest"] = serde_json::json!(digest);
    manifest["signing"]["signature"] = serde_json::json!(signature);

    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .last_modified_time(zip::DateTime::default());
        let manifest_bytes = serde_json::to_string(&manifest).unwrap().into_bytes();
        let mut all: Vec<(&str, &[u8])> = vec![("manifest.json", &manifest_bytes)];
        all.extend(files.iter().copied());
        all.sort_by_key(|(p, _)| *p);
        for (path, bytes) in all {
            zip.start_file(path, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }
    Fixture {
        bundle: buf,
        pubkey_hex,
    }
}

#[test]
fn conformant_bundle_verifies_in_both_key_modes() {
    let fx = build_fixture();

    let report = verify_bundle(&fx.bundle, None);
    assert!(report.valid, "self-attested mode: {:#?}", report.steps);
    assert!(report.key_self_attested);

    let report = verify_bundle(&fx.bundle, Some(&fx.pubkey_hex));
    assert!(report.valid, "out-of-band mode: {:#?}", report.steps);
    assert!(!report.key_self_attested);
    assert!(report.steps.iter().all(|s| s.status == StepStatus::Pass));
}

#[test]
fn wrong_out_of_band_key_fails_signature_step() {
    let fx = build_fixture();
    let wrong = hex_encode(
        SigningKey::from_bytes(&[9u8; 32])
            .verifying_key()
            .as_bytes(),
    );
    let report = verify_bundle(&fx.bundle, Some(&wrong));
    assert!(!report.valid);
}

#[test]
fn archive_entry_limit_fails_before_manifest_parse() {
    let bundle = stored_zip((0..1_025).map(|index| (format!("extra/{index}"), Vec::new())));

    let report = verify_bundle(&bundle, None);
    assert!(!report.valid);
    assert_eq!(report.steps.len(), 1);
    assert_eq!(report.steps[0].name, "archive readable");
    assert_eq!(report.steps[0].status, StepStatus::Fail);
    assert_eq!(
        report.steps[0].detail,
        "archive has 1025 entries; limit is 1024"
    );
}

#[test]
fn archive_payload_limits_fail_closed() {
    let single = stored_zip(vec![("payload".to_string(), vec![0; 17])]);
    let report = verify_bundle_with_test_limits(&single, None, 1_024, 4, 16, 32);
    assert!(!report.valid);
    assert_eq!(
        report.steps[0].detail,
        "entry payload has 17 bytes; limit is 16"
    );

    let total = stored_zip(vec![
        ("first".to_string(), vec![0; 16]),
        ("second".to_string(), vec![0; 16]),
    ]);
    let report = verify_bundle_with_test_limits(&total, None, 1_024, 4, 16, 31);
    assert!(!report.valid);
    assert_eq!(
        report.steps[0].detail,
        "archive payload has 32 bytes; limit is 31"
    );
}

#[test]
fn archive_raw_size_limit_fails_before_zip_parsing() {
    let report = verify_bundle_with_test_limits(b"not-a-zip", None, 8, 4, 16, 32);
    assert!(!report.valid);
    assert_eq!(report.steps[0].detail, "archive has 9 bytes; limit is 8");
}

/// AC 1 core: flip ONE byte at every offset class of the archive and
/// demand detection. ZIP local-header hash bytes, payload bytes, manifest
/// bytes — anything that changes the verified surfaces must fail; flips
/// that only touch ZIP padding/metadata may still pass content checks but
/// must never corrupt a PASS into wrong content.
#[test]
fn single_byte_flips_in_payload_are_detected() {
    let fx = build_fixture();

    // Locate the payload regions: every stored (uncompressed) file's bytes
    // appear verbatim in the archive — flip a byte inside each.
    let needles: [&[u8]; 4] = [
        b"ctx_search",      // audit entry content
        b"genesis",         // chain anchor in trail
        b"baseline",        // policy payload
        b"evidence-bundle", // manifest content
    ];
    let mut tested = 0;
    for needle in needles {
        if let Some(pos) = find(&fx.bundle, needle) {
            let mut mutated = fx.bundle.clone();
            mutated[pos] ^= 0x01;
            let report = verify_bundle(&mutated, Some(&fx.pubkey_hex));
            assert!(
                !report.valid,
                "1-byte flip in {:?} region was NOT detected",
                String::from_utf8_lossy(needle)
            );
            tested += 1;
        }
    }
    assert_eq!(tested, 4, "all four payload regions must be present");
}

#[test]
fn truncated_chain_is_detected() {
    let fx = build_fixture();
    // Rebuild the archive with the last audit line dropped but the original
    // manifest kept — count/head mismatch must fail.
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&fx.bundle)).unwrap();
    let mut out = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut out));
        let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .last_modified_time(zip::DateTime::default());
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).unwrap();
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut buf).unwrap();
            let name = entry.name().expect("valid ZIP entry name").into_owned();
            if name == "audit/trail.jsonl" {
                let text = String::from_utf8(buf).unwrap();
                let mut lines: Vec<&str> = text.lines().collect();
                lines.pop();
                buf = format!("{}\n", lines.join("\n")).into_bytes();
            }
            zip.start_file(&name, options).unwrap();
            zip.write_all(&buf).unwrap();
        }
        zip.finish().unwrap();
    }
    let report = verify_bundle(&out, Some(&fx.pubkey_hex));
    assert!(!report.valid, "dropped tail entry must be detected");
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
