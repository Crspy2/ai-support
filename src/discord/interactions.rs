use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::{json, Value};

use crate::agent::client::{call_moderation, call_openai};
use crate::agent::context::build_messages_array;
use crate::discord::respond::send_interaction_followup;
use crate::state::AppState;

pub struct InteractionState {
    pub verifying_key: VerifyingKey,
    pub app: Arc<AppState>,
}

pub async fn handle_interaction(
    State(state): State<Arc<InteractionState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !verify_signature(&state.verifying_key, &headers, &body) {
        return (StatusCode::UNAUTHORIZED, Json(json!({}))).into_response();
    }

    let interaction: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({}))).into_response(),
    };

    match interaction["type"].as_u64() {
        Some(1) => Json(json!({ "type": 1 })).into_response(),
        Some(2) => {
            let state = Arc::clone(&state);
            let interaction = interaction.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_command(interaction, state).await {
                    tracing::error!("interaction command error: {e:#}");
                }
            });

            Json(json!({ "type": 5 })).into_response()
        }
        Some(3) => {
            let custom_id = interaction["data"]["custom_id"].as_str().unwrap_or_default();
            if custom_id.starts_with("info_collect:") {
                let user_id = interaction["member"]["user"]["id"]
                    .as_str()
                    .or_else(|| interaction["user"]["id"].as_str())
                    .unwrap_or_default();
                let body = state.app.info_collector.build_modal_response(custom_id, user_id);
                Json(body).into_response()
            } else {
                let state = Arc::clone(&state);
                let interaction = interaction.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_component(interaction, state).await {
                        tracing::error!("component interaction error: {e:#}");
                    }
                });
                (StatusCode::OK, Json(json!({ "type": 6 }))).into_response()
            }
        }
        Some(5) => {
            let state = Arc::clone(&state);
            let interaction = interaction.clone();
            tokio::spawn(async move {
                if let Err(e) = state.app.info_collector.handle_modal_submit(interaction).await {
                    tracing::error!("modal submit error: {e:#}");
                }
            });
            (StatusCode::OK, Json(json!({ "type": 6 }))).into_response()
        }
        _ => (StatusCode::BAD_REQUEST, Json(json!({}))).into_response(),
    }
}

async fn handle_command(interaction: Value, state: Arc<InteractionState>) -> anyhow::Result<()> {
    let token = interaction["token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing token"))?
        .to_string();

    let command_name = interaction["data"]["name"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let content = match command_name.as_str() {
        "ask" => interaction["data"]["options"][0]["value"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        "Ask Support Bot" => {
            let target_id = interaction["data"]["target_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            interaction["data"]["resolved"]["messages"][&target_id]["content"]
                .as_str()
                .unwrap_or("")
                .to_string()
        }
        _ => return Ok(()),
    };

    if call_moderation(&state.app.openai, &content).await? {
        send_interaction_followup(
            &state.app.http,
            state.app.application_id,
            &token,
            "Your message couldn't be processed.",
        )
        .await?;
        return Ok(());
    }

    let kb_context = state.app.knowledge_base.search(&content, 5).await.unwrap_or_else(|e| {
        tracing::warn!("KB search failed: {e:#}");
        vec![]
    });

    let messages = build_messages_array(
        &state.app.config.ai_system_prompt,
        &[],
        &[],
        &content,
        &[],
        state.app.bot_user_id,
        &kb_context,
    )?;

    let ai_response = call_openai(
        &state.app.openai,
        &state.app.config.ai_model,
        messages,
        &state.app.extensions,
        None,
        None,
    )
    .await?;

    if let Some(text) = ai_response.content {
        send_interaction_followup(
            &state.app.http,
            state.app.application_id,
            &token,
            &text,
        )
        .await?;
    }

    Ok(())
}

async fn handle_component(body: Value, state: Arc<InteractionState>) -> anyhow::Result<()> {
    let custom_id = body["data"]["custom_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing custom_id"))?;
    if custom_id.starts_with("memory_") {
        state.app.memory_tracker.handle_button(custom_id).await
    } else {
        state.app.issue_tracker.handle_button(custom_id).await
    }
}

fn verify_signature(key: &VerifyingKey, headers: &HeaderMap, body: &Bytes) -> bool {
    let Some(sig_str) = headers.get("x-signature-ed25519").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(timestamp) = headers.get("x-signature-timestamp").and_then(|v| v.to_str().ok()) else {
        return false;
    };

    let Ok(sig_bytes) = hex::decode(sig_str) else {
        return false;
    };
    let Ok(sig_array) = sig_bytes.try_into() else {
        return false;
    };
    let signature = Signature::from_bytes(&sig_array);
    let message = [timestamp.as_bytes(), body.as_ref()].concat();

    key.verify(&message, &signature).is_ok()
}
