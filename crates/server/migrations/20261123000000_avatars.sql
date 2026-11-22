-- OpenID Connect users' pictures (the `picture` claim), fetched at sign-in
-- and served by Texel itself: the page loads no image from elsewhere.
ALTER TABLE users
    -- The URL it was fetched from: fetched again only when it changes.
    ADD COLUMN avatar_url VARCHAR(1024) NULL,
    -- PNG, JPEG, WebP or GIF, at most 2 MiB.
    ADD COLUMN avatar MEDIUMBLOB NULL,
    ADD COLUMN avatar_type VARCHAR(32) NULL,
    -- A hash of the image, in its URL: browsers keep it until it changes.
    ADD COLUMN avatar_version VARCHAR(16) NULL;
