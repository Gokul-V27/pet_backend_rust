//! Settings come from environment variables and are checked once at start-up, so a missing or
//! unsafe value stops the server immediately instead of failing later.

use anyhow::{Context, bail};
use axum::http::HeaderValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Env {
    Development,
    Production,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub env: Env,
    pub bind: String,
    pub database_url: String,
    /// Websites allowed to call the API with cookies.
    pub cors_origins: Vec<HeaderValue>,
    /// Mixed into stored one-time-code hashes.
    pub otp_pepper: String,
    /// Development only: write one-time codes to the log instead of sending an SMS.
    pub otp_dev_echo: bool,
    /// Development only: every sign-in code is this one (6 digits) instead of a random code.
    pub otp_dev_code: Option<String>,
    /// Image storage (S3-compatible, e.g. Supabase Storage). `None` = uploads switched off.
    pub storage: Option<StorageConfig>,
    /// Online payments. `None` = only cash on delivery is offered.
    pub razorpay: Option<RazorpayConfig>,
    /// `None` when no SMS provider is set: customers can't sign in outside development.
    pub sms: Option<SmsConfig>,
}

/// MSG91 (sign-in codes by SMS). Server-only.
#[derive(Clone)]
pub struct SmsConfig {
    pub auth_key: String,
    /// The DLT-approved OTP template in MSG91.
    pub template_id: String,
    /// The template's variable that holds the code (`##otp##` → `otp`).
    pub code_var: String,
}

#[derive(Clone)]
pub struct RazorpayConfig {
    /// Public id, also sent to the browser to open Razorpay Checkout.
    pub key_id: String,
    /// Server-only. Signs API calls and checkout signatures.
    pub key_secret: String,
    /// Server-only. Checks that webhooks really come from Razorpay.
    pub webhook_secret: String,
}

// Never print the secrets, even in debug logs.
/// The auth key never appears in logs.
impl std::fmt::Debug for SmsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmsConfig")
            .field("template_id", &self.template_id)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for RazorpayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RazorpayConfig")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct StorageConfig {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Where shoppers' browsers load the files from, e.g.
    /// https://<ref>.supabase.co/storage/v1/object/public/<bucket>
    pub public_base_url: String,
}

