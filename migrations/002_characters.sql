CREATE TABLE characters (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  UUID        NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name        TEXT        NOT NULL UNIQUE,
    race        SMALLINT    NOT NULL,
    class       SMALLINT    NOT NULL,
    level       INT         NOT NULL DEFAULT 1,
    zone_id     INT         NOT NULL DEFAULT 1,
    position_x  REAL        NOT NULL DEFAULT 0.0,
    position_y  REAL        NOT NULL DEFAULT 0.0,
    position_z  REAL        NOT NULL DEFAULT 0.0,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_characters_account_id ON characters(account_id);
