use anyhow::Result;
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionRequestAssistantMessageArgs,
    ChatCompletionRequestUserMessageArgs, ChatCompletionRequestSystemMessageArgs,
    ChatCompletionRequestUserMessageContent, ChatCompletionRequestUserMessageContentPart,
    ChatCompletionRequestMessageContentPartText, ChatCompletionRequestMessageContentPartImage,
    ImageUrl,
};
use twilight_http::Client as HttpClient;
use twilight_model::channel::Attachment;
use twilight_model::channel::message::Message;
use twilight_model::id::marker::UserMarker;
use twilight_model::id::Id;

use crate::state::{HistoryEntry, Role};

const TOOL_INSTRUCTIONS: &str = "\
TOOL USE — REQUIRED RULES:
\
1. Always call the appropriate tool before answering any question about a user's account, \
subscriptions, license keys, or product status. Never guess or fabricate information. \
If no tool can answer the question, say so honestly.
\
2. Only the knowledge base should supply factual information in your replies. \
Tool data is for verification and context only — use it to confirm what the user tells you, \
identify their product, or check eligibility, then answer using KB content.";

pub async fn fetch_reply_chain(
    msg: &Message,
    http: &HttpClient,
    max_depth: usize,
) -> Vec<Message> {
    let mut chain = Vec::new();
    let mut current = msg.clone();

    for _ in 0..max_depth {
        let parent = if let Some(ref boxed) = current.referenced_message {
            Some(*boxed.clone())
        } else if let Some(ref reference) = current.reference {
            if let Some(message_id) = reference.message_id {
                match http.message(current.channel_id, message_id).await {
                    Ok(resp) => resp.model().await.ok(),
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
                chain.push(p.clone());
                current = p;
            }
            None => break,
        }
    }

    chain.reverse();
    chain
}

/// Build a message array where a tool was already called (e.g. after a modal submission).
/// Structure: system + history + user_message + assistant(tool_call) + tool_result
/// so the AI knows the data is already fetched and just needs to respond.
pub fn build_messages_with_prefetched_tool(
    system_prompt: &str,
    history: &[HistoryEntry],
    user_message: &str,
    bot_user_id: Id<UserMarker>,
    kb_context: &[String],
    tool_name: &str,
    tool_args: &serde_json::Value,
    tool_result: &str,
) -> anyhow::Result<Vec<ChatCompletionRequestMessage>> {
    let mut messages = build_messages_array(
        system_prompt,
        history,
        &[],
        user_message,
        &[],
        bot_user_id,
        kb_context,
    )?;

    // Synthetic assistant message showing the tool was already called.
    let tool_call_msg: ChatCompletionRequestMessage = serde_json::from_value(serde_json::json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": "prefetched_0",
            "type": "function",
            "function": {
                "name": tool_name,
                "arguments": serde_json::to_string(tool_args).unwrap_or_default()
            }
        }]
    }))?;

    // Synthetic tool result message.
    let tool_result_msg: ChatCompletionRequestMessage = serde_json::from_value(serde_json::json!({
        "role": "tool",
        "tool_call_id": "prefetched_0",
        "content": tool_result
    }))?;

    messages.push(tool_call_msg);
    messages.push(tool_result_msg);

    Ok(messages)
}

pub fn build_messages_array(
    system_prompt: &str,
    history: &[HistoryEntry],
    reply_chain: &[Message],
    content: &str,
    attachments: &[Attachment],
    bot_user_id: Id<UserMarker>,
    kb_context: &[String],
) -> anyhow::Result<Vec<ChatCompletionRequestMessage>> {
    let mut messages = Vec::new();

    let full_system = if kb_context.is_empty() {
        format!("{}\n\n{}", system_prompt, TOOL_INSTRUCTIONS)
    } else {
        format!(
            "{}\n\n{}\n\n[Relevant knowledge context:]\n{}",
            system_prompt,
            TOOL_INSTRUCTIONS,
            kb_context.join("\n---\n")
        )
    };
    messages.push(
        ChatCompletionRequestSystemMessageArgs::default()
            .content(full_system)
            .build()?
            .into(),
    );

    for msg in reply_chain {
        if msg.author.id == bot_user_id {
            messages.push(
                ChatCompletionRequestAssistantMessageArgs::default()
                    .content(msg.content.clone())
                    .build()?
                    .into(),
            );
        } else {
            messages.push(
                ChatCompletionRequestUserMessageArgs::default()
                    .content(msg.content.clone())
                    .build()?
                    .into(),
            );
        }
    }

    for entry in history {
        match entry.role {
            Role::Assistant => {
                messages.push(
                    ChatCompletionRequestAssistantMessageArgs::default()
                        .content(entry.content.clone())
                        .build()?
                        .into(),
                );
            }
            Role::User => {
                messages.push(build_user_message(&entry.content, &entry.image_urls)?);
            }
        }
    }

    let image_urls: Vec<String> = attachments
        .iter()
        .filter(|a| {
            a.content_type
                .as_deref()
                .map(|ct| ct.starts_with("image/"))
                .unwrap_or(false)
        })
        .map(|a| a.url.clone())
        .collect();

    let non_image_note: String = attachments
        .iter()
        .filter(|a| {
            !a.content_type
                .as_deref()
                .map(|ct| ct.starts_with("image/"))
                .unwrap_or(false)
        })
        .map(|a| format!("User attached: {}", a.filename))
        .collect::<Vec<_>>()
        .join("\n");

    let full_content = if non_image_note.is_empty() {
        content.to_string()
    } else {
        format!("{}\n\n{}", content, non_image_note)
    };

    messages.push(build_user_message(&full_content, &image_urls)?);

    Ok(messages)
}

fn build_user_message(
    content: &str,
    image_urls: &[String],
) -> Result<ChatCompletionRequestMessage> {
    if image_urls.is_empty() {
        Ok(ChatCompletionRequestUserMessageArgs::default()
            .content(content)
            .build()?
            .into())
    } else {
        let mut parts: Vec<ChatCompletionRequestUserMessageContentPart> = vec![
            ChatCompletionRequestMessageContentPartText { text: content.to_string() }.into(),
        ];

        for url in image_urls {
            parts.push(
                ChatCompletionRequestMessageContentPartImage {
                    image_url: ImageUrl {
                        url: url.clone(),
                        detail: None,
                    },
                }
                    .into(),
            );
        }

        Ok(ChatCompletionRequestUserMessageArgs::default()
            .content(ChatCompletionRequestUserMessageContent::Array(parts))
            .build()?
            .into())
    }
}
