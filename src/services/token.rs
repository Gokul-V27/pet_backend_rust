//! Session tokens: 32 random bytes from the operating system, handed to the browser once.
//! Only a SHA-256 hash is stored, so a copy of the database can't be used to sign in.

use sha2::{Digest, Sha256};

/// Returns `(token_for_the_cookie, hash_for_the_database)`.
pub fn new_token() -> anyhow::Result<(String, String)> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no secure random source: {e}"))?;
    let token = hex::encode(bytes);
    let hash = hash_token(&token);
    Ok((token, hash))
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_unique_and_hash_to_something_else() {
        let (a, ha) = new_token().unwrap();
        let (b, _) = new_token().unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert_ne!(a, ha);
        assert_eq!(hash_token(&a), ha);
    }
}
