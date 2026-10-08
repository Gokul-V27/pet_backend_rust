//! Razorpay: create a gateway order, check the checkout signature, check webhooks, refund.
//! Secrets only ever live in the server's environment. Every check compares in constant time,
//! and a payment is only "paid" after the server has verified it — never because the browser said so.

use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::config::RazorpayConfig;

const API: &str = "https://api.razorpay.com/v1";

#[derive(Clone)]
pub struct Razorpay {
    cfg: RazorpayConfig,
    http: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct Created {
    id: String,
}

impl Razorpay {
    pub fn new(cfg: RazorpayConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self { cfg, http })
    }

    pub fn key_id(&self) -> &str {
        &self.cfg.key_id
    }

    /// Creates a Razorpay order for exactly `amount_paise`; returns its id (order_…).
    pub async fn create_order(&self, amount_paise: i64, receipt: &str) -> anyhow::Result<String> {
        let res = self
            .http
            .post(format!("{API}/orders"))
            .basic_auth(&self.cfg.key_id, Some(&self.cfg.key_secret))
            .json(&json!({ "amount": amount_paise, "currency": "INR", "receipt": receipt }))
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            anyhow::bail!("razorpay create order failed with status {status}");
        }
        Ok(res.json::<Created>().await?.id)
    }

    /// Refunds `amount_paise` of a captured payment; returns the refund id (rfnd_…).
    pub async fn refund(&self, payment_id: &str, amount_paise: i64) -> anyhow::Result<String> {
        let res = self
            .http
            .post(format!("{API}/payments/{payment_id}/refund"))
            .basic_auth(&self.cfg.key_id, Some(&self.cfg.key_secret))
            .json(&json!({ "amount": amount_paise }))
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            anyhow::bail!("razorpay refund failed with status {status}");
        }
        Ok(res.json::<Created>().await?.id)
    }

    /// Checkout success: signature = HMAC-SHA256(order_id + "|" + payment_id, key secret).
    pub fn checkout_signature_ok(&self, order_id: &str, payment_id: &str, signature: &str) -> bool {
        hmac_matches(
            &self.cfg.key_secret,
            format!("{order_id}|{payment_id}").as_bytes(),
            signature,
        )
    }

    /// Webhook: X-Razorpay-Signature = HMAC-SHA256(raw body, webhook secret).
    pub fn webhook_signature_ok(&self, body: &[u8], signature: &str) -> bool {
        hmac_matches(&self.cfg.webhook_secret, body, signature)
    }
}

/// True when `signature_hex` is the HMAC-SHA256 of `message` under `secret`, compared in constant time.
pub fn hmac_matches(secret: &str, message: &[u8], signature_hex: &str) -> bool {
    let Ok(given) = hex::decode(signature_hex.trim()) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(message);
    let expected = mac.finalize().into_bytes();
    expected.as_slice().ct_eq(&given).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, msg: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(msg);
        hex::encode(mac.finalize().into_bytes())
    }

    fn rzp() -> Razorpay {
        Razorpay::new(RazorpayConfig {
            key_id: "rzp_test_x".into(),
            key_secret: "key-secret".into(),
            webhook_secret: "hook-secret".into(),
        })
        .unwrap()
    }

    #[test]
    fn checkout_signature_matches_only_the_right_pair() {
        let r = rzp();
        let good = sign("key-secret", b"order_1|pay_1");
        assert!(r.checkout_signature_ok("order_1", "pay_1", &good));
        assert!(!r.checkout_signature_ok("order_1", "pay_2", &good));
        assert!(!r.checkout_signature_ok("order_1", "pay_1", &sign("other", b"order_1|pay_1")));
        assert!(!r.checkout_signature_ok("order_1", "pay_1", "not-hex"));
        assert!(!r.checkout_signature_ok("order_1", "pay_1", ""));
    }

    #[test]
    fn webhook_uses_its_own_secret_over_the_raw_body() {
        let r = rzp();
        let body = br#"{"event":"payment.captured"}"#;
        assert!(r.webhook_signature_ok(body, &sign("hook-secret", body)));
        assert!(!r.webhook_signature_ok(body, &sign("key-secret", body)));
        assert!(!r.webhook_signature_ok(b"{}", &sign("hook-secret", body)));
    }
}
