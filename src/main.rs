use std::io::BufRead;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, bail};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;
use wagwell_api::config::{Config, Env};
use wagwell_api::models::AdminRole;
use wagwell_api::services::password;
use wagwell_api::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info"));
    if cfg.env == Env::Production {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    let db = PgPoolOptions::new()
        .max_connections(10)
        .connect(&cfg.database_url)
        .await
        .context("could not connect to the database")?;
    sqlx::migrate!()
        .run(&db)
        .await
        .context("database migration failed")?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("create-admin") {
        return create_admin(&db, &args[1..]).await;
    }

    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .with_context(|| format!("could not listen on {}", cfg.bind))?;
    tracing::info!(bind = %cfg.bind, env = ?cfg.env, "wagwell-api started");
    let storage = cfg
        .storage
        .as_ref()
        .map(wagwell_api::services::storage::Storage::new);
    match &cfg.storage {
        Some(s) => tracing::info!(bucket = %s.bucket, "image storage on"),
        None => tracing::warn!("image storage off (no S3 keys set)"),
    }
    let state = AppState {
        db,
        cfg: Arc::new(cfg),
        storage,
    };
    axum::serve(listener, wagwell_api::app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// `wagwell-api create-admin <email> "<full name>" <role>`, with the password on standard input
/// (so it never appears in the process list or shell history). There is no staff sign-up page.
async fn create_admin(db: &sqlx::PgPool, args: &[String]) -> anyhow::Result<()> {
    let [email, name, role] = args else {
        bail!(
            "usage: create-admin <email> \"<full name>\" <super_admin|admin|order_manager|inventory_manager|support>   (password on stdin)"
        );
    };
    let email = email.trim().to_lowercase();
    if !wagwell_api::services::password::looks_like_email(&email) {
        bail!("\"{email}\" isn't an email address — use your real email, e.g. name@gmail.com");
    }
    if name.trim().is_empty() || name.trim().eq_ignore_ascii_case("your name") {
        bail!("give your real name, e.g. \"Gokul\"");
    }
    let role = AdminRole::from_str(role).map_err(|_| anyhow::anyhow!("unknown role {role}"))?;
    let mut pw = String::new();
    std::io::stdin().lock().read_line(&mut pw)?;
    let pw = pw.trim_end_matches(['\n', '\r']);
    password::check_strength(pw).map_err(|m| anyhow::anyhow!("password: {m}"))?;

    let hash = password::hash(pw)?;
    let id: uuid::Uuid = sqlx::query_scalar("INSERT INTO admin_users (email, name, role, password_hash) VALUES ($1, $2, $3, $4) RETURNING id")
        .bind(&email)
        .bind(name.trim())
        .bind(role.as_str())
        .bind(hash)
        .fetch_one(db)
        .await
        .context("could not create the account (is the email already used?)")?;
    println!(
        "created {} ({}) with id {id}",
        email.trim().to_lowercase(),
        role.as_str()
    );
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.expect("ctrl-c handler") };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    tracing::info!("shutting down");
}
