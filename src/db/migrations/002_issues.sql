CREATE TABLE IF NOT EXISTS issue_signals (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    TEXT NOT NULL,
    content    TEXT NOT NULL,
    embedding  vector(1536) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS issue_signals_created_at_idx
    ON issue_signals (created_at);

CREATE TABLE IF NOT EXISTS issues (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    summary        TEXT NOT NULL,
    embedding      vector(1536) NOT NULL,
    status         TEXT NOT NULL DEFAULT 'proposed',
    user_count     INTEGER NOT NULL,
    dm_channel_id  TEXT NOT NULL,
    dm_message_id  TEXT NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS issues_embedding_idx
    ON issues USING ivfflat (embedding vector_cosine_ops)
    WITH (lists = 10);
