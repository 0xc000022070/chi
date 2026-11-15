CREATE TABLE IF NOT EXISTS jobs (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL,
  agent TEXT NOT NULL,
  model TEXT NOT NULL DEFAULT '',
  system TEXT NOT NULL DEFAULT '',
  prompt TEXT NOT NULL,
  source TEXT NOT NULL DEFAULT '',
  requester TEXT NOT NULL DEFAULT '',
  timeout INTEGER NOT NULL DEFAULT 300,
  status TEXT NOT NULL DEFAULT 'queued',
  output TEXT NOT NULL DEFAULT '',
  error TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  started_at INTEGER,
  finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS jobs_by_org ON jobs (org_id, id);
CREATE INDEX IF NOT EXISTS jobs_queued ON jobs (status, id);
