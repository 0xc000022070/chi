CREATE TABLE IF NOT EXISTS orgs (
  id INTEGER PRIMARY KEY,
  slug TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS users (
  id INTEGER PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  password_hash TEXT,
  admin INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  kind TEXT NOT NULL DEFAULT 'human'
);
CREATE TABLE IF NOT EXISTS memberships (
  user_id INTEGER NOT NULL REFERENCES users(id),
  org_id INTEGER NOT NULL REFERENCES orgs(id),
  role TEXT NOT NULL,
  PRIMARY KEY (user_id, org_id)
);
CREATE TABLE IF NOT EXISTS providers (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL REFERENCES orgs(id),
  name TEXT NOT NULL,
  issuer TEXT NOT NULL,
  client_id TEXT NOT NULL,
  trusted INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS identities (
  provider_id INTEGER NOT NULL REFERENCES providers(id),
  subject TEXT NOT NULL,
  user_id INTEGER NOT NULL REFERENCES users(id),
  PRIMARY KEY (provider_id, subject)
);
CREATE TABLE IF NOT EXISTS sessions (
  token_hash TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id),
  provider_id INTEGER,
  expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS invites (
  token_hash TEXT PRIMARY KEY,
  org_id INTEGER REFERENCES orgs(id),
  role TEXT NOT NULL,
  expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS repos (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL REFERENCES orgs(id),
  name TEXT NOT NULL,
  url TEXT NOT NULL DEFAULT '',
  kind TEXT NOT NULL DEFAULT '',
  token_hash TEXT NOT NULL UNIQUE,
  pipeline TEXT NOT NULL DEFAULT '',
  UNIQUE (org_id, name)
);
CREATE TABLE IF NOT EXISTS clients (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  redirect_uris TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS grants (
  token_hash TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id),
  provider_id INTEGER,
  client_id TEXT NOT NULL REFERENCES clients(id),
  expires_at INTEGER NOT NULL
);
