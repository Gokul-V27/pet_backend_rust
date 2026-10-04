//! Staff passwords: Argon2id, the current standard. A stolen database can't be used to read
//! passwords, and each guess is deliberately slow.

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};

pub const MIN_LENGTH: usize = 12;

pub fn check_strength(password: &str) -> Result<(), &'static str> {
    if password.chars().count() < MIN_LENGTH {
        return Err("use at least 12 characters");
    }
    if password.chars().count() > 128 {
        return Err("use at most 128 characters");
    }
    Ok(())
}

pub fn hash(password: &str) -> anyhow::Result<String> {
    // The salt is generated from the operating system's random source (argon2's `getrandom` feature).
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hashing failed: {e}"))
}

pub fn verify(password: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A real hash of a throw-away password. Checking a login against it when the email isn't found
/// keeps the response time the same, so timing can't reveal which emails are staff.
pub fn dummy_hash() -> &'static str {
    use std::sync::OnceLock;
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| hash("not-a-real-password-for-timing-only").expect("hashing works"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let h = hash("correct horse battery staple").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify("correct horse battery staple", &h));
        assert!(!verify("correct horse battery stapler", &h));
        assert!(!verify("anything", "not-a-hash"));
    }

    #[test]
    fn strength_rules() {
        assert!(check_strength("short").is_err());
        assert!(check_strength("a-long-enough-pass").is_ok());
        assert!(check_strength(&"x".repeat(129)).is_err());
    }
}
