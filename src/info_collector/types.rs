use serde_json::Value;
use twilight_model::channel::message::component::TextInputStyle;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, MessageMarker};
use uuid::Uuid;

pub struct InfoField {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub placeholder: Option<String>,
    pub required: bool,
    pub style: TextInputStyle,
    pub cache: bool,
    pub cache_ttl_hours: Option<u64>,
}

pub struct PendingInfoRequest {
    pub id: Uuid,
    pub target_user_id: String,
    pub channel_id: Id<ChannelMarker>,
    pub reply_to_msg_id: Id<MessageMarker>,
    pub title: String,
    pub message: String,
    pub fields: Vec<InfoField>,
    pub ext_name: String,
    pub method_name: String,
    pub known_args: Value,
}
