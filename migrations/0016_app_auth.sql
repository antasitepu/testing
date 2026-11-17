CREATE TABLE app_users (
    user_id UUID PRIMARY KEY,
    username TEXT NOT NULL UNIQUE CHECK (username ~ '^[a-z0-9._-]{3,32}$'),
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('ADMIN', 'USER')),
    device_limit SMALLINT NOT NULL DEFAULT 2 CHECK (device_limit BETWEEN 1 AND 20),
    disabled_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- One row per logged-in device. Only the SHA-256 of the cookie token is stored.
CREATE TABLE app_sessions (
    session_id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES app_users(user_id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    user_agent TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX app_sessions_user_idx ON app_sessions(user_id, created_at);

CREATE TABLE auth_events (
    event_id UUID PRIMARY KEY,
    event_type TEXT NOT NULL,
    username TEXT NOT NULL,
    actor TEXT,
    client_ip TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX auth_events_created_idx ON auth_events(created_at DESC);
