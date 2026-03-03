use anyhow::Result;
use twilight_http::Client as HttpClient;
use twilight_model::channel::Message;
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
        .content(content)
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
        .content(content)
        .await?;

    Ok(())
}
