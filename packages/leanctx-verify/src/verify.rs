//! Verification core — five steps, each reported PASS/FAIL with an
//! auditor-readable detail line (contract: evidence-bundle-v1).

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pass,
    Fail,
    Skipped,
}

#[derive(Debug, Serialize)]
pub struct Step {
    pub name: &'static str,
    pub status: StepStatus,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub valid: bool,
    pub key_self_attested: bool,
    pub steps: Vec<Step>,
}

/// Reserved top-level directories: unknown files in here are tolerated
/// (future bundle versions), unknown files anywhere else are an error.
const RESERVED_DIRS: &[&str] = &["slo/", "registry/"];

pub const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 1_024;
const MAX_ENTRY_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy)]
struct ArchiveLimits {
    raw_bytes: u64,
    entries: usize,
    entry_bytes: u64,
    total_bytes: u64,
}

const DEFAULT_ARCHIVE_LIMITS: ArchiveLimits = ArchiveLimits {
    raw_bytes: MAX_ARCHIVE_BYTES,
    entries: MAX_ARCHIVE_ENTRIES,
    entry_bytes: MAX_ENTRY_BYTES,
    total_bytes: MAX_TOTAL_BYTES,
};

pub fn verify_bundle(raw: &[u8], pubkey_override: Option<&str>) -> Report {
    verify_bundle_with_limits(raw, pubkey_override, DEFAULT_ARCHIVE_LIMITS)
}

