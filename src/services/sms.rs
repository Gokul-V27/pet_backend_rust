//! Sends sign-in codes by SMS through MSG91 (DLT-registered, as Indian SMS requires). The server makes
//! the code and keeps only its hash; MSG91 just delivers the text from the approved template.
//! The auth key lives only in the server's environment.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::SmsConfig;

const FLOW_URL: &str = "https://control.msg91.com/api/v5/flow";

#[derive(Clone)]
pub struct Sms {
    cfg: SmsConfig,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct Reply {
    #[serde(rename = "type")]
    kind: Option<String>,
}

impl Sms {
    pub fn new(cfg: SmsConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self { cfg, http })
    }

    /// Sends `code` to an Indian 10-digit `mobile` using the approved OTP template.
    pub async fn send_code(&self, mobile: &str, code: &str) -> anyhow::Result<()> {
        let res = self
            .http
            .post(FLOW_URL)
            .header("authkey", &self.cfg.auth_key)
            .json(&flow_body(
                &self.cfg.template_id,
                &self.cfg.code_var,
                mobile,
                code,
            ))
            .send()
            .await?;
        let status = res.status();
        // MSG91 answers 200 with {"type":"error"} for some refusals, so both have to be checked.
        let reply: Reply = res.json().await.unwrap_or(Reply { kind: None });
        if !status.is_success() || reply.kind.as_deref() != Some("success") {
            anyhow::bail!("msg91 refused the code SMS (status {status})");
        }
        Ok(())
    }
}

/// The request MSG91's flow API expects: the template and one recipient with the code filled in.
fn flow_body(template_id: &str, code_var: &str, mobile: &str, code: &str) -> Value {
    let mut recipient = serde_json::Map::new();
    recipient.insert("mobiles".into(), json!(format!("91{mobile}")));
    recipient.insert(code_var.into(), json!(code));
    json!({ "template_id": template_id, "short_url": "0", "recipients": [recipient] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_has_the_country_code_and_the_template_variable() {
        let b = flow_body("tmpl-1", "otp", "9876543210", "123456");
        assert_eq!(b["template_id"], "tmpl-1");
        assert_eq!(b["recipients"][0]["mobiles"], "919876543210");
        assert_eq!(b["recipients"][0]["otp"], "123456");
    }
}
