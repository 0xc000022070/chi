CREATE TABLE IF NOT EXISTS projects (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL,
  prefix TEXT NOT NULL,
  name TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  lead INTEGER,
  status TEXT NOT NULL DEFAULT 'active',
  target_date TEXT NOT NULL DEFAULT '',
  repo_id INTEGER,
  next_number INTEGER NOT NULL DEFAULT 1,
  linear_id TEXT UNIQUE,
  created_at INTEGER NOT NULL,
  UNIQUE (org_id, prefix)
);
CREATE TABLE IF NOT EXISTS milestones (
  id INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  target_date TEXT NOT NULL DEFAULT '',
  linear_id TEXT UNIQUE,
  UNIQUE (project_id, name)
);
CREATE TABLE IF NOT EXISTS issues (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL,
  project_id INTEGER NOT NULL REFERENCES projects(id),
  number INTEGER NOT NULL,
  title TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'backlog',
  priority INTEGER NOT NULL DEFAULT 0,
  estimate INTEGER,
  assignee INTEGER,
  creator INTEGER,
  parent_id INTEGER REFERENCES issues(id) ON DELETE SET NULL,
  milestone_id INTEGER REFERENCES milestones(id) ON DELETE SET NULL,
  due_date TEXT NOT NULL DEFAULT '',
  error_ref INTEGER,
  sort REAL NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  completed_at INTEGER,
  linear_id TEXT UNIQUE,
  UNIQUE (project_id, number)
);
CREATE INDEX IF NOT EXISTS issues_by_org ON issues (org_id, updated_at);
CREATE INDEX IF NOT EXISTS issues_by_parent ON issues (parent_id);
CREATE UNIQUE INDEX IF NOT EXISTS issues_one_per_error ON issues (org_id, error_ref) WHERE error_ref IS NOT NULL;
CREATE TABLE IF NOT EXISTS labels (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL,
  name TEXT NOT NULL COLLATE NOCASE,
  color TEXT NOT NULL DEFAULT '#8a8f98',
  UNIQUE (org_id, name)
);
CREATE TABLE IF NOT EXISTS issue_labels (
  issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  label_id INTEGER NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
  PRIMARY KEY (issue_id, label_id)
);
CREATE TABLE IF NOT EXISTS relations (
  id INTEGER PRIMARY KEY,
  issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  other_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  UNIQUE (issue_id, kind, other_id)
);
CREATE TABLE IF NOT EXISTS comments (
  id INTEGER PRIMARY KEY,
  issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  author INTEGER,
  body TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  linear_id TEXT UNIQUE
);
CREATE INDEX IF NOT EXISTS comments_by_issue ON comments (issue_id, id);
CREATE TABLE IF NOT EXISTS history (
  id INTEGER PRIMARY KEY,
  issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  actor INTEGER,
  field TEXT NOT NULL,
  old TEXT NOT NULL,
  new TEXT NOT NULL,
  at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS history_by_issue ON history (issue_id, id);
CREATE TABLE IF NOT EXISTS documents (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL,
  project_id INTEGER REFERENCES projects(id) ON DELETE SET NULL,
  title TEXT NOT NULL,
  body TEXT NOT NULL DEFAULT '',
  author INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  linear_id TEXT UNIQUE
);
CREATE TABLE IF NOT EXISTS attachments (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL,
  issue_id INTEGER REFERENCES issues(id) ON DELETE CASCADE,
  title TEXT NOT NULL,
  url TEXT NOT NULL DEFAULT '',
  file TEXT NOT NULL DEFAULT '',
  mime TEXT NOT NULL DEFAULT '',
  size INTEGER NOT NULL DEFAULT 0,
  uploader INTEGER,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS attachments_by_issue ON attachments (issue_id);
CREATE TABLE IF NOT EXISTS templates (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL,
  name TEXT NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  description TEXT NOT NULL DEFAULT '',
  fields TEXT NOT NULL DEFAULT '{}',
  UNIQUE (org_id, name)
);
CREATE TABLE IF NOT EXISTS views (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL,
  owner INTEGER NOT NULL,
  name TEXT NOT NULL,
  query TEXT NOT NULL,
  layout TEXT NOT NULL DEFAULT 'list',
  shared INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS notifications (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL,
  user_id INTEGER NOT NULL,
  issue_id INTEGER REFERENCES issues(id) ON DELETE CASCADE,
  actor INTEGER,
  text TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  read_at INTEGER
);
CREATE INDEX IF NOT EXISTS notifications_by_user ON notifications (user_id, org_id, id);
CREATE TABLE IF NOT EXISTS subscriptions (
  issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  user_id INTEGER NOT NULL,
  PRIMARY KEY (issue_id, user_id)
);
CREATE TABLE IF NOT EXISTS links (
  id INTEGER PRIMARY KEY,
  issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  url TEXT NOT NULL,
  title TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT '',
  at INTEGER NOT NULL,
  UNIQUE (issue_id, url)
);
CREATE VIRTUAL TABLE IF NOT EXISTS issues_fts USING fts5(title, description, content='issues', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS issues_ai AFTER INSERT ON issues BEGIN
  INSERT INTO issues_fts (rowid, title, description) VALUES (new.id, new.title, new.description);
END;
CREATE TRIGGER IF NOT EXISTS issues_ad AFTER DELETE ON issues BEGIN
  INSERT INTO issues_fts (issues_fts, rowid, title, description) VALUES ('delete', old.id, old.title, old.description);
END;
CREATE TRIGGER IF NOT EXISTS issues_au AFTER UPDATE OF title, description ON issues BEGIN
  INSERT INTO issues_fts (issues_fts, rowid, title, description) VALUES ('delete', old.id, old.title, old.description);
  INSERT INTO issues_fts (rowid, title, description) VALUES (new.id, new.title, new.description);
END;
CREATE VIRTUAL TABLE IF NOT EXISTS comments_fts USING fts5(body, content='comments', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS comments_ai AFTER INSERT ON comments BEGIN
  INSERT INTO comments_fts (rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER IF NOT EXISTS comments_ad AFTER DELETE ON comments BEGIN
  INSERT INTO comments_fts (comments_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;
CREATE TRIGGER IF NOT EXISTS comments_au AFTER UPDATE OF body ON comments BEGIN
  INSERT INTO comments_fts (comments_fts, rowid, body) VALUES ('delete', old.id, old.body);
  INSERT INTO comments_fts (rowid, body) VALUES (new.id, new.body);
END;
CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(title, body, content='documents', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS documents_ai AFTER INSERT ON documents BEGIN
  INSERT INTO documents_fts (rowid, title, body) VALUES (new.id, new.title, new.body);
END;
CREATE TRIGGER IF NOT EXISTS documents_ad AFTER DELETE ON documents BEGIN
  INSERT INTO documents_fts (documents_fts, rowid, title, body) VALUES ('delete', old.id, old.title, old.body);
END;
CREATE TRIGGER IF NOT EXISTS documents_au AFTER UPDATE OF title, body ON documents BEGIN
  INSERT INTO documents_fts (documents_fts, rowid, title, body) VALUES ('delete', old.id, old.title, old.body);
  INSERT INTO documents_fts (rowid, title, body) VALUES (new.id, new.title, new.body);
END;
