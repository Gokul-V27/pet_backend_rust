use std::sync::Arc;

use sqlx::PgPool;

use crate::config::Config;
use crate::services::razorpay::Razorpay;
use crate::services::sms::Sms;
use crate::services::storage::Storage;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub cfg: Arc<Config>,
    /// `None` when image storage isn't configured.
    pub storage: Option<Storage>,
    /// `None` when Razorpay isn't configured (cash on delivery only).
    pub razorpay: Option<Razorpay>,
    /// `None` when no SMS provider is configured.
    pub sms: Option<Sms>,
}
