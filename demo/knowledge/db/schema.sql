CREATE TABLE payments (
  id       UUID PRIMARY KEY,
  amount   INTEGER NOT NULL,
  currency TEXT NOT NULL DEFAULT 'usd'
);

CREATE TABLE ledger_entries (
  id         UUID PRIMARY KEY,
  payment_id UUID REFERENCES payments(id),
  delta      INTEGER NOT NULL
);

CREATE TABLE audit_log (
  id    UUID PRIMARY KEY,
  actor TEXT NOT NULL
);
