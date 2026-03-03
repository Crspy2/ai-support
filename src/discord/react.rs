use anyhow::Result;
use twilight_http::Client as HttpClient;
use twilight_http::request::channel::reaction::RequestReactionType;
use twilight_model::id::marker::{ChannelMarker, MessageMarker};
use twilight_model::id::Id;

pub async fn add_reaction(
    http: &HttpClient,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    emoji: &str,
) -> Result<()> {
    http.create_reaction(channel_id, message_id, &RequestReactionType::Unicode { name: emoji })
        .await?;
    Ok(())
}