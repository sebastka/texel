-- Texel's database: its users and their sessions, what they keep (memory
-- cards, save states, the BIOS of each game), and the verification hashes
-- of the library's files. Saves are keyed by user and by the disc's serial
-- ("SLES-00049"), or "file-<id>" for discs without one; memory cards and
-- settings use the first disc's.

-- Users sign in with OpenID Connect, or with an e-mail address and a
-- password (people invited by an administrator).
CREATE TABLE users (
    id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    name VARCHAR(64) NOT NULL,
    -- From the configuration for OpenID Connect users, at each sign-in;
    -- invited users are guests.
    role ENUM('admin', 'guest') NOT NULL DEFAULT 'guest',
    -- OpenID Connect users: the provider and their subject there.
    oidc_issuer VARCHAR(255) NULL,
    oidc_subject VARCHAR(255) NULL,
    -- Invited users: their e-mail address (lower case: what they sign in
    -- with) and Argon2id password hash.
    email VARCHAR(254) NULL,
    password_hash VARCHAR(255) NULL,
    created_at TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    last_seen_at TIMESTAMP(3) NULL,
    PRIMARY KEY (id),
    UNIQUE KEY users_oidc (oidc_issuer, oidc_subject),
    UNIQUE KEY users_email (email)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_bin;

-- Signed-in browsers: the SHA-256 of the cookie's random token.
CREATE TABLE sessions (
    token_hash BINARY(32) NOT NULL,
    user_id BIGINT UNSIGNED NOT NULL,
    -- For signing out at the provider too.
    id_token TEXT NULL,
    created_at TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    last_used_at TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    expires_at TIMESTAMP(3) NOT NULL,
    PRIMARY KEY (token_hash),
    KEY sessions_user (user_id),
    CONSTRAINT sessions_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_bin;

-- OpenID Connect sign-ins under way, used once by the callback.
CREATE TABLE oidc_logins (
    state VARCHAR(64) NOT NULL,
    verifier VARCHAR(128) NOT NULL,
    nonce VARCHAR(64) NOT NULL,
    -- The SHA-256 of the cookie binding the sign-in to the browser.
    browser_hash BINARY(32) NOT NULL,
    return_to VARCHAR(512) NOT NULL,
    created_at TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    PRIMARY KEY (state)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_bin;

-- Tokens an administrator hands over: to register (an invitation), or to
-- set a new password (a reset, for an existing invited user).
CREATE TABLE invites (
    id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    token_hash BINARY(32) NOT NULL,
    kind ENUM('invite', 'reset') NOT NULL,
    -- A reset's user.
    user_id BIGINT UNSIGNED NULL,
    -- Who it is for, as the administrator noted it.
    note VARCHAR(100) NOT NULL DEFAULT '',
    created_by BIGINT UNSIGNED NULL,
    created_at TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    expires_at TIMESTAMP(3) NOT NULL,
    used_at TIMESTAMP(3) NULL,
    used_by BIGINT UNSIGNED NULL,
    PRIMARY KEY (id),
    UNIQUE KEY invites_token (token_hash),
    CONSTRAINT invites_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE,
    CONSTRAINT invites_created_by FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL,
    CONSTRAINT invites_used_by FOREIGN KEY (used_by) REFERENCES users (id) ON DELETE SET NULL
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_bin;

-- Saves.
CREATE TABLE memory_cards (
    user_id BIGINT UNSIGNED NOT NULL,
    save_key VARCHAR(64) NOT NULL,
    slot TINYINT UNSIGNED NOT NULL,
    -- 128 KiB (PlayStation) or 8.25 MiB (PS2, with ECC).
    data MEDIUMBLOB NOT NULL,
    updated_at TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3),
    PRIMARY KEY (user_id, save_key, slot),
    CONSTRAINT memory_cards_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_bin;

CREATE TABLE save_states (
    user_id BIGINT UNSIGNED NOT NULL,
    save_key VARCHAR(64) NOT NULL,
    slot TINYINT UNSIGNED NOT NULL,
    -- Gzip-compressed by the browser: a few MiB.
    data LONGBLOB NOT NULL,
    -- PNG, sent after the state; cleared when the state is replaced.
    screenshot MEDIUMBLOB NULL,
    -- The BIOS it was saved with: states load only with it.
    bios VARCHAR(64) NULL,
    saved_at TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    PRIMARY KEY (user_id, save_key, slot),
    CONSTRAINT save_states_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_bin;

CREATE TABLE game_settings (
    user_id BIGINT UNSIGNED NOT NULL,
    save_key VARCHAR(64) NOT NULL,
    -- The BIOS the game was last started with.
    bios VARCHAR(64) NULL,
    updated_at TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3),
    PRIMARY KEY (user_id, save_key),
    CONSTRAINT game_settings_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_bin;

CREATE TABLE file_hashes (
    -- Relative to the library; tracks of CHD/PBP images as "disc.chd#track01.bin".
    path VARCHAR(700) NOT NULL,
    size BIGINT UNSIGNED NOT NULL,
    -- Unix seconds: a hash is reused while size and time are unchanged.
    mtime BIGINT UNSIGNED NOT NULL,
    sha1 CHAR(40) NOT NULL,
    PRIMARY KEY (path)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_bin;
