//! One-time codes for customer sign-in: 6 digits, valid 5 minutes, 5 tries, never stored in
//! plain text. The hash includes the mobile number and a server-side secret (the "pepper"), so
//! a stolen table of hashes can't be reversed by trying all million codes.

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const CODE_LIFETIME_MINUTES: i64 = 5;
pub const MAX_ATTEMPTS: i32 = 5;
/// At most this many codes per mobile number per hour, and one every 30 seconds.
pub const MAX_PER_HOUR: i64 = 5;
pub const RESEND_AFTER_SECONDS: i64 = 30;

/// Indian mobile numbers: ten digits starting 6–9.
pub fn valid_mobile(m: &str) -> bool {
    m.len() == 10 && m.bytes().all(|b| b.is_ascii_digit()) && matches!(m.as_bytes()[0], b'6'..=b'9')
}

/// A uniformly random 6-digit code (rejection sampling, so no digit is likelier than another).
pub fn new_code() -> anyhow::Result<String> {
    loop {
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("no secure random source: {e}"))?;
        let n = u32::from_le_bytes(b);
        // Largest multiple of 1,000,000 below 2^32.
        if n < 4_294_000_000 {
            return Ok(format!("{:06}", n % 1_000_000));
        }
    }
}

pub fn hash_code(pepper: &str, mobile: &str, code: &str) -> String {
    hex::encode(Sha256::digest(
        format!("{pepper}:{mobile}:{code}").as_bytes(),
    ))
}

/// Compares in constant time so timing can't hint at how many characters matched.
pub fn matches(stored_hash: &str, pepper: &str, mobile: &str, code: &str) -> bool {
    let candidate = hash_code(pepper, mobile, code);
    candidate.as_bytes().ct_eq(stored_hash.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mobile_rules() {
        assert!(valid_mobile("9876543210"));
        assert!(valid_mobile("6000000000"));
        assert!(!valid_mobile("5876543210"));
        assert!(!valid_mobile("987654321"));
        assert!(!valid_mobile("98765432100"));
        assert!(!valid_mobile("98765abcde"));
        assert!(!valid_mobile("+919876543210"));
    }

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..200 {
            let c = new_code().unwrap();
            assert_eq!(c.len(), 6);
            assert!(c.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[test]
    fn hash_checks_code_mobile_and_pepper() {
        let h = hash_code("pepper", "9876543210", "123456");
        assert!(matches(&h, "pepper", "9876543210", "123456"));
        assert!(!matches(&h, "pepper", "9876543210", "123457"));
        assert!(!matches(&h, "pepper", "9876543211", "123456"));
        assert!(!matches(&h, "other", "9876543210", "123456"));
        assert!(!h.contains("123456"));
    }
}
