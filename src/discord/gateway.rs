use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt};
use twilight_gateway::error::ReceiveMessageErrorType;
use twilight_http::Client as HttpClient;
use twilight_model::channel::ChannelType;
use twilight_model::channel::message::Message;

use uuid::Uuid;

use crate::agent::client::{call_moderation, call_openai};
use crate::agent::context::build_messages_array;
use crate::discord::react::add_reaction;
use crate::discord::respond::send_thread_message;
use crate::info_collector::PendingInfoRequest;
use crate::state::{AppState, HistoryEntry, Role};

pub async fn run_gateway(state: Arc<AppState>) -> Result<()> {
    let mut shard = Shard::new(
        ShardId::ONE,
        state.config.discord_token.clone(),
        Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT,
    );

    loop {
        let event = match shard
            .next_event(EventTypeFlags::MESSAGE_CREATE | EventTypeFlags::READY)
            .await
        {
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

        match event {
            Event::Ready(ready) => {
                tracing::info!(
                    id = %ready.user.id,
                    username = %ready.user.name,
                    guilds = ready.guilds.len(),
                    "bot is ready",
                );
            }
            Event::MessageCreate(msg) => {
                let state = Arc::clone(&state);
                let msg = msg.0;
                tokio::spawn(async move {
                    if let Err(e) = handle_message(msg, state).await {
                        tracing::error!("error handling message: {e:#}");
                    }
                });
            }
            _ => {}
        }
    }

    Ok(())
}

async fn handle_message(msg: Message, state: Arc<AppState>) -> Result<()> {
    if msg.author.bot {
        return Ok(());
    }

    // Message in a tracked support thread → continuation
    if state.threads.contains(&msg.channel_id) {
        return handle_thread_message(msg, state).await;
    }

    // Fresh @mention → new thread
    let is_fresh_mention = msg.mentions.iter().any(|u| u.id == state.bot_user_id)
        && msg.content.starts_with(&format!("<@{}>", state.bot_user_id));

    if is_fresh_mention {
        handle_new_conversation(msg, state).await
    } else {
        Ok(())
    }
}

async fn handle_new_conversation(msg: Message, state: Arc<AppState>) -> Result<()> {
    let mention = format!("<@{}>", state.bot_user_id);
    let content = msg.content.strip_prefix(&mention).unwrap_or(&msg.content).trim();
    tracing::info!(user = %msg.author.name, content = %content, "new conversation");

    if call_moderation(&state.openai, content).await? {
        tracing::info!("message flagged by moderation, reacting with ❌");
        add_reaction(&state.http, msg.channel_id, msg.id, "❌").await?;
        return Ok(());
    }

    // Create private thread
    let thread_name = format!("{}'s Support", msg.author.name);
    let thread = state
        .http
        .create_thread(msg.channel_id, &thread_name, ChannelType::PrivateThread)
        .await?
        .model()
        .await?;

    // Add user to thread, react with 🧵, notify in channel
    state.http.add_thread_member(thread.id, msg.author.id).await?;
    add_reaction(&state.http, msg.channel_id, msg.id, "🧵").await?;
    state
        .http
        .create_message(msg.channel_id)
        .reply(msg.id)
        .content(&format!(
            "I've opened a private support thread: <#{}>",
            thread.id
        ))
        .await?;

    let _typing = start_typing(Arc::clone(&state.http), thread.id);

    let kb_context = state.knowledge_base.search(content, 5).await.unwrap_or_else(|e| {
        tracing::warn!("KB search failed: {e:#}");
        vec![]
    });

    let guild = msg.guild_id.map(|id| id.to_string()).unwrap_or_else(|| "@me".to_string());
    let message_link = format!("https://discord.com/channels/{guild}/{}/{}", msg.channel_id, msg.id);
    let system_prompt = format!(
        "{}\n\nMessage context — author_id: {}, owner_id: {}, message_link: {}",
        state.config.ai_system_prompt,
        msg.author.id,
        state.config.owner_id,
        message_link,
    );

    let messages = build_messages_array(
        &system_prompt,
        &[],
        &[],
        content,
        &msg.attachments,
        state.bot_user_id,
        &kb_context,
    )?;

    let ai_response = call_openai(
        &state.openai,
        &state.config.ai_model,
        messages,
        &state.extensions,
        Some(&state.memory_tracker),
        Some(&state.info_collector),
        &HashMap::new(),
    )
    .await?;

    // Track thread before sending so replies are caught
    state.threads.insert(thread.id);

    if let Some(emoji) = &ai_response.reaction {
        add_reaction(&state.http, msg.channel_id, msg.id, emoji).await?;
    }

    if let Some(partial) = ai_response.info_request {
        let pending = PendingInfoRequest {
            id: Uuid::new_v4(),
            target_user_id: msg.author.id.to_string(),
            channel_id: thread.id,
            reply_to_msg_id: None,
            title: partial.title,
            message: partial.message,
            fields: partial.fields,
            ext_name: partial.ext_name,
            method_name: partial.method_name,
            known_args: partial.known_args,
            user_message: content.to_string(),
            system_prompt: system_prompt.clone(),
            history: vec![],
            kb_context: kb_context.clone(),
            conv_id: Some(thread.id),
        };
        state.info_collector.initiate(pending, Arc::clone(&state)).await?;
        return Ok(());
    }

    if let Some(text) = ai_response.content {
        let prefixed = format!("<@{}> {}", msg.author.id, text);
        send_thread_message(&state.http, thread.id, &prefixed).await?;

        if !ai_response.tool_results.is_empty() {
            state.conv_tool_cache
                .entry(thread.id)
                .or_default()
                .extend(ai_response.tool_results);
        }

        state.history.insert(thread.id, vec![
            HistoryEntry {
                role: Role::User,
                content: content.to_string(),
                image_urls: collect_image_urls(&msg.attachments),
            },
            HistoryEntry {
                role: Role::Assistant,
                content: text,
                image_urls: vec![],
            },
        ]);
    }

    {
        let tracker = Arc::clone(&state.issue_tracker);
        let user_id = msg.author.id.to_string();
        let content_owned = content.to_string();
        tokio::spawn(async move {
            if let Err(e) = tracker.record_signal(&user_id, &content_owned).await {
                tracing::warn!("issue signal recording failed: {e:#}");
            }
        });
    }

    Ok(())
}

async fn handle_thread_message(msg: Message, state: Arc<AppState>) -> Result<()> {
    let thread_id = msg.channel_id;
    tracing::info!(user = %msg.author.name, content = %msg.content, "thread continuation");
    let _typing = start_typing(Arc::clone(&state.http), thread_id);

    if call_moderation(&state.openai, &msg.content).await? {
        tracing::info!("message flagged by moderation, reacting with ❌");
        add_reaction(&state.http, thread_id, msg.id, "❌").await?;
        return Ok(());
    }

    let history = state.history.get(&thread_id)
        .map(|h| h.clone())
        .unwrap_or_default();

    let kb_context = state.knowledge_base.search(&msg.content, 5).await.unwrap_or_else(|e| {
        tracing::warn!("KB search failed: {e:#}");
        vec![]
    });

    let guild = msg.guild_id.map(|id| id.to_string()).unwrap_or_else(|| "@me".to_string());
    let message_link = format!("https://discord.com/channels/{guild}/{}/{}", thread_id, msg.id);
    let system_prompt = format!(
        "{}\n\nMessage context — author_id: {}, owner_id: {}, message_link: {}",
        state.config.ai_system_prompt,
        msg.author.id,
        state.config.owner_id,
        message_link,
    );

    let messages = build_messages_array(
        &system_prompt,
        &history,
        &[],
        &msg.content,
        &msg.attachments,
        state.bot_user_id,
        &kb_context,
    )?;

    let existing_cache: HashMap<String, String> = state.conv_tool_cache
        .get(&thread_id)
        .map(|e| e.value().clone())
        .unwrap_or_default();

    let ai_response = call_openai(
        &state.openai,
        &state.config.ai_model,
        messages,
        &state.extensions,
        Some(&state.memory_tracker),
        Some(&state.info_collector),
        &existing_cache,
    )
    .await?;

    if let Some(emoji) = &ai_response.reaction {
        add_reaction(&state.http, thread_id, msg.id, emoji).await?;
    }

    if let Some(partial) = ai_response.info_request {
        let pending = PendingInfoRequest {
            id: Uuid::new_v4(),
            target_user_id: msg.author.id.to_string(),
            channel_id: thread_id,
            reply_to_msg_id: Some(msg.id),
            title: partial.title,
            message: partial.message,
            fields: partial.fields,
            ext_name: partial.ext_name,
            method_name: partial.method_name,
            known_args: partial.known_args,
            user_message: msg.content.clone(),
            system_prompt: system_prompt.clone(),
            history: history.clone(),
            kb_context: kb_context.clone(),
            conv_id: Some(thread_id),
        };
        state.info_collector.initiate(pending, Arc::clone(&state)).await?;
        return Ok(());
    }

    if let Some(text) = ai_response.content {
        send_thread_message(&state.http, thread_id, &text).await?;

        if !ai_response.tool_results.is_empty() {
            state.conv_tool_cache
                .entry(thread_id)
                .or_default()
                .extend(ai_response.tool_results);
        }

        if let Some(mut h) = state.history.get_mut(&thread_id) {
            h.push(HistoryEntry {
                role: Role::User,
                content: msg.content.clone(),
                image_urls: collect_image_urls(&msg.attachments),
            });
            h.push(HistoryEntry {
                role: Role::Assistant,
                content: text,
                image_urls: vec![],
            });
        }
    }

    {
        let tracker = Arc::clone(&state.issue_tracker);
        let user_id = msg.author.id.to_string();
        let content_owned = msg.content.clone();
        tokio::spawn(async move {
            if let Err(e) = tracker.record_signal(&user_id, &content_owned).await {
                tracing::warn!("issue signal recording failed: {e:#}");
            }
        });
    }

    Ok(())
}

/// Sends a typing indicator immediately, then re-triggers every 8 seconds.
/// Stops automatically when the returned sender is dropped (end of handler scope).
fn start_typing(
    http: Arc<HttpClient>,
    channel_id: twilight_model::id::Id<twilight_model::id::marker::ChannelMarker>,
) -> tokio::sync::oneshot::Sender<()> {
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            let _ = http.create_typing_trigger(channel_id).await;
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(8)) => {}
                _ = &mut done_rx => break,
            }
        }
    });
    done_tx
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
