mod agent;
mod config;
mod discord;
mod state;

use std::sync::Arc;

use anyhow::Result;
use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use axum::{Router, routing::post};
use dashmap::DashMap;
use ed25519_dalek::VerifyingKey;
use tokio::net::TcpListener;
use twilight_http::Client as HttpClient;

use config::Config;
use discord::interactions::{handle_interaction, InteractionState};
use state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let config = Config::from_env()?;

    let http = Arc::new(HttpClient::new(config.discord_token.clone()));

    let bot_user_id = http.current_user().await?.model().await?.id;
    let application_id = http.current_user_application().await?.model().await?.id;

    let public_key_bytes: [u8; 32] = hex::decode(&config.discord_public_key)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("public key must be 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&public_key_bytes)?;

    let openai = Arc::new(OpenAIClient::with_config(
        OpenAIConfig::new().with_api_key(&config.openai_api_key),
    ));

    let app_state = Arc::new(AppState {
        http,
        application_id,
        bot_user_id,
        conversations: Arc::new(DashMap::new()),
        history: Arc::new(DashMap::new()),
        openai,
        config: Arc::new(config),
    });

    discord::commands::register_commands(&app_state.http, application_id).await?;

    let interaction_state = Arc::new(InteractionState {
        verifying_key,
        app: Arc::clone(&app_state),
    });

    tokio::spawn(discord::gateway::run_gateway(Arc::clone(&app_state)));

    let router = Router::new()
        .route("/interactions", post(handle_interaction))
        .with_state(interaction_state);

    let listener = TcpListener::bind("0.0.0.0:8000").await?;
    tracing::info!("listening on port 8000");
    axum::serve(listener, router).await?;

    Ok(())
}
