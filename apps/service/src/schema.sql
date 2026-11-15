CREATE TABLE IF NOT EXISTS stryk_scan (
    market text PRIMARY KEY,
    slot bigint NOT NULL,
    signature text
);

CREATE TABLE IF NOT EXISTS stryk_positions (
    market text NOT NULL,
    address text NOT NULL,
    writer text NOT NULL,
    maker text NOT NULL,
    opened_at bigint NOT NULL,
    slot bigint NOT NULL,
    record jsonb NOT NULL,
    PRIMARY KEY (market, address)
);

ALTER TABLE stryk_positions ADD COLUMN IF NOT EXISTS entry_spot jsonb;

CREATE TABLE IF NOT EXISTS stryk_quote_spots (
    market text NOT NULL,
    position text NOT NULL,
    valid_until bigint NOT NULL,
    spot jsonb NOT NULL,
    PRIMARY KEY (market, position)
);

CREATE INDEX IF NOT EXISTS stryk_quote_spots_expiry
    ON stryk_quote_spots (market, valid_until, position);

ALTER TABLE stryk_scan
    ADD COLUMN IF NOT EXISTS signature_slot bigint,
    ADD COLUMN IF NOT EXISTS scan_head text,
    ADD COLUMN IF NOT EXISTS scan_head_slot bigint,
    ADD COLUMN IF NOT EXISTS scan_before text,
    ADD COLUMN IF NOT EXISTS scan_ready boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS scan_archive boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS indexing_healthy boolean NOT NULL DEFAULT false;

CREATE TABLE IF NOT EXISTS stryk_signature_queue (
    id bigint GENERATED ALWAYS AS IDENTITY,
    market text NOT NULL,
    signature text NOT NULL,
    slot bigint NOT NULL,
    succeeded boolean NOT NULL,
    PRIMARY KEY (market, id),
    UNIQUE (market, signature)
);

CREATE INDEX IF NOT EXISTS stryk_positions_writer
    ON stryk_positions (market, writer, opened_at DESC, address DESC);
CREATE INDEX IF NOT EXISTS stryk_positions_maker
    ON stryk_positions (market, maker, opened_at DESC, address DESC);

CREATE INDEX IF NOT EXISTS stryk_positions_writer_active
    ON stryk_positions (market, writer, opened_at DESC, address DESC)
    WHERE record->>'outcome' = '0';

CREATE TABLE IF NOT EXISTS stryk_receipts (
    market text NOT NULL,
    signature text NOT NULL,
    instruction_index integer NOT NULL,
    position text NOT NULL,
    phase smallint NOT NULL,
    PRIMARY KEY (market, signature, instruction_index)
);

CREATE INDEX IF NOT EXISTS stryk_receipt_position ON stryk_receipts (market, position, phase);
