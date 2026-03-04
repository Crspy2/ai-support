use std::sync::Arc;

use anyhow::Result;
use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt};
use twilight_gateway::error::ReceiveMessageErrorType;
use twilight_http::Client as HttpClient;
use twilight_model::channel::message::Message;
use twilight_model::id::Id;
use twilight_model::id::marker::MessageMarker;

use crate::agent::client::{call_moderation, call_openai};
use crate::agent::context::{build_messages_array, fetch_reply_chain};
use crate::discord::react::add_reaction;
use crate::discord::respond::send_gateway_reply;
use crate::state::{AppState, ConversationStore, HistoryEntry, Role};

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
    } else if ref_id.is_some() {
        // Reply to another user — check whether a tracked bot message appears within
        // the first ai_reply_chain_depth/4 hops of the reply chain.
        let max_hops = (state.config.ai_reply_chain_depth / 4).max(1);
        if let Some(bot_msg_id) =
            find_bot_in_chain(&msg, &state.http, &state.conversations, max_hops).await
        {
            handle_continuation(msg, bot_msg_id, state).await
        } else {
            Ok(())
        }
    } else {
        Ok(())
    }
}

async fn handle_new_conversation(msg: Message, state: Arc<AppState>) -> Result<()> {
    let mention = format!("<@{}>", state.bot_user_id);
    let content = msg.content.strip_prefix(&mention).unwrap_or(&msg.content).trim();

    if call_moderation(&state.openai, content).await? {
        tracing::info!("message flagged by moderation, reacting with ❌");
        add_reaction(&state.http, msg.channel_id, msg.id, "❌").await?;
        return Ok(());
    }

    let reply_chain = if msg.reference.is_some() {
        fetch_reply_chain(&msg, &state.http, state.config.ai_reply_chain_depth).await
    } else {
        vec![]
    };

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
        &reply_chain,
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
    )
    .await?;

    if let Some(emoji) = &ai_response.reaction {
        add_reaction(&state.http, msg.channel_id, msg.id, emoji).await?;
    }

    if let Some(text) = ai_response.content {
        let sent = send_gateway_reply(&state.http, msg.channel_id, msg.id, &text).await?;

        state.conversations.insert(sent.id, sent.id);
        state.history.insert(sent.id, vec![
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

async fn handle_continuation(
    msg: Message,
    ref_id: Id<MessageMarker>,
    state: Arc<AppState>,
) -> Result<()> {
    if call_moderation(&state.openai, &msg.content).await? {
        tracing::info!("message flagged by moderation, reacting with ❌");
        add_reaction(&state.http, msg.channel_id, msg.id, "❌").await?;
        return Ok(());
    }

    let conv_id = *state.conversations.get(&ref_id)
        .ok_or_else(|| anyhow::anyhow!("conversation not found"))?;

    let history = state.history.get(&conv_id)
        .map(|h| h.clone())
        .unwrap_or_default();

    let kb_context = state.knowledge_base.search(&msg.content, 5).await.unwrap_or_else(|e| {
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
        &history,
        &[],
        &msg.content,
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
    )
    .await?;

    if let Some(emoji) = &ai_response.reaction {
        add_reaction(&state.http, msg.channel_id, msg.id, emoji).await?;
    }

    if let Some(text) = ai_response.content {
        let sent = send_gateway_reply(&state.http, msg.channel_id, msg.id, &text).await?;

        state.conversations.insert(sent.id, conv_id);

        if let Some(mut h) = state.history.get_mut(&conv_id) {
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

async fn find_bot_in_chain(
    msg: &Message,
    http: &HttpClient,
    conversations: &ConversationStore,
    max_hops: usize,
) -> Option<Id<MessageMarker>> {
    let mut current = msg.clone();

    for _ in 0..max_hops {
        let parent: Option<Message> = if let Some(ref boxed) = current.referenced_message {
            Some(*boxed.clone())
        } else if let Some(ref reference) = current.reference {
            if let Some(message_id) = reference.message_id {
                if conversations.contains_key(&message_id) {
                    return Some(message_id);
                }
                match http.message(current.channel_id, message_id).await {
                    Ok(resp) => resp.model().await.ok() as Option<Message>,
                    Err(_) => None,
                }
            } else {
                None
            }
        } else {
            None
        };

        match parent {
            Some(p) => {
                if conversations.contains_key(&p.id) {
                    return Some(p.id);
                }
                current = p;
            }
            None => break,
        }
    }

    None
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
