use anyhow::Result;
use twilight_http::Client as HttpClient;
use twilight_model::channel::Message;
use twilight_model::channel::message::MessageFlags;
use twilight_model::channel::message::component::{Component, TextDisplay};
use twilight_model::id::marker::{ApplicationMarker, ChannelMarker, MessageMarker};
use twilight_model::id::Id;

pub async fn send_gateway_reply(
    http: &HttpClient,
    channel_id: Id<ChannelMarker>,
    reply_to: Id<MessageMarker>,
    content: &str,
) -> Result<Message> {
    let msg = http
        .create_message(channel_id)
        .reply(reply_to)
        .flags(MessageFlags::IS_COMPONENTS_V2)
        .components(&[Component::TextDisplay(TextDisplay {
            id: None,
            content: content.to_string(),
        })])
        .await?
        .model()
        .await?;

    Ok(msg)
}

pub async fn send_interaction_followup(
    http: &HttpClient,
    application_id: Id<ApplicationMarker>,
    interaction_token: &str,
    content: &str,
) -> Result<()> {
    http.interaction(application_id)
        .create_followup(interaction_token)
        .flags(MessageFlags::IS_COMPONENTS_V2)
        .components(&[Component::TextDisplay(TextDisplay {
            id: None,
            content: content.to_string(),
        })])
        .await?;

    Ok(())
}
