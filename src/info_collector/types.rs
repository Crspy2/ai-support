use serde_json::Value;
use twilight_model::channel::message::component::TextInputStyle;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, MessageMarker};
use uuid::Uuid;

use crate::state::{ConversationId, HistoryEntry};

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
    pub reply_to_msg_id: Option<Id<MessageMarker>>,
    pub title: String,
    pub message: String,
    pub fields: Vec<InfoField>,
    pub ext_name: String,
    pub method_name: String,
    pub known_args: Value,
    /// The original user message that triggered this request.
    pub user_message: String,
    /// System prompt (with author/owner/message_link context baked in).
    pub system_prompt: String,
    /// Conversation history at the time of the request.
    pub history: Vec<HistoryEntry>,
    /// KB search results at the time of the request.
    pub kb_context: Vec<String>,
    /// Conversation ID to link the reply to (None for a new conversation).
    pub conv_id: Option<ConversationId>,
}
