use std::collections::HashMap;
use std::sync::Arc;

use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use dashmap::{DashMap, DashSet};
use twilight_http::Client as HttpClient;
use twilight_model::id::{
    marker::{ApplicationMarker, ChannelMarker, UserMarker},
    Id,
};

use crate::config::Config;
use crate::extensions::ExtensionRegistry;
use crate::info_collector::InfoCollector;
use crate::issues::IssueTracker;
use crate::knowledge::KnowledgeBase;
use crate::memory::MemoryTracker;

pub type ConversationId = Id<ChannelMarker>;
pub type HistoryStore = DashMap<ConversationId, Vec<HistoryEntry>>;
/// Maps conv_id → { "ToolName:args_json" → result } so tools aren't re-called
/// within the same conversation thread.
pub type ConvToolCache = DashMap<ConversationId, HashMap<String, String>>;

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
    pub threads: Arc<DashSet<Id<ChannelMarker>>>,
    pub history: Arc<HistoryStore>,
    pub conv_tool_cache: Arc<ConvToolCache>,
    pub openai: Arc<OpenAIClient<OpenAIConfig>>,
    pub config: Arc<Config>,
    pub extensions: Arc<ExtensionRegistry>,
    pub knowledge_base: Arc<KnowledgeBase>,
    pub issue_tracker: Arc<IssueTracker>,
    pub memory_tracker: Arc<MemoryTracker>,
    pub info_collector: Arc<InfoCollector>,
}
