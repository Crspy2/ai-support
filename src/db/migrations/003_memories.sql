CREATE TABLE pending_memories (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    content       TEXT NOT NULL,
    summary       TEXT NOT NULL,
    message_link  TEXT NOT NULL,
    embedding     vector(1536) NOT NULL,
    status        TEXT NOT NULL DEFAULT 'pending',
    dm_channel_id TEXT NOT NULL,
    dm_message_id TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
