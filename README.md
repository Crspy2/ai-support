# ai-support

A production-grade AI support bot for Discord. Answers user questions using a semantic knowledge base, calls extension tools to look up live data, and collects required information from users via Discord modals — all without leaking private data into public channels.

## Features

- **Semantic KB search** — pgvector-backed knowledge base populated from embeddable extension fetchers at startup; per-topic chunking for high-recall retrieval
- **Extension system** — declare fetchers and actions with simple proc-macro attributes; the AI picks the right tool automatically
- **Info collection modals** — when an action needs data (e.g. a panel username), the bot asks for it privately via a Discord modal and caches the response for the conversation
- **Conversation tool cache** — tool results are cached per conversation so re-fetches in reply chains are avoided
- **Issue detection** — tracks repeated similar questions, proposes KB articles when a pattern is detected
- **Privacy by design** — tool results containing account data are treated as private background context; only the KB supplies factual content in public replies

## Architecture

```
Discord Gateway / Interactions
         │
         ▼
    AppState (Arc)
    ├── KnowledgeBase  ─── pgvector (semantic search)
    ├── ExtensionRegistry ─ fetchers + actions
    ├── InfoCollector  ─── modal request queue (DashMap)
    ├── ConvToolCache  ─── per-conversation result cache (DashMap)
    ├── IssueTracker   ─── signal clustering + proposal
    └── MemoryTracker  ─── per-user memory management
         │
         ▼
    call_openai (10-turn tool-use loop)
    ├── checks ConvToolCache before executing
    ├── executes via ExtensionRegistry
    └── returns AiResponse { content, tool_results }
```

## Extension System

Extensions are registered at compile time via `inventory::submit!`. The `#[extension]` proc macro generates the trait implementation automatically.

### Cache strategies

| Attribute value | Behaviour |
|---|---|
| `cache = "startup"` | Fetched once at startup; result embedded into KB |
| `cache = "24h"` / `"1h"` / `"5m"` | Fetched at most once per TTL; result available as a tool |
| `cache = "per_request"` | Fetched on every tool call (no caching) |

`embeddable = true` causes the result to be stored in the vector KB. Output split on `\n---\n` — each section becomes a separate KB chunk for better recall.

### Adding an extension

1. Create `src/extensions/my_extension.rs`:

```rust
use extensions_macros::{extension, ArgsSchema};
use anyhow::Result;

#[derive(serde::Deserialize, ArgsSchema)]
struct LookupArgs {
    #[description("The user's panel username")]
    username: String,
}

pub struct MyExtension;

#[extension]
impl MyExtension {
    #[fetch(
        cache = "per_request",
        description = "Look up a user by their panel username"
    )]
    async fn lookup_user(&self, args: LookupArgs) -> Result<String> {
        Ok(format!("User: {}", args.username))
    }

    #[action(description = "Reset a user's HWID")]
    async fn reset_hwid(&self, args: ResetArgs) -> Result<String> {
        // ...
        Ok("HWID reset successfully.".to_string())
    }
}

inventory::submit!(crate::extensions::ExtensionRegistration {
    name: "MyExtension",
    factory: || std::sync::Arc::new(MyExtension),
});
```

2. Add `pub mod my_extension;` to `src/extensions/mod.rs`.

That's it — the extension is registered and the AI can call its tools.

## Info Collection Modals

When an action requires data that the AI doesn't have (e.g. a panel username), use `request_info`:

```
request_info(
  title = "Account Lookup",
  message = "I need your panel username to look up your account.",
  fields = [{ id = "username", label = "Panel Username" }],
  then = "MyExtension::lookup_user"
)
```

The bot posts a "Provide Information" button. When the user clicks it, a modal appears. On submission:
- Cacheable fields are upserted to `user_info` (with optional TTL)
- The original action is called with the submitted values
- The result is posted as a reply (with user ping) then deleted after 10 seconds

## Configuration

Copy `.env.example` to `.env` and fill in the values:

```env
# Discord
DISCORD_TOKEN=""
DISCORD_PUBLIC_KEY=""
APPLICATION_ID=""
OWNER_ID=""

# OpenAI
OPENAI_API_KEY=""
OPENAI_MODEL="gpt-4o"
OPENAI_EMBEDDING_MODEL="text-embedding-3-small"

# AI system prompt (instructions for the bot's personality and scope)
AI_SYSTEM_PROMPT=""

# Database (PostgreSQL with pgvector)
DATABASE_URL="postgres://postgres:password@localhost/ai_support"
```

## Running

**Prerequisites:** Docker, Rust toolchain (stable).

```bash
# Start PostgreSQL with pgvector
docker compose up -d

# Build and run
cargo run

# Or for production
cargo build --release
./target/release/ai-support
```

The bot connects to Discord via gateway and registers an Axum HTTP server for interaction webhooks.

## Database

Migrations run automatically at startup via `sqlx::migrate!`.

| Table | Purpose |
|---|---|
| `knowledge_chunks` | Embedded KB content with pgvector index |
| `issue_signals` | Raw message signals for issue detection |
| `issues` | Proposed/confirmed recurring issues |
| `user_info` | Cached user-provided values (modal submissions) |

## Project Structure

```
src/
├── main.rs                    # Startup: DB → extensions → KB → AppState
├── state.rs                   # AppState, ConvToolCache type aliases
├── config.rs                  # Config loaded from env
├── agent/
│   ├── client.rs              # call_openai — 10-turn tool-use loop
│   └── context.rs             # build_messages_array, TOOL_INSTRUCTIONS
├── discord/
│   ├── gateway.rs             # Gateway message handler, typing indicator
│   ├── interactions.rs        # HTTP interaction handler (slash commands, modals)
│   └── respond.rs             # send_interaction_followup helper
├── extensions/
│   ├── mod.rs                 # ExtensionRegistry, re-exports
│   ├── traits.rs              # ExtensionTrait, CacheStrategy, descriptors
│   └── example_extension.rs  # Demo extension
├── knowledge/
│   ├── mod.rs                 # KnowledgeBase: populate_at_startup, search
│   └── embed.rs               # embed_text() via text-embedding-3-small
├── info_collector/
│   ├── mod.rs                 # pub use re-exports
│   ├── collector.rs           # InfoCollector: initiate, modal handling
│   ├── types.rs               # InfoField, PendingInfoRequest
│   └── ui.rs                  # Discord component builders
├── issues/
│   └── mod.rs                 # IssueTracker: signal recording, clustering
├── memory/
│   └── mod.rs                 # MemoryTracker: per-user persistent memory
└── db/
    ├── mod.rs                 # connect(), run_migrations()
    └── migrations/
        ├── 001_initial.sql    # knowledge_chunks
        ├── 002_issues.sql     # issue_signals, issues
        └── 003_user_info.sql  # user_info

extensions-macros/
└── src/lib.rs                 # #[extension], #[fetch], #[action], ArgsSchema derive
```
