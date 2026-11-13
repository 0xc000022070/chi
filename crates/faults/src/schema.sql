CREATE TABLE IF NOT EXISTS projects (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL,
  name TEXT NOT NULL,
  repo_id INTEGER,
  path_prefix TEXT NOT NULL DEFAULT '',
  UNIQUE (org_id, name)
);
CREATE TABLE IF NOT EXISTS keys (
  public_key TEXT PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  label TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS issues (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  fingerprint TEXT NOT NULL,
  title TEXT NOT NULL,
  culprit TEXT NOT NULL,
  level TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'open',
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  count INTEGER NOT NULL DEFAULT 1,
  UNIQUE (project_id, fingerprint)
);
CREATE INDEX IF NOT EXISTS issues_listing ON issues (project_id, status, last_seen);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY,
  issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  event_id TEXT,
  received_at INTEGER NOT NULL,
  release TEXT,
  environment TEXT,
  payload BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS events_by_issue ON events (issue_id, id);
CREATE INDEX IF NOT EXISTS events_by_age ON events (received_at);
