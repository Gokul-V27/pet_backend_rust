//! Encrypts small secrets kept in the database (staff authenticator secrets) with AES-256-GCM, so
//! a copy of the database alone can't produce sign-in codes. The key lives only in the server's
//! environment (`MFA_ENCRYPTION_KEY`). Stored as `v1:` + hex(nonce ‖ ciphertext ‖ tag).

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};

const PREFIX: &str = "v1:";

/// Seals `plain`. `context` (e.g. the staff member's id) is bound to the result, so a value copied
/// onto another row doesn't open.
pub fn seal(key: &[u8; 32], context: &str, plain: &[u8]) -> anyhow::Result<String> {
    let mut nonce = [0u8; NONCE_LEN];
    // A fresh random nonce every time: never reuse one with the same key.
    getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("no secure random source: {e}"))?;
    let k = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, key).map_err(|_| anyhow::anyhow!("bad key"))?,
    );
    let mut buf = plain.to_vec();
    k.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(context.as_bytes()),
        &mut buf,
    )
    .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&buf);
    Ok(format!("{PREFIX}{}", hex::encode(out)))
}

/// Opens a value made by [`seal`] with the same key and context.
pub fn open(key: &[u8; 32], context: &str, stored: &str) -> anyhow::Result<Vec<u8>> {
    let raw = stored
        .strip_prefix(PREFIX)
        .ok_or_else(|| anyhow::anyhow!("unknown secret format"))
        .and_then(|h| hex::decode(h).map_err(|_| anyhow::anyhow!("corrupt secret")))?;
    if raw.len() < NONCE_LEN + AES_256_GCM.tag_len() {
        anyhow::bail!("corrupt secret");
    }
    let (nonce, rest) = raw.split_at(NONCE_LEN);
    let nonce =
        Nonce::try_assume_unique_for_key(nonce).map_err(|_| anyhow::anyhow!("corrupt secret"))?;
    let k = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, key).map_err(|_| anyhow::anyhow!("bad key"))?,
    );
    let mut buf = rest.to_vec();
    let plain = k
        .open_in_place(nonce, Aad::from(context.as_bytes()), &mut buf)
        .map_err(|_| anyhow::anyhow!("secret could not be decrypted (wrong key or tampered)"))?;
    Ok(plain.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7u8; 32];

    #[test]
    fn round_trips_and_never_repeats() {
        let a = seal(&KEY, "admin-1", b"secret").unwrap();
        let b = seal(&KEY, "admin-1", b"secret").unwrap();
        assert_ne!(a, b, "a fresh nonce each time");
        assert_eq!(open(&KEY, "admin-1", &a).unwrap(), b"secret");
    }

    #[test]
    fn refuses_wrong_key_context_or_tampering() {
        let a = seal(&KEY, "admin-1", b"secret").unwrap();
        assert!(open(&[8u8; 32], "admin-1", &a).is_err());
        assert!(open(&KEY, "admin-2", &a).is_err());
        let mut t = a.clone().into_bytes();
        let last = t.len() - 1;
        t[last] = if t[last] == b'0' { b'1' } else { b'0' };
        assert!(open(&KEY, "admin-1", std::str::from_utf8(&t).unwrap()).is_err());
        assert!(open(&KEY, "admin-1", "v1:00").is_err());
        assert!(open(&KEY, "admin-1", "plain").is_err());
    }
}
