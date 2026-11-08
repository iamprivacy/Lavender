//! The signed release manifest. Every release carries `SHA256SUMS` (the
//! SHA-256 of each asset, coreutils format, sorted by name) whose first line
//! names the release (`# lavender-version: v2.2.0-local.3`), and
//! `SHA256SUMS.sig`, a minisign signature over the exact bytes of that file
//! made with the same key that signs the updater artifacts (`tauri signer
//! sign`; the `.sig` file is the base64 of the minisign signature text, and the
//! public key in `tauri.conf.json` is the base64 of the minisign public key
//! text). The version line is covered by the signature, so it is the trusted
//! version for the downgrade check.

use std::collections::HashMap;

use data_encoding::BASE64;
use minisign_verify::{PublicKey, Signature};
use ring::digest;

pub const SUMS_NAME: &str = "SHA256SUMS";
pub const SIG_NAME: &str = "SHA256SUMS.sig";
pub const VERSION_PREFIX: &str = "# lavender-version:";

/// Verifies `sig_b64` over `manifest` with `pubkey_b64`. Fails closed: an
/// empty or malformed key or signature is an error, never a pass.
pub fn verify(manifest: &[u8], sig_b64: &str, pubkey_b64: &str) -> Result<(), String> {
    let pubkey_text = decode_text(pubkey_b64).ok_or("the update key is not valid")?;
    let public_key = PublicKey::decode(&pubkey_text).map_err(|_| "the update key is not valid")?;
    let sig_text = decode_text(sig_b64).ok_or("the manifest signature is not valid")?;
    let signature =
        Signature::decode(&sig_text).map_err(|_| "the manifest signature is not valid")?;
    public_key
        .verify(manifest, &signature, false)
        .map_err(|_| "the manifest signature does not match".to_string())
}

fn decode_text(b64: &str) -> Option<String> {
    let b64 = b64.trim();
    if b64.is_empty() {
        return None;
    }
    let bytes = BASE64.decode(b64.as_bytes()).ok()?;
    String::from_utf8(bytes).ok()
}

/// The release named by the manifest's version line, e.g. `v2.2.0-local.3`.
pub fn version_line(manifest: &str) -> Option<&str> {
    manifest
        .lines()
        .find_map(|line| line.strip_prefix(VERSION_PREFIX))
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// File name to lowercase hex digest. Blank lines and `#` lines are skipped,
/// and a leading `*` (binary mode marker) on the name is dropped.
pub fn parse_sums(manifest: &str) -> HashMap<String, String> {
    let mut sums = HashMap::new();
    for line in manifest.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((hash, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) && !name.is_empty() {
            sums.insert(name.to_string(), hash.to_ascii_lowercase());
        }
    }
    sums
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = digest::digest(&digest::SHA256, bytes);
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Signed with a throwaway key made for these tests only; `other-key.pub`
    // is a second throwaway key.
    const MANIFEST: &[u8] = include_bytes!("fixtures/SHA256SUMS");
    const SIG: &str = include_str!("fixtures/SHA256SUMS.sig");
    const PUBKEY: &str = include_str!("fixtures/test-key.pub");

    #[test]
    fn a_genuine_manifest_verifies() {
        assert_eq!(verify(MANIFEST, SIG, PUBKEY), Ok(()));
    }

    #[test]
    fn a_tampered_manifest_is_refused() {
        let mut tampered = MANIFEST.to_vec();
        let at = tampered.iter().position(|b| *b == b'2').unwrap();
        tampered[at] = b'3';
        assert!(verify(&tampered, SIG, PUBKEY).is_err());
        let mut extended = MANIFEST.to_vec();
        extended.extend_from_slice(b"0000  extra\n");
        assert!(verify(&extended, SIG, PUBKEY).is_err());
    }

    #[test]
    fn a_missing_or_foreign_key_or_signature_is_refused() {
        assert!(verify(MANIFEST, SIG, "").is_err());
        assert!(verify(MANIFEST, "", PUBKEY).is_err());
        assert!(verify(MANIFEST, "not base64!", PUBKEY).is_err());
        assert!(verify(MANIFEST, SIG, "bm90IGEga2V5").is_err());
        let foreign = include_str!("fixtures/other-key.pub");
        assert_eq!(
            verify(MANIFEST, SIG, foreign),
            Err("the manifest signature does not match".to_string())
        );
    }

    #[test]
    fn reads_the_version_line_and_the_sums() {
        let text = std::str::from_utf8(MANIFEST).unwrap();
        assert_eq!(version_line(text), Some("v2.2.0-local.2"));
        let sums = parse_sums(text);
        assert_eq!(sums.len(), 2);
        assert_eq!(
            sums.get("Lavender_2.2.0_x64-setup.exe").map(String::as_str),
            Some(sha256_hex(b"windows payload").as_str())
        );
        assert_eq!(
            sums.get("Lavender_aarch64.app.tar.gz").map(String::as_str),
            Some(sha256_hex(b"macos payload").as_str())
        );
    }

    #[test]
    fn parse_sums_skips_comments_and_strips_the_binary_marker() {
        let hash = "A".repeat(64);
        let text = format!("# comment\n\n{hash} *file.zip\r\nshort  bad.zip\n{hash}\n");
        let sums = parse_sums(&text);
        assert_eq!(sums.len(), 1);
        assert_eq!(sums.get("file.zip"), Some(&"a".repeat(64)));
        assert_eq!(version_line(&text), None);
        assert_eq!(version_line("# lavender-version:   \n"), None);
    }

    #[test]
    fn sha256_matches_a_known_digest() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
