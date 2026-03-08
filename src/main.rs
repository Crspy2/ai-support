mod agent;
mod config;
mod db;
mod discord;
mod extensions;
mod info_collector;
mod issues;
mod knowledge;
mod memory;
mod state;

use std::sync::Arc;

use anyhow::Result;
use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use axum::{Router, routing::post};
use tower_http::trace::TraceLayer;
use dashmap::DashMap;
use ed25519_dalek::VerifyingKey;
use tokio::net::TcpListener;
use twilight_http::Client as HttpClient;

use config::Config;
use discord::interactions::{handle_interaction, InteractionState};
use extensions::ExtensionRegistry;
use knowledge::KnowledgeBase;
use state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Arc::new(Config::from_env()?);

    let pool = db::connect(&config.database_url).await?;
    db::run_migrations(&pool).await?;
    let pool = Arc::new(pool);
    let cleanup_pool = Arc::clone(&pool);

    let http = Arc::new(HttpClient::new(config.discord_token.clone()));
    let bot_user_id = http.current_user().await?.model().await?.id;
    let application_id = http.current_user_application().await?.model().await?.id;

    let openai = Arc::new(OpenAIClient::with_config(
        OpenAIConfig::new().with_api_key(&config.openai_api_key),
    ));

    let ext_ctx = extensions::ExtensionContext {
        db: Arc::clone(&pool),
        openai: Arc::clone(&openai),
        config: Arc::clone(&config),
    };
    let extensions = Arc::new(ExtensionRegistry::from_inventory(&ext_ctx));

    let knowledge_base = Arc::new(KnowledgeBase::new(Arc::clone(&pool), Arc::clone(&openai)));
    knowledge_base.populate_at_startup(&extensions).await?;

    let issue_tracker = Arc::new(
        issues::IssueTracker::new(
            Arc::clone(&pool),
            Arc::clone(&openai),
            Arc::clone(&config),
            Arc::clone(&extensions),
            Arc::clone(&http),
        )?,
    );

    let memory_tracker = Arc::new(
        memory::MemoryTracker::new(
            Arc::clone(&pool),
            Arc::clone(&openai),
            Arc::clone(&config),
            Arc::clone(&extensions),
            Arc::clone(&http),
        )?,
    );

    let info_collector = Arc::new(info_collector::InfoCollector::new(
        Arc::clone(&pool),
        Arc::clone(&http),
        Arc::clone(&extensions),
        application_id,
    ));

    let app_state = Arc::new(AppState {
        http,
        application_id,
        bot_user_id,
        conversations: Arc::new(DashMap::new()),
        history: Arc::new(DashMap::new()),
        conv_tool_cache: Arc::new(DashMap::new()),
        openai,
        config: Arc::clone(&config),
        extensions,
        knowledge_base,
        issue_tracker,
        memory_tracker,
        info_collector,
    });

    discord::commands::register_commands(
        &app_state.http,
        application_id,
        config.guild_id.as_deref(),
    )
    .await?;

    let public_key_bytes: [u8; 32] = hex::decode(&config.discord_public_key)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("public key must be 32 bytes"))?;
    let interaction_state = Arc::new(InteractionState {
        verifying_key: VerifyingKey::from_bytes(&public_key_bytes)?,
        app: Arc::clone(&app_state),
    });

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            let _ = sqlx::query(
                "DELETE FROM issue_signals WHERE created_at < now() - INTERVAL '2 hours'",
            )
            .execute(cleanup_pool.as_ref())
            .await;
        }
    });

    tokio::spawn(discord::gateway::run_gateway(Arc::clone(&app_state)));

    let router = Router::new()
        .route("/interactions", post(handle_interaction))
        .layer(TraceLayer::new_for_http())
        .with_state(interaction_state);

    let listener = TcpListener::bind("0.0.0.0:8000").await?;
    tracing::info!(addr = %listener.local_addr()?, "http server listening");
    axum::serve(listener, router).await?;

    Ok(())
}
