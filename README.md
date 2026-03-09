# ai-support

A production-grade AI support bot for Discord. When a user @mentions the bot, it creates a private thread for the conversation. Answers questions using a semantic knowledge base, calls extension tools to look up live data, and collects required information via Discord modals — all without leaking private data.

## Features

- **Private thread conversations** — @mentions create a private thread; all follow-up messages in the thread continue the conversation with full history
- **Semantic KB search** — pgvector-backed knowledge base populated from embeddable extension fetchers at startup; per-topic chunking for high-recall retrieval
- **Extension system** — declare fetchers and actions with simple proc-macro attributes; the AI picks the right tool automatically
- **Account linking** — Discord accounts are linked to loader accounts via license key verification; ownership is enforced on all account tools automatically
- **Info collection modals** — when an action needs data (e.g. an external username), the bot asks for it privately via a Discord modal
- **Conversation tool cache** — tool results are cached per thread so re-fetches are avoided
- **Issue detection** — tracks repeated similar questions, proposes KB articles when a pattern is detected
- **Per-user memory** — persistent memory per user across conversations
- **Privacy by design** — tool results containing account data are treated as private background context; only the KB supplies factual content in replies

## How It Works

1. User @mentions the bot in a channel
2. Bot creates a private thread, adds the user, and reacts with 🧵
3. Bot searches the knowledge base, calls any needed tools, and responds in the thread
4. All subsequent messages in the thread are treated as continuations of the same conversation
5. Conversation history, tool cache, and context persist for the thread's lifetime

## Architecture

```
Discord Gateway
       │
       ├── @mention → create PrivateThread → handle_new_conversation
       └── thread message → handle_thread_message (with history)
       │
       ▼
  AppState (Arc)
  ├── KnowledgeBase    ─── pgvector (semantic search)
  ├── ExtensionRegistry ── fetchers + actions
  ├── InfoCollector    ─── modal request queue (DashMap)
  ├── ConvToolCache    ─── per-thread result cache (DashMap)
  ├── IssueTracker     ─── signal clustering + proposal
  └── MemoryTracker    ─── per-user memory management
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
use extensions_macros::{extension, ExtensionSchema};
use anyhow::Result;

#[derive(serde::Deserialize, ExtensionSchema)]
struct LookupArgs {
    #[description("The user's loader username")]
    username: String,
}

pub struct MyExtension;

#[extension]
impl MyExtension {
    #[fetch(
        cache = "per_request",
        description = "Look up a user by their loader username"
    )]
    async fn lookup_user(&self, args: LookupArgs) -> Result<String> {
        Ok(format!("User: {}", args.username))
    }
}

inventory::submit!(crate::extensions::ExtensionFactory {
    build: |_ctx| std::sync::Arc::new(MyExtension),
});
```

2. Add `pub mod my_extension;` to `src/extensions/mod.rs`.

That's it — the extension is registered and the AI can call its tools.

## Info Collection Modals

When an action requires data that the AI doesn't have (e.g. a loader username and license key), it uses `request_info`:

```
request_info(
  title = "Example Action",
  message = "I need to get a user's info from the db.",
  fields = [
    { id = "username", label = "Username" },
  ],
  then = "PanelExtension::get_account_info"
)
```

The bot posts a "Provide Information" button in the thread. When the user clicks it, a modal appears. On submission the original action is called with the submitted values.

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
AI_MODEL="gpt-4o"

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
| `memories` | Per-user persistent memory |
| `user_info` | Cached user-provided values (modal submissions) |
## Project Structure

```
src/
├── main.rs                    # Startup: DB → extensions → KB → AppState
├── state.rs                   # AppState, ConvToolCache, HistoryStore
├── config.rs                  # Config loaded from env
├── agent/
│   ├── client.rs              # call_openai — 10-turn tool-use loop
│   └── context.rs             # build_messages_array, TOOL_INSTRUCTIONS
├── discord/
│   ├── gateway.rs             # Gateway: @mention → thread, thread continuations
│   ├── interactions.rs        # HTTP interaction handler (slash commands, modals, buttons)
│   ├── commands.rs            # Slash command registration
│   ├── respond.rs             # Message sending helpers
│   └── react.rs               # Reaction helpers
├── extensions/
│   ├── mod.rs                 # ExtensionRegistry, re-exports
│   ├── traits.rs              # ExtensionTrait, CacheStrategy, descriptors
├── knowledge/
│   ├── mod.rs                 # KnowledgeBase: populate_at_startup, search
│   └── embed.rs               # embed_text() via text-embedding-3-small
├── info_collector/
│   ├── mod.rs                 # pub use re-exports
│   ├── collector.rs           # InfoCollector: initiate, modal handling
│   ├── types.rs               # InfoField, PendingInfoRequest
│   └── ui.rs                  # Discord component builders
├── issues/
│   ├── mod.rs                 # IssueTracker: signal recording, clustering
│   └── hooks.rs               # Issue lifecycle hooks
├── memory/
│   ├── mod.rs                 # MemoryTracker: per-user persistent memory
│   └── hooks.rs               # Memory lifecycle hooks
└── db/
    ├── mod.rs                 # connect(), run_migrations()
    └── migrations/
        ├── 001_initial.sql    # knowledge_chunks
        ├── 002_issues.sql     # issue_signals, issues
        ├── 003_memories.sql   # memories
        ├── 004_user_info.sql  # user_info

extensions-macros/
└── src/lib.rs                 # #[extension], #[fetch], #[action], ExtensionSchema derive
```
