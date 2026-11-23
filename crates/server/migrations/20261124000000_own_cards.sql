-- Memory cards become the user's own: named, of one console, inserted in
-- whichever game the user plays (rather than each game having its two).

CREATE TABLE cards (
    id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    user_id BIGINT UNSIGNED NOT NULL,
    console ENUM('ps1', 'ps2') NOT NULL,
    name VARCHAR(64) NOT NULL,
    -- 128 KiB (PlayStation) or 8.25 MiB (PS2, with ECC).
    data MEDIUMBLOB NOT NULL,
    created_at TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    updated_at TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3),
    PRIMARY KEY (id),
    KEY cards_user (user_id, console),
    CONSTRAINT cards_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_bin;

-- The cards each game was last played with, chosen again next time.
ALTER TABLE game_settings
    ADD COLUMN card1 BIGINT UNSIGNED NULL,
    ADD COLUMN card2 BIGINT UNSIGNED NULL,
    ADD CONSTRAINT game_settings_card1 FOREIGN KEY (card1) REFERENCES cards (id) ON DELETE SET NULL,
    ADD CONSTRAINT game_settings_card2 FOREIGN KEY (card2) REFERENCES cards (id) ON DELETE SET NULL;

-- The games' cards become cards of their own, named after the game's key
-- (its serial) and slot, and stay the game's cards.
INSERT INTO cards (user_id, console, name, data, created_at, updated_at)
SELECT user_id, IF(LENGTH(data) = 131072, 'ps1', 'ps2'), CONCAT(save_key, ' card ', slot), data, updated_at, updated_at
FROM memory_cards
ORDER BY user_id, save_key, slot;

INSERT INTO game_settings (user_id, save_key, card1, card2)
SELECT m.user_id, m.save_key,
    (SELECT c.id FROM cards c WHERE c.user_id = m.user_id AND c.name = CONCAT(m.save_key, ' card 1') LIMIT 1),
    (SELECT c.id FROM cards c WHERE c.user_id = m.user_id AND c.name = CONCAT(m.save_key, ' card 2') LIMIT 1)
FROM (SELECT DISTINCT user_id, save_key FROM memory_cards) m
ON DUPLICATE KEY UPDATE card1 = VALUES(card1), card2 = VALUES(card2);

DROP TABLE memory_cards;