// Never print the keys, even in debug logs.
impl std::fmt::Debug for StorageConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageConfig")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("public_base_url", &self.public_base_url)
            .finish_non_exhaustive()
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Takes a lookup function so tests can supply values without touching the real environment.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        // Anything other than an explicit "development" is treated as production (the safe way round).
        let env = match get("APP_ENV").as_deref() {
            Some("development") => Env::Development,
            _ => Env::Production,
        };
        let database_url = get("DATABASE_URL").context("DATABASE_URL is required")?;
        let otp_pepper = get("OTP_PEPPER").context("OTP_PEPPER is required")?;
        if otp_pepper.len() < 32 {
            bail!("OTP_PEPPER must be at least 32 characters (openssl rand -hex 32)");
        }
        let otp_dev_echo = get("OTP_DEV_ECHO").is_some_and(|v| v == "true");
        if otp_dev_echo && env == Env::Production {
            bail!("OTP_DEV_ECHO=true is not allowed unless APP_ENV=development");
        }
        let otp_dev_code = get("OTP_DEV_CODE").map(|c| c.trim().to_owned()).filter(|c| !c.is_empty());
        if let Some(code) = &otp_dev_code {
            if env == Env::Production {
                bail!("OTP_DEV_CODE is not allowed unless APP_ENV=development");
            }
            if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
                bail!("OTP_DEV_CODE must be exactly 6 digits");
            }
        }
        let cors_origins = get("CORS_ORIGINS")
            .unwrap_or_else(|| "http://localhost:3000,http://localhost:3001".into())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                if s == "*" {
                    bail!("CORS_ORIGINS must list real origins, not *");
                }
                s.trim_end_matches('/')
                    .parse::<HeaderValue>()
                    .with_context(|| format!("bad CORS origin {s}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let storage = match (get("S3_ACCESS_KEY_ID"), get("S3_SECRET_ACCESS_KEY")) {
            (Some(access_key_id), Some(secret_access_key)) => {
                let need = |k: &str| {
                    get(k).with_context(|| format!("{k} is required when S3 keys are set"))
                };
                let public_base_url = need("STORAGE_PUBLIC_URL")?.trim_end_matches('/').to_owned();
                if !public_base_url.starts_with("https://") && env == Env::Production {
                    bail!("STORAGE_PUBLIC_URL must be https in production");
                }
                Some(StorageConfig {
                    // Exactly one trailing slash: the S3 client appends "<bucket>/<key>" to the endpoint's
                    // path, and without it Supabase's ".../s3" + "image_pet" becomes ".../s3image_pet".
                    endpoint: format!("{}/", need("S3_ENDPOINT")?.trim_end_matches('/')),
                    region: need("S3_REGION")?,
                    bucket: need("S3_BUCKET")?,
                    access_key_id,
                    secret_access_key,
                    public_base_url,
                })
            }
            (None, None) => None,
            _ => bail!("set both S3_ACCESS_KEY_ID and S3_SECRET_ACCESS_KEY, or neither"),
        };
        let razorpay = match (get("RAZORPAY_KEY_ID"), get("RAZORPAY_KEY_SECRET")) {
            (Some(key_id), Some(key_secret)) => {
                let webhook_secret = get("RAZORPAY_WEBHOOK_SECRET")
                    .context("RAZORPAY_WEBHOOK_SECRET is required when Razorpay keys are set")?;
                if !key_id.starts_with("rzp_") {
                    bail!("RAZORPAY_KEY_ID should start with rzp_test_ or rzp_live_");
                }
                if key_id.starts_with("rzp_live_") && env == Env::Development {
                    bail!(
                        "live Razorpay keys are not allowed with APP_ENV=development; use rzp_test_ keys"
                    );
                }
                Some(RazorpayConfig {
                    key_id,
                    key_secret,
                    webhook_secret,
                })
            }
            (None, None) => None,
            _ => bail!("set both RAZORPAY_KEY_ID and RAZORPAY_KEY_SECRET, or neither"),
        };
        let sms = match (get("MSG91_AUTH_KEY"), get("MSG91_OTP_TEMPLATE_ID")) {
            (Some(auth_key), Some(template_id)) => Some(SmsConfig {
                auth_key,
                template_id,
                code_var: get("MSG91_OTP_VAR").unwrap_or_else(|| "otp".into()),
            }),
            (None, None) => None,
            _ => bail!("set both MSG91_AUTH_KEY and MSG91_OTP_TEMPLATE_ID, or neither"),
        };
        let bind = if let Some(bind) = get("BIND") {
            if (get("RENDER").is_some() || get("PORT").is_some())
                && (bind.starts_with("127.0.0.1:") || bind.starts_with("localhost:"))
            {
                let port = get("PORT")
                    .unwrap_or_else(|| bind.rsplit(':').next().unwrap_or("8080").to_string());
                tracing::warn!(
                    "cloud deployment detected with loopback BIND; overriding host to 0.0.0.0:{port}"
                );
                format!("0.0.0.0:{port}")
            } else {
                bind
            }
        } else if let Some(port) = get("PORT") {
            let host = get("HOST").unwrap_or_else(|| "0.0.0.0".into());
            format!("{host}:{port}")
        } else {
            let host = get("HOST").unwrap_or_else(|| match env {
                Env::Production => "0.0.0.0".into(),
                Env::Development => "127.0.0.1".into(),
            });
            format!("{host}:8080")
        };
        Ok(Self {
            env,
            storage,
            razorpay,
            sms,
            bind,
            database_url,
            cors_origins,
            otp_pepper,
            otp_dev_echo,
            otp_dev_code,
        })
    }

    /// Cookies are only sent over HTTPS outside development.
    pub fn cookie_secure(&self) -> bool {
        self.env == Env::Production
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> anyhow::Result<Config> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| m.get(k).cloned())
    }

    const PEPPER: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn defaults_to_production() {
        let c = cfg(&[("DATABASE_URL", "postgres://x"), ("OTP_PEPPER", PEPPER)]).unwrap();
        assert_eq!(c.env, Env::Production);
        assert!(c.cookie_secure());
        assert_eq!(c.cors_origins.len(), 2);
    }

    #[test]
    fn short_pepper_is_refused() {
        assert!(cfg(&[("DATABASE_URL", "postgres://x"), ("OTP_PEPPER", "short")]).is_err());
    }

    #[test]
    fn dev_echo_is_refused_in_production() {
        let bad = cfg(&[
            ("DATABASE_URL", "postgres://x"),
            ("OTP_PEPPER", PEPPER),
            ("OTP_DEV_ECHO", "true"),
        ]);
        assert!(bad.is_err());
        let ok = cfg(&[
            ("DATABASE_URL", "postgres://x"),
            ("OTP_PEPPER", PEPPER),
            ("OTP_DEV_ECHO", "true"),
            ("APP_ENV", "development"),
        ])
        .unwrap();
        assert!(ok.otp_dev_echo && !ok.cookie_secure());
    }

    #[test]
    fn fixed_dev_code_is_development_only_and_six_digits() {
        let base = [("DATABASE_URL", "postgres://x"), ("OTP_PEPPER", PEPPER)];
        let with = |extra: &[(&'static str, &'static str)]| {
            let mut v = base.to_vec();
            v.extend_from_slice(extra);
            cfg(&v)
        };
        assert!(with(&[("OTP_DEV_CODE", "369369")]).is_err());
        assert!(with(&[("OTP_DEV_CODE", "4321"), ("APP_ENV", "development")]).is_err());
        let ok = with(&[("OTP_DEV_CODE", "369369"), ("APP_ENV", "development")]).unwrap();
        assert_eq!(ok.otp_dev_code.as_deref(), Some("369369"));
        assert!(cfg(&base).unwrap().otp_dev_code.is_none());
    }

    #[test]
    fn storage_needs_both_keys_and_hides_them_in_debug() {
        let base = [("DATABASE_URL", "postgres://x"), ("OTP_PEPPER", PEPPER)];
        assert!(cfg(&base).unwrap().storage.is_none());
        let mut half = base.to_vec();
        half.push(("S3_ACCESS_KEY_ID", "id"));
        assert!(cfg(&half).is_err());
        let mut full = half.clone();
        full.extend([
            ("S3_SECRET_ACCESS_KEY", "super-secret-value"),
            ("S3_ENDPOINT", "https://s3.example/"),
            ("S3_REGION", "ap-southeast-1"),
            ("S3_BUCKET", "image_pet"),
            (
                "STORAGE_PUBLIC_URL",
                "https://cdn.example/public/image_pet/",
            ),
        ]);
        let s = cfg(&full).unwrap().storage.unwrap();
        assert_eq!(s.public_base_url, "https://cdn.example/public/image_pet");
        assert_eq!(s.endpoint, "https://s3.example/");
        assert!(!format!("{s:?}").contains("super-secret-value"));
    }

    #[test]
    fn wildcard_cors_is_refused() {
        assert!(
            cfg(&[
                ("DATABASE_URL", "postgres://x"),
                ("OTP_PEPPER", PEPPER),
                ("CORS_ORIGINS", "*")
            ])
            .is_err()
        );
    }

    #[test]
    fn bind_defaults_based_on_environment() {
        let prod = cfg(&[("DATABASE_URL", "postgres://x"), ("OTP_PEPPER", PEPPER)]).unwrap();
        assert_eq!(prod.bind, "0.0.0.0:8080");

        let dev = cfg(&[
            ("DATABASE_URL", "postgres://x"),
            ("OTP_PEPPER", PEPPER),
            ("APP_ENV", "development"),
        ])
        .unwrap();
        assert_eq!(dev.bind, "127.0.0.1:8080");
    }

    #[test]
    fn bind_respects_port_env_var() {
        let c = cfg(&[
            ("DATABASE_URL", "postgres://x"),
            ("OTP_PEPPER", PEPPER),
            ("PORT", "10000"),
        ])
        .unwrap();
        assert_eq!(c.bind, "0.0.0.0:10000");
    }

    #[test]
    fn bind_overrides_loopback_in_cloud() {
        let c = cfg(&[
            ("DATABASE_URL", "postgres://x"),
            ("OTP_PEPPER", PEPPER),
            ("BIND", "127.0.0.1:8080"),
            ("RENDER", "true"),
            ("PORT", "10000"),
        ])
        .unwrap();
        assert_eq!(c.bind, "0.0.0.0:10000");
    }

    #[test]
    fn razorpay_needs_all_three_and_test_keys_in_development() {
        let base = [
            ("DATABASE_URL", "postgres://x"),
            ("OTP_PEPPER", PEPPER),
            ("APP_ENV", "development"),
        ];
        assert!(cfg(&base).unwrap().razorpay.is_none());
        let mut half = base.to_vec();
        half.push(("RAZORPAY_KEY_ID", "rzp_test_abc"));
        assert!(cfg(&half).is_err());
        let mut no_hook = half.clone();
        no_hook.push(("RAZORPAY_KEY_SECRET", "s3cret"));
        assert!(cfg(&no_hook).is_err());
        let mut full = no_hook.clone();
        full.push(("RAZORPAY_WEBHOOK_SECRET", "w3bhook"));
        let c = cfg(&full).unwrap();
        let shown = format!("{:?}", c.razorpay);
        assert!(
            shown.contains("rzp_test_abc")
                && !shown.contains("s3cret")
                && !shown.contains("w3bhook")
        );
        let mut live = full.clone();
        live[3] = ("RAZORPAY_KEY_ID", "rzp_live_abc");
        assert!(cfg(&live).is_err());
    }
}
