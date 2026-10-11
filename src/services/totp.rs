//! Staff two-step sign-in: time-based one-time codes (TOTP, RFC 6238) as authenticator apps
//! (Google Authenticator, Microsoft Authenticator, 1Password…) make them: HMAC-SHA1, 30-second
//! steps, 6 digits. A code is accepted one step either side of now (clock drift), and each step is
//! accepted only once, so a code seen over someone's shoulder can't be used again.
//!
//! Recovery codes stand in for the phone when it's lost: ten single-use codes shown once, stored
//! only as hashes.

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const DIGITS: u32 = 6;
pub const STEP_SECONDS: i64 = 30;
/// Steps either side of now that still count (one step = 30 seconds of clock drift).
pub const WINDOW: i64 = 1;
pub const SECRET_BYTES: usize = 20;
pub const RECOVERY_CODES: usize = 10;

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 without padding: how authenticator apps take a secret.
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &b in bytes {
        buffer = (buffer << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// A fresh random secret.
pub fn new_secret() -> anyhow::Result<Vec<u8>> {
    let mut s = vec![0u8; SECRET_BYTES];
    getrandom::fill(&mut s).map_err(|e| anyhow::anyhow!("no secure random source: {e}"))?;
    Ok(s)
}

/// The code for one 30-second step.
pub fn code_at(secret: &[u8], step: i64) -> u32 {
    let mut mac =
        <Hmac<Sha1> as KeyInit>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(&step.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let offset = usize::from(h[h.len() - 1] & 0x0f);
    let bin = (u32::from(h[offset] & 0x7f) << 24)
        | (u32::from(h[offset + 1]) << 16)
        | (u32::from(h[offset + 2]) << 8)
        | u32::from(h[offset + 3]);
    bin % 10u32.pow(DIGITS)
}

pub fn step_of(unix_seconds: i64) -> i64 {
    unix_seconds.div_euclid(STEP_SECONDS)
}

/// Checks a typed code. Returns the step it matched, which must be stored as the last one used;
/// a step at or before `last_step` is refused (the same code can't be used twice).
pub fn verify(secret: &[u8], typed: &str, unix_seconds: i64, last_step: i64) -> Option<i64> {
    let typed: String = typed.chars().filter(|c| !c.is_whitespace()).collect();
    if typed.len() != DIGITS as usize || !typed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let now = step_of(unix_seconds);
    // Check every step in the window without stopping early, so timing doesn't say which matched.
    let mut hit = None;
    for step in (now - WINDOW)..=(now + WINDOW) {
        let want = format!("{:0width$}", code_at(secret, step), width = DIGITS as usize);
        if bool::from(want.as_bytes().ct_eq(typed.as_bytes())) && step > last_step {
            hit = Some(step);
        }
    }
    hit
}

/// The link an authenticator app understands (also what a QR code would hold).
pub fn otpauth_url(issuer: &str, account: &str, secret: &[u8]) -> String {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECONDS}",
        enc(issuer),
        enc(account),
        base32_encode(secret),
        enc(issuer)
    )
}

/// Letters and digits that can't be mistaken for one another when read aloud or typed.
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// A random code of `len` characters from [`CODE_ALPHABET`], grouped in fours: `K7QM-3XRA`.
pub fn readable_code(len: usize) -> anyhow::Result<String> {
    let mut out = String::with_capacity(len + len / 4);
    let mut byte = [0u8; 1];
    while out.chars().filter(|c| *c != '-').count() < len {
        getrandom::fill(&mut byte).map_err(|e| anyhow::anyhow!("no secure random source: {e}"))?;
        // Rejection sampling keeps every character equally likely.
        if usize::from(byte[0]) >= 256 - 256 % CODE_ALPHABET.len() {
            continue;
        }
        let n = out.chars().filter(|c| *c != '-').count();
        if n > 0 && n % 4 == 0 {
            out.push('-');
        }
        out.push(CODE_ALPHABET[usize::from(byte[0]) % CODE_ALPHABET.len()] as char);
    }
    Ok(out)
}

/// The same code however it was typed: upper case, no dashes or spaces.
pub fn normalize_code(typed: &str) -> String {
    typed
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// How a recovery or setup code is stored: a hash of its normalized form, with a purpose label so
/// one kind of code can never be used as another.
pub fn hash_code(purpose: &str, typed: &str) -> String {
    hex::encode(Sha256::digest(
        format!("{purpose}:{}", normalize_code(typed)).as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 appendix B, SHA-1 secret "12345678901234567890"; the 8-digit codes cut to 6.
    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn matches_the_rfc_test_vectors() {
        assert_eq!(code_at(RFC_SECRET, step_of(59)), 287_082);
        assert_eq!(code_at(RFC_SECRET, step_of(1_111_111_109)), 81_804);
        assert_eq!(code_at(RFC_SECRET, step_of(1_234_567_890)), 5_924);
        assert_eq!(code_at(RFC_SECRET, step_of(2_000_000_000)), 279_037);
    }

    #[test]
    fn base32_matches_rfc_4648() {
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_encode(b"f"), "MY");
        assert_eq!(base32_encode(b""), "");
    }

    #[test]
    fn accepts_drift_but_not_reuse_or_junk() {
        let t = 1_234_567_890;
        let code = format!("{:06}", code_at(RFC_SECRET, step_of(t)));
        let step = verify(RFC_SECRET, &code, t, 0).expect("current code");
        assert_eq!(step, step_of(t));
        // 30 seconds later the same code still works once…
        assert!(verify(RFC_SECRET, &code, t + 30, 0).is_some());
        // …but not after it was used.
        assert!(verify(RFC_SECRET, &code, t + 30, step).is_none());
        // Too old, wrong, or not a code at all.
        assert!(verify(RFC_SECRET, &code, t + 120, 0).is_none());
        assert!(verify(RFC_SECRET, "000000", t, 0).is_none() || code == "000000");
        assert!(verify(RFC_SECRET, "12345", t, 0).is_none());
        assert!(verify(RFC_SECRET, "abcdef", t, 0).is_none());
        // Spaces as people type them ("123 456") are fine.
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        assert!(verify(RFC_SECRET, &spaced, t, 0).is_some());
    }

    #[test]
    fn otpauth_link_has_the_secret_and_issuer() {
        let url = otpauth_url("Wagwell", "asha@wagwell.in", b"foobar");
        assert!(url.starts_with(
            "otpauth://totp/Wagwell:asha%40wagwell.in?secret=MZXW6YTBOI&issuer=Wagwell"
        ));
    }

    #[test]
    fn readable_codes_are_grouped_and_hash_the_same_however_typed() {
        let c = readable_code(8).unwrap();
        assert_eq!(c.len(), 9);
        assert_eq!(c.as_bytes()[4], b'-');
        assert!(
            c.chars()
                .filter(|c| *c != '-')
                .all(|ch| CODE_ALPHABET.contains(&(ch as u8)))
        );
        assert_ne!(readable_code(8).unwrap(), c);
        assert_eq!(
            hash_code("recovery", &c),
            hash_code("recovery", &c.to_lowercase().replace('-', " "))
        );
        assert_ne!(hash_code("recovery", &c), hash_code("rider-setup", &c));
    }
}
