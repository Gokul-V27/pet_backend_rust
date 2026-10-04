use std::sync::Arc;

use sqlx::PgPool;

use crate::config::Config;
use crate::services::storage::Storage;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub cfg: Arc<Config>,
    /// `None` when image storage isn't configured.
    pub storage: Option<Storage>,
}
