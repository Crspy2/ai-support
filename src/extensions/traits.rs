use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use dashmap::DashMap;
use serde_json::Value;

#[derive(Clone)]
#[allow(dead_code)]
pub enum CacheStrategy {
    /// Loaded once at startup, never re-fetched.
    Startup,
    /// Re-fetched after the given duration.
    Ttl(Duration),
    /// Always executed, never cached.
    PerRequest,
}

pub type HandlerFn =
    Box<dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> + Send + Sync>;

pub type HookHandlerFn =
    Box<dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;

/// All hookable events in the system.  Adding a new event here requires updating
/// `extensions-macros/src/lib.rs` (`VALID_HOOK_EVENTS` + `event_to_variant`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    IssueProposed,
    IssueAccepted,
    IssueRejected,
    IssueEnded,
    MemoryRequested,
    MemoryApproved,
    MemoryRejected,
}

pub struct HookDescriptor {
    pub event: HookEvent,
    pub handler: HookHandlerFn,
}

pub struct FetchDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    /// If true, fetched at startup for KB population; not exposed as an AI tool.
    pub embeddable: bool,
    pub cache: CacheStrategy,
    /// JSON Schema `object` for tool parameters.
    pub schema: Value,
    pub handler: HandlerFn,
}

pub struct ActionDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
    pub handler: HandlerFn,
}

pub trait ExtensionSchema {
    fn schema() -> serde_json::Value;
}

pub trait ExtensionTrait: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn fetchers(self: Arc<Self>) -> Vec<FetchDescriptor>;
    fn actions(self: Arc<Self>) -> Vec<ActionDescriptor>;
    fn hooks(self: Arc<Self>) -> Vec<HookDescriptor> {
        vec![]
    }
}

/// Cache entry: (populated_at, value)
type CacheEntry = (Instant, String);

pub struct ExtensionRegistry {
    extensions: Vec<Arc<dyn ExtensionTrait>>,
    cache: DashMap<String, CacheEntry>,
}

impl ExtensionRegistry {
    pub fn new(extensions: Vec<Arc<dyn ExtensionTrait>>) -> Self {
        Self {
            extensions,
            cache: DashMap::new(),
        }
    }

    pub fn extensions(&self) -> &[Arc<dyn ExtensionTrait>] {
        &self.extensions
    }

    /// Execute a fetcher by extension name + method name, applying cache logic.
    pub async fn call_fetcher(
        &self,
        ext_name: &str,
        method_name: &str,
        args: Value,
    ) -> Result<String> {
        let args_hash = sha256_hex(&args.to_string());
        let cache_key = format!("{ext_name}::{method_name}::{args_hash}");

        let ext = self
            .extensions
            .iter()
            .find(|e| e.name() == ext_name)
            .ok_or_else(|| anyhow::anyhow!("extension '{ext_name}' not found"))?;

        let fetchers = Arc::clone(ext).fetchers();
        let descriptor = fetchers
            .into_iter()
            .find(|f| f.name == method_name)
            .ok_or_else(|| anyhow::anyhow!("fetcher '{method_name}' not found"))?;

        match &descriptor.cache {
            CacheStrategy::Startup => {
                if let Some(entry) = self.cache.get(&cache_key) {
                    return Ok(entry.1.clone());
                }
                let result = (descriptor.handler)(args).await?;
                self.cache.insert(cache_key, (Instant::now(), result.clone()));
                Ok(result)
            }
            CacheStrategy::Ttl(ttl) => {
                if let Some(entry) = self.cache.get(&cache_key) {
                    if entry.0.elapsed() < *ttl {
                        return Ok(entry.1.clone());
                    }
                }
                let result = (descriptor.handler)(args).await?;
                self.cache.insert(cache_key, (Instant::now(), result.clone()));
                Ok(result)
            }
            CacheStrategy::PerRequest => (descriptor.handler)(args).await,
        }
    }

    /// Execute a fetcher or action by name — tries fetchers first, then actions.
    pub async fn call(
        &self,
        ext_name: &str,
        method_name: &str,
        args: Value,
    ) -> Result<String> {
        match self.call_fetcher(ext_name, method_name, args.clone()).await {
            Ok(r) => Ok(r),
            Err(_) => self.call_action(ext_name, method_name, args).await,
        }
    }

    /// Call an action by extension name + method name.
    pub async fn call_action(
        &self,
        ext_name: &str,
        method_name: &str,
        args: Value,
    ) -> Result<String> {
        let ext = self
            .extensions
            .iter()
            .find(|e| e.name() == ext_name)
            .ok_or_else(|| anyhow::anyhow!("extension '{ext_name}' not found"))?;

        let actions = Arc::clone(ext).actions();
        let descriptor = actions
            .into_iter()
            .find(|a| a.name == method_name)
            .ok_or_else(|| anyhow::anyhow!("action '{method_name}' not found"))?;

        (descriptor.handler)(args).await
    }

    /// Returns all non-embeddable fetchers as (ext_name, name, description, schema) tuples.
    pub fn non_embeddable_fetchers(&self) -> Vec<(String, &'static str, &'static str, Value)> {
        let mut result = Vec::new();
        for ext in &self.extensions {
            let ext_name = ext.name().to_string();
            let fetchers = Arc::clone(ext).fetchers();
            for f in fetchers {
                if !f.embeddable {
                    result.push((ext_name.clone(), f.name, f.description, f.schema.clone()));
                }
            }
        }
        result
    }

    /// Fire an event hook on all extensions that handle it.  Failures are logged, not propagated.
    pub async fn fire_hook(&self, event: HookEvent, payload: Value) {
        for ext in &self.extensions {
            for hook in Arc::clone(ext).hooks() {
                if hook.event == event {
                    if let Err(e) = (hook.handler)(payload.clone()).await {
                        tracing::warn!("hook '{event:?}' on '{}' failed: {e:#}", ext.name());
                    }
                }
            }
        }
    }

    /// Returns all actions as (ext_name, name, description, schema) tuples.
    pub fn all_actions(&self) -> Vec<(String, &'static str, &'static str, Value)> {
        let mut result = Vec::new();
        for ext in &self.extensions {
            let ext_name = ext.name().to_string();
            let actions = Arc::clone(ext).actions();
            for a in actions {
                result.push((ext_name.clone(), a.name, a.description, a.schema.clone()));
            }
        }
        result
    }
}

/// Shared resources available to extensions at construction time.
#[allow(dead_code)]
pub struct ExtensionContext {
    pub db: Arc<sqlx::PgPool>,
    pub openai: Arc<async_openai::Client<async_openai::config::OpenAIConfig>>,
    pub config: Arc<crate::config::Config>,
}

/// Registered by each extension via `inventory::submit!`.
pub struct ExtensionFactory {
    pub build: fn(&ExtensionContext) -> Arc<dyn ExtensionTrait>,
}

inventory::collect!(ExtensionFactory);

impl ExtensionRegistry {
    pub fn from_inventory(ctx: &ExtensionContext) -> Self {
        let extensions = inventory::iter::<ExtensionFactory>()
            .map(|f| (f.build)(ctx))
            .collect();
        Self::new(extensions)
    }
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}
