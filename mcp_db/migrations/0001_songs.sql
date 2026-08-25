-- Songs seen in a Spotify playlist, and where they can be bought elsewhere.

-- One row per song. The identity of a song here is its *normalized* primary
-- artist and title: the point of this table is matching a Spotify track against
-- other stores (Bandcamp, Beatport), and those have no identifier in common —
-- only the text. Normalizing in the database rather than in the caller keeps the
-- keys and the unique constraint impossible to disagree with each other.
CREATE TABLE songs (
    id       bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,

    -- As the source gave them: the lead artist, and the title.
    artist   text NOT NULL,
    title    text NOT NULL,

    -- Lower-cased, trimmed, internal whitespace collapsed. Every function used
    -- here is IMMUTABLE, which is what lets them be stored and indexed.
    artist_key text GENERATED ALWAYS AS (
        lower(regexp_replace(btrim(artist), '\s+', ' ', 'g'))
    ) STORED,
    title_key text GENERATED ALWAYS AS (
        lower(regexp_replace(btrim(title), '\s+', ' ', 'g'))
    ) STORED,

    -- Every credited artist in the source's order, `artist` included. Kept
    -- whole because the key deliberately drops the featured credits.
    artists  text[] NOT NULL DEFAULT '{}',

    album              text,
    -- Spotify's release date is "2019", "2019-03" or "2019-03-12" depending on
    -- the release's precision, so it cannot be a `date`.
    album_release_date text,
    duration_ms        bigint,

    -- Identifiers that may let a later, better matcher skip the text entirely.
    -- Neither is guaranteed: local files have no Spotify ID, and not every
    -- track carries an ISRC.
    spotify_id text,
    isrc       text,

    first_seen_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT songs_artist_title_key UNIQUE (artist_key, title_key)
);

-- One row per search for a song on one store — appended, never updated, so the
-- history (including how the price moved) is kept. The current state of a song
-- on a platform is the most recent row:
--
--   SELECT DISTINCT ON (song_id, platform) *
--   FROM purchase_options
--   ORDER BY song_id, platform, searched_at DESC;
CREATE TABLE purchase_options (
    id      bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    song_id bigint NOT NULL REFERENCES songs (id) ON DELETE CASCADE,

    -- Free text ('bandcamp', 'beatport', ...) rather than an enum, so adding a
    -- store does not need a migration.
    platform text NOT NULL,

    -- When the search that produced this row was made.
    searched_at timestamptz NOT NULL DEFAULT now(),
    -- Where the song was found on that platform, if it was found at all.
    url         text,
    purchased   boolean NOT NULL DEFAULT false,
    -- Bandcamp prices in several currencies, so the amount alone is ambiguous.
    price       numeric(12, 2),
    currency    char(3)
);

-- Serves the DISTINCT ON above, and lookups of one song's options.
CREATE INDEX purchase_options_song_platform_idx
    ON purchase_options (song_id, platform, searched_at DESC);
