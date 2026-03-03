use std::sync::Arc;

use anyhow::Result;
use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt};
use twilight_gateway::error::ReceiveMessageErrorType;
use twilight_model::channel::message::Message;
use twilight_model::id::Id;
use twilight_model::id::marker::MessageMarker;

use crate::agent::client::call_openai;
use crate::agent::context::{build_messages_array, fetch_reply_chain};
use crate::discord::respond::send_gateway_reply;
use crate::state::{AppState, HistoryEntry, Role};

pub async fn run_gateway(state: Arc<AppState>) -> Result<()> {
    let mut shard = Shard::new(
        ShardId::ONE,
        state.config.discord_token.clone(),
        Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT,
    );

    loop {
        let event = match shard.next_event(EventTypeFlags::MESSAGE_CREATE).await {
            Some(Ok(event)) => event,
            Some(Err(e)) => {
                tracing::warn!("gateway error: {e}");
                if matches!(e.kind(), ReceiveMessageErrorType::Reconnect { .. }) {
                    break;
                }
                continue;
            }
            None => break,
        };

        if let Event::MessageCreate(msg) = event {
            let state = Arc::clone(&state);
            let msg = msg.0;
            tokio::spawn(async move {
                if let Err(e) = handle_message(msg, state).await {
                    tracing::error!("error handling message: {e:#}");
                }
            });
        }
    }

    Ok(())
}

async fn handle_message(msg: Message, state: Arc<AppState>) -> Result<()> {
    if msg.author.bot {
        return Ok(());
    }

    let ref_id = msg.reference.as_ref().and_then(|r| r.message_id);

    let is_reply_to_bot = ref_id
        .map(|id| state.conversations.contains_key(&id))
        .unwrap_or(false);

    let is_fresh_mention = msg.reference.is_none()
        && msg.mentions.iter().any(|u| u.id == state.bot_user_id)
        && msg.content.starts_with(&format!("<@{}>", state.bot_user_id));

    if is_reply_to_bot {
        handle_continuation(msg, ref_id.unwrap(), state).await
    } else if is_fresh_mention {
        handle_new_conversation(msg, state).await
    } else {
        Ok(())
    }
}

async fn handle_new_conversation(msg: Message, state: Arc<AppState>) -> Result<()> {
    let mention = format!("<@{}>", state.bot_user_id);
    let content = msg.content.strip_prefix(&mention).unwrap_or(&msg.content).trim();

    let reply_chain = if msg.reference.is_some() {
        fetch_reply_chain(&msg, &state.http, state.config.ai_reply_chain_depth).await
    } else {
        vec![]
    };

    let messages = build_messages_array(
        &state.config.ai_system_prompt,
        &[],
        &reply_chain,
        content,
        &msg.attachments,
        state.bot_user_id,
    )?;

    let response = call_openai(&state.openai, &state.config.ai_model, messages).await?;
    let sent = send_gateway_reply(&state.http, msg.channel_id, msg.id, &response).await?;

    state.conversations.insert(sent.id, sent.id);
    state.history.insert(sent.id, vec![
        HistoryEntry {
            role: Role::User,
            content: content.to_string(),
            image_urls: collect_image_urls(&msg.attachments),
        },
        HistoryEntry {
            role: Role::Assistant,
            content: response,
            image_urls: vec![],
        },
    ]);

    Ok(())
}

async fn handle_continuation(
    msg: Message,
    ref_id: Id<MessageMarker>,
    state: Arc<AppState>,
) -> Result<()> {
    let conv_id = *state.conversations.get(&ref_id)
        .ok_or_else(|| anyhow::anyhow!("conversation not found"))?;

    let history = state.history.get(&conv_id)
        .map(|h| h.clone())
        .unwrap_or_default();

    let messages = build_messages_array(
        &state.config.ai_system_prompt,
        &history,
        &[],
        &msg.content,
        &msg.attachments,
        state.bot_user_id,
    )?;

    let response = call_openai(&state.openai, &state.config.ai_model, messages).await?;
    let sent = send_gateway_reply(&state.http, msg.channel_id, msg.id, &response).await?;

    state.conversations.insert(sent.id, conv_id);

    if let Some(mut h) = state.history.get_mut(&conv_id) {
        h.push(HistoryEntry {
            role: Role::User,
            content: msg.content.clone(),
            image_urls: collect_image_urls(&msg.attachments),
        });
        h.push(HistoryEntry {
            role: Role::Assistant,
            content: response,
            image_urls: vec![],
        });
    }

    Ok(())
}

fn collect_image_urls(attachments: &[twilight_model::channel::Attachment]) -> Vec<String> {
    attachments
        .iter()
        .filter(|a| {
            a.content_type
                .as_deref()
                .map(|ct| ct.starts_with("image/"))
                .unwrap_or(false)
        })
        .map(|a| a.url.clone())
        .collect()
}
