//! Compact call IDs carrying versioned metadata without external lookup state.

use std::io::Read;

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

pub const NORMAL: u8 = 0;
/// App-generated calls in the saved session that the compact overlay omits. Kind `1`
/// was the removed injected-file kind.
pub const SYNTHETIC: u8 = 2;
const VERSION: u8 = 1;
const MAX_DATA: usize = 17;
const MASK_DOMAIN: &[u8] = b"harness/tool-call-id/mask/v1\0";

#[derive(Debug, PartialEq, Eq)]
pub struct Metadata {
    pub kind: u8,
    pub data: Vec<u8>,
}

/// Encode metadata with fresh entropy for the clear prefix and unused body bytes.
pub fn generate(kind: u8, data: &[u8]) -> Result<String> {
    ensure!(data.len() <= MAX_DATA, "tool call ID data exceeds 17 bytes");
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .context("reading tool call ID entropy from /dev/urandom")?;
    Ok(encode(bytes, kind, data))
}

/// Decode only canonical IDs with a supported version and valid payload length.
pub fn decode(id: &str) -> Option<Metadata> {
    if id.len() != 32 {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(id).ok()?;
    if URL_SAFE_NO_PAD.encode(&decoded) != id {
        return None;
    }
    let mut bytes: [u8; 24] = decoded.try_into().ok()?;
    mask_body(&mut bytes);
    let length = usize::from(bytes[6]);
    if bytes[4] != VERSION || length > MAX_DATA {
        return None;
    }
    Some(Metadata {
        kind: bytes[5],
        data: bytes[7..7 + length].to_vec(),
    })
}

/// The caller bounds payload length; remaining entropy bytes provide body padding.
fn encode(mut bytes: [u8; 24], kind: u8, data: &[u8]) -> String {
    bytes[4] = VERSION;
    bytes[5] = kind;
    bytes[6] = data.len() as u8;
    bytes[7..7 + data.len()].copy_from_slice(data);
    mask_body(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// XOR is reversible formatting, not encryption or authentication; the prefix is public.
fn mask_body(bytes: &mut [u8; 24]) {
    let mut hash = Sha256::new();
    hash.update(MASK_DOMAIN);
    hash.update(&bytes[..4]);
    let mask = hash.finalize();
    for (byte, mask) in bytes[4..].iter_mut().zip(mask.iter()) {
        *byte ^= mask;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every allowed payload length preserves arbitrary binary data and kind bytes.
    #[test]
    fn binary_roundtrip_and_format() {
        for length in 0..=MAX_DATA {
            let data: Vec<_> = (0..length).map(|index| (index * 83) as u8).collect();
            for kind in [NORMAL, SYNTHETIC, 255] {
                let id = generate(kind, &data).unwrap();
                assert_eq!(id.len(), 32);
                assert!(
                    id.bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
                );
                assert_eq!(
                    decode(&id),
                    Some(Metadata {
                        kind,
                        data: data.clone()
                    })
                );
            }
        }
        assert!(generate(NORMAL, &[0; 18]).is_err());
        assert_ne!(
            generate(NORMAL, &[]).unwrap(),
            generate(NORMAL, &[]).unwrap()
        );
    }

    /// Legacy names and corrupt envelopes are not interpreted as current metadata.
    #[test]
    fn rejects_invalid_envelopes() {
        for id in [
            "",
            "fyi-123-1",
            "injection-123-1",
            "reasoning-3",
            "================================",
            "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!",
        ] {
            assert!(decode(id).is_none());
        }
        let valid = generate(NORMAL, &[]).unwrap();
        assert!(decode(&format!("{valid}=")).is_none());
        assert!(decode(&valid[..31]).is_none());
        for (offset, value) in [(4, VERSION + 1), (6, 18)] {
            let mut bytes: [u8; 24] = URL_SAFE_NO_PAD.decode(&valid).unwrap().try_into().unwrap();
            mask_body(&mut bytes);
            bytes[offset] = value;
            mask_body(&mut bytes);
            assert!(decode(&URL_SAFE_NO_PAD.encode(bytes)).is_none());
        }
    }
}