fn verify_bundle_with_limits(
    raw: &[u8],
    pubkey_override: Option<&str>,
    limits: ArchiveLimits,
) -> Report {
    let mut steps = Vec::new();
    let mut valid = true;
    let fail = |steps: &mut Vec<Step>, name: &'static str, detail: String| {
        steps.push(Step {
            name,
            status: StepStatus::Fail,
            detail,
        });
    };

    // ── step 1: archive + manifest ───────────────────────────────────────
    let raw_len = u64::try_from(raw.len()).expect("usize fits in u64");
    if raw_len > limits.raw_bytes {
        return archive_limit_failure(format!(
            "archive has {raw_len} bytes; limit is {}",
            limits.raw_bytes
        ));
    }
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    match zip::ZipArchive::new(std::io::Cursor::new(raw)) {
        Ok(mut archive) => {
            if archive.len() > limits.entries {
                return archive_limit_failure(format!(
                    "archive has {} entries; limit is {}",
                    archive.len(),
                    limits.entries
                ));
            }
            let mut total_bytes = 0_u64;
            for i in 0..archive.len() {
                match archive.by_index(i) {
                    Ok(mut entry) => {
                        let name = match entry.name() {
                            Ok(name) => name.into_owned(),
                            Err(error) => {
                                fail(
                                    &mut steps,
                                    "archive readable",
                                    format!("entry {i} has invalid name: {error}"),
                                );
                                return Report {
                                    valid: false,
                                    key_self_attested: false,
                                    steps,
                                };
                            }
                        };
                        let declared_bytes = entry.size();
                        if declared_bytes > limits.entry_bytes {
                            return archive_limit_failure(format!(
                                "entry {name} has {declared_bytes} bytes; limit is {}",
                                limits.entry_bytes
                            ));
                        }
                        total_bytes = match total_bytes.checked_add(declared_bytes) {
                            Some(total) if total <= limits.total_bytes => total,
                            Some(total) => {
                                return archive_limit_failure(format!(
                                    "archive payload has {total} bytes; limit is {}",
                                    limits.total_bytes
                                ));
                            }
                            None => {
                                return archive_limit_failure(
                                    "archive payload size overflowed".to_string(),
                                );
                            }
                        };

                        let capacity =
                            usize::try_from(declared_bytes).expect("entry limit fits in usize");
                        let mut buf = Vec::with_capacity(capacity);
                        if entry
                            .by_ref()
                            .take(declared_bytes.saturating_add(1))
                            .read_to_end(&mut buf)
                            .is_err()
                        {
                            fail(
                                &mut steps,
                                "archive readable",
                                format!("entry {name} unreadable"),
                            );
                            return Report {
                                valid: false,
                                key_self_attested: false,
                                steps,
                            };
                        }
                        let actual_bytes = u64::try_from(buf.len()).expect("usize fits in u64");
                        if actual_bytes != declared_bytes {
                            return archive_limit_failure(format!(
                                "entry {name} read {actual_bytes} bytes; declared {declared_bytes}"
                            ));
                        }
                        files.insert(name, buf);
                    }
                    Err(e) => {
                        fail(&mut steps, "archive readable", format!("entry {i}: {e}"));
                        return Report {
                            valid: false,
                            key_self_attested: false,
                            steps,
                        };
                    }
                }
            }
        }
        Err(e) => {
            fail(&mut steps, "archive readable", format!("not a ZIP: {e}"));
            return Report {
                valid: false,
                key_self_attested: false,
                steps,
            };
        }
    }

    let manifest: serde_json::Value = match files
        .get("manifest.json")
        .and_then(|b| serde_json::from_slice(b).ok())
    {
        Some(m) => m,
        None => {
            fail(
                &mut steps,
                "manifest parses",
                "manifest.json missing or not valid JSON".to_string(),
            );
            return Report {
                valid: false,
                key_self_attested: false,
                steps,
            };
        }
    };
    if manifest["bundle"] != "evidence-bundle" || manifest["version"] != 1 {
        fail(
            &mut steps,
            "manifest parses",
            format!(
                "unsupported bundle/version: {}/{}",
                manifest["bundle"], manifest["version"]
            ),
        );
        return Report {
            valid: false,
            key_self_attested: false,
            steps,
        };
    }
    steps.push(Step {
        name: "archive + manifest",
        status: StepStatus::Pass,
        detail: format!("evidence-bundle v1, {} archive entries", files.len()),
    });

    // ── step 2: file inventory + hashes ──────────────────────────────────
    let mut inventory_ok = true;
    let mut detail = String::new();
    let listed: Vec<(String, String)> = manifest["files"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    Some((
                        f["path"].as_str()?.to_string(),
                        f["sha256"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    if listed.is_empty() {
        inventory_ok = false;
        detail = "manifest lists no files".to_string();
    }
    for (path, expected) in &listed {
        match files.get(path) {
            None => {
                inventory_ok = false;
                detail = format!("listed file missing from archive: {path}");
                break;
            }
            Some(bytes) => {
                let actual = sha256_hex(bytes);
                if &actual != expected {
                    inventory_ok = false;
                    detail =
                        format!("hash mismatch for {path}: manifest {expected}, archive {actual}");
                    break;
                }
            }
        }
    }
    if inventory_ok {
        for name in files.keys() {
            if name == "manifest.json" || name.ends_with('/') {
                continue;
            }
            let listed_it = listed.iter().any(|(p, _)| p == name);
            let reserved = RESERVED_DIRS.iter().any(|d| name.starts_with(d));
            if !listed_it && !reserved {
                inventory_ok = false;
                detail = format!("unlisted payload file in archive: {name}");
                break;
            }
        }
    }
    steps.push(Step {
        name: "file inventory + SHA-256",
        status: if inventory_ok {
            StepStatus::Pass
        } else {
            StepStatus::Fail
        },
        detail: if inventory_ok {
            format!("{} files match their manifest hashes", listed.len())
        } else {
            detail
        },
    });
    valid &= inventory_ok;

    // ── step 3: audit chain replay ───────────────────────────────────────
    let chain_step = verify_chain(&files, &manifest);
    valid &= chain_step.status == StepStatus::Pass;
    steps.push(chain_step);

    // ── step 4: manifest signature ───────────────────────────────────────
    let manifest_key = manifest["signing"]["public_key"].as_str().unwrap_or("");
    let key_self_attested = pubkey_override.is_none();
    let key_hex = pubkey_override.unwrap_or(manifest_key);
    let sig_step = verify_manifest_signature(&manifest, key_hex, manifest_key);
    let manifest_key_ok = sig_step.status == StepStatus::Pass;
    valid &= manifest_key_ok;
    steps.push(sig_step);

    // ── step 5: per-entry signatures (where present) ─────────────────────
    let entry_step = verify_entry_signatures(&files, key_hex);
    valid &= entry_step.status != StepStatus::Fail;
    steps.push(entry_step);

    Report {
        valid,
        key_self_attested,
        steps,
    }
}

fn archive_limit_failure(detail: String) -> Report {
    Report {
        valid: false,
        key_self_attested: false,
        steps: vec![Step {
            name: "archive readable",
            status: StepStatus::Fail,
            detail,
        }],
    }
}

#[cfg(test)]
// Integration tests include this module directly and need small limits without
// allocating production-sized fixtures; the binary's unit-test target does not.
#[allow(dead_code)]
pub(crate) fn verify_bundle_with_test_limits(
    raw: &[u8],
    pubkey_override: Option<&str>,
    raw_bytes: u64,
    entries: usize,
    entry_bytes: u64,
    total_bytes: u64,
) -> Report {
    verify_bundle_with_limits(
        raw,
        pubkey_override,
        ArchiveLimits {
            raw_bytes,
            entries,
            entry_bytes,
            total_bytes,
        },
    )
}

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub(super) fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Replay the hash chain exactly as the contract defines it:
/// `entry_hash = sha256(prev_hash ‖ canonical(data))` where data is the
/// seven-field object serialized with sorted keys.
fn verify_chain(files: &BTreeMap<String, Vec<u8>>, manifest: &serde_json::Value) -> Step {
    let Some(trail) = files.get("audit/trail.jsonl") else {
        return Step {
            name: "audit chain replay",
            status: StepStatus::Fail,
            detail: "audit/trail.jsonl missing".to_string(),
        };
    };
    let text = String::from_utf8_lossy(trail);
    let anchor = manifest["chain"]["anchor_prev_hash"].as_str().unwrap_or("");
    let head = manifest["chain"]["head_hash"].as_str().unwrap_or("");
    let expected_count = manifest["chain"]["entries"].as_u64().unwrap_or(0) as usize;

    let mut prev = anchor.to_string();
    let mut count = 0usize;
    for line in text.lines() {
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Step {
                    name: "audit chain replay",
                    status: StepStatus::Fail,
                    detail: format!("entry {count}: not valid JSON ({e})"),
                }
            }
        };
        if entry["prev_hash"] != serde_json::Value::String(prev.clone()) {
            return Step {
                name: "audit chain replay",
                status: StepStatus::Fail,
                detail: format!(
                    "entry {count}: prev_hash {} breaks the chain (expected {prev})",
                    entry["prev_hash"]
                ),
            };
        }
        // Canonical data object: serde_json maps sort keys.
        let data = serde_json::json!({
            "agent_id": entry["agent_id"],
            "tool": entry["tool"],
            "action": entry["action"],
            "input_hash": entry["input_hash"],
            "output_tokens": entry["output_tokens"],
            "role": entry["role"],
            "event_type": entry["event_type"],
        });
        let data_json = serde_json::to_string(&data).expect("serializable");
        let mut hasher = Sha256::new();
        hasher.update(prev.as_bytes());
        hasher.update(data_json.as_bytes());
        let expected_hash = format!("{:x}", hasher.finalize());

        let actual = entry["entry_hash"].as_str().unwrap_or("");
        if actual != expected_hash {
            return Step {
                name: "audit chain replay",
                status: StepStatus::Fail,
                detail: format!("entry {count}: recorded hash does not match recomputation — content was modified"),
            };
        }
        prev = expected_hash;
        count += 1;
    }

    if count != expected_count {
        return Step {
            name: "audit chain replay",
            status: StepStatus::Fail,
            detail: format!("manifest claims {expected_count} entries, archive holds {count}"),
        };
    }
    if prev != head {
        return Step {
            name: "audit chain replay",
            status: StepStatus::Fail,
            detail: "head hash does not match the manifest".to_string(),
        };
    }
    Step {
        name: "audit chain replay",
        status: StepStatus::Pass,
        detail: format!("{count} entries replay from anchor to head"),
    }
}

fn verify_manifest_signature(
    manifest: &serde_json::Value,
    key_hex: &str,
    manifest_key: &str,
) -> Step {
    let signature_hex = manifest["signing"]["signature"].as_str().unwrap_or("");
    let claimed_digest = manifest["signing"]["signed_digest"].as_str().unwrap_or("");

    // Recompute the digest over the manifest with signature/digest blanked.
    let mut unsigned = manifest.clone();
    unsigned["signing"]["signature"] = serde_json::Value::String(String::new());
    unsigned["signing"]["signed_digest"] = serde_json::Value::String(String::new());
    let digest = sha256_hex(
        serde_json::to_string(&unsigned)
            .expect("serializable")
            .as_bytes(),
    );
    if digest != claimed_digest {
        return Step {
            name: "manifest signature",
            status: StepStatus::Fail,
            detail: "manifest digest does not recompute — manifest was modified".to_string(),
        };
    }

    let (Some(key_bytes), Some(sig_bytes)) = (hex_decode(key_hex), hex_decode(signature_hex))
    else {
        return Step {
            name: "manifest signature",
            status: StepStatus::Fail,
            detail: "key or signature is not valid hex".to_string(),
        };
    };
    let Ok(key) = VerifyingKey::from_bytes(&key_bytes.try_into().unwrap_or([0u8; 32])) else {
        return Step {
            name: "manifest signature",
            status: StepStatus::Fail,
            detail: "public key is not a valid Ed25519 key".to_string(),
        };
    };
    let Ok(sig) = Signature::from_slice(&sig_bytes) else {
        return Step {
            name: "manifest signature",
            status: StepStatus::Fail,
            detail: "signature is not a valid Ed25519 signature".to_string(),
        };
    };
    if key.verify(digest.as_bytes(), &sig).is_err() {
        return Step {
            name: "manifest signature",
            status: StepStatus::Fail,
            detail: "Ed25519 verification failed — wrong key or forged manifest".to_string(),
        };
    }
    let mode = if key_hex == manifest_key {
        "embedded key (self-attested)"
    } else {
        "out-of-band key"
    };
    Step {
        name: "manifest signature",
        status: StepStatus::Pass,
        detail: format!("Ed25519 valid over digest {} ({mode})", &digest[..16]),
    }
}

fn verify_entry_signatures(files: &BTreeMap<String, Vec<u8>>, key_hex: &str) -> Step {
    let Some(trail) = files.get("audit/trail.jsonl") else {
        return Step {
            name: "entry signatures",
            status: StepStatus::Skipped,
            detail: "no audit segment".to_string(),
        };
    };
    let Some(key_bytes) = hex_decode(key_hex) else {
        return Step {
            name: "entry signatures",
            status: StepStatus::Skipped,
            detail: "no usable key".to_string(),
        };
    };
    let Ok(key) = VerifyingKey::from_bytes(&key_bytes.try_into().unwrap_or([0u8; 32])) else {
        return Step {
            name: "entry signatures",
            status: StepStatus::Skipped,
            detail: "no usable key".to_string(),
        };
    };

    let text = String::from_utf8_lossy(trail);
    let (mut signed, mut unsigned) = (0usize, 0usize);
    for (i, line) in text.lines().enumerate() {
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // chain replay already failed on this
        };
        match entry["signature"].as_str() {
            None | Some("") => unsigned += 1,
            Some(sig_hex) => {
                let entry_hash = entry["entry_hash"].as_str().unwrap_or("");
                let Some(sig_bytes) = hex_decode(sig_hex) else {
                    return Step {
                        name: "entry signatures",
                        status: StepStatus::Fail,
                        detail: format!("entry {i}: signature is not hex"),
                    };
                };
                let Ok(sig) = Signature::from_slice(&sig_bytes) else {
                    return Step {
                        name: "entry signatures",
                        status: StepStatus::Fail,
                        detail: format!("entry {i}: malformed signature"),
                    };
                };
                if key.verify(entry_hash.as_bytes(), &sig).is_err() {
                    return Step {
                        name: "entry signatures",
                        status: StepStatus::Fail,
                        detail: format!("entry {i}: signature does not verify"),
                    };
                }
                signed += 1;
            }
        }
    }
    Step {
        name: "entry signatures",
        status: StepStatus::Pass,
        detail: if unsigned == 0 {
            format!("{signed} entries signed and verified")
        } else {
            format!("{signed} verified, {unsigned} unsigned (early-boot entries reduce provenance, not integrity)")
        },
    }
}
