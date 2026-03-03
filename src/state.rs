use std::sync::Arc;

use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use dashmap::DashMap;
use twilight_http::Client as HttpClient;
use twilight_model::id::{
    marker::{ApplicationMarker, MessageMarker, UserMarker},
    Id,
};

use crate::config::Config;
use crate::extensions::ExtensionRegistry;
use crate::knowledge::KnowledgeBase;

pub type ConversationId = Id<MessageMarker>;
pub type ConversationStore = DashMap<Id<MessageMarker>, ConversationId>;
pub type HistoryStore = DashMap<ConversationId, Vec<HistoryEntry>>;

#[derive(Clone, Debug)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub role: Role,
    pub content: String,
    pub image_urls: Vec<String>,
}

pub struct AppState {
    pub http: Arc<HttpClient>,
    pub application_id: Id<ApplicationMarker>,
    pub bot_user_id: Id<UserMarker>,
    pub conversations: Arc<ConversationStore>,
    pub history: Arc<HistoryStore>,
    pub openai: Arc<OpenAIClient<OpenAIConfig>>,
    pub config: Arc<Config>,
    pub extensions: Arc<ExtensionRegistry>,
    pub knowledge_base: Arc<KnowledgeBase>,
}
