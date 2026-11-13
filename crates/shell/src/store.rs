use rusqlite::{Connection, OptionalExtension, Result};

use crate::Role;

#[derive(Clone)]
pub struct Org {
    pub id: i64,
    pub slug: String,
    pub name: String,
}

#[derive(Clone)]
pub struct Repo {
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub url: String,
    pub kind: String,
}

pub struct Provider {
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub issuer: String,
    pub client_id: String,
}

#[derive(Clone)]
pub struct SessionUser {
    pub user_id: i64,
    pub email: String,
    pub admin: bool,
    pub provider: Option<(i64, i64, bool)>,
}

fn org_row(row: &rusqlite::Row) -> Result<Org> {
    Ok(Org { id: row.get(0)?, slug: row.get(1)?, name: row.get(2)? })
}

pub fn org_by_slug(conn: &Connection, slug: &str) -> Result<Option<Org>> {
    conn.query_row("SELECT id, slug, name FROM orgs WHERE slug = ?1", [slug], org_row).optional()
}

pub fn org(conn: &Connection, org_id: i64) -> Result<Option<Org>> {
    conn.query_row("SELECT id, slug, name FROM orgs WHERE id = ?1", [org_id], org_row).optional()
}

pub fn orgs(conn: &Connection) -> Result<Vec<Org>> {
    conn.prepare("SELECT id, slug, name FROM orgs ORDER BY name")?.query_map([], org_row)?.collect()
}

pub fn has_admin(conn: &Connection) -> Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM users WHERE admin = 1)", [], |row| row.get(0))
}

pub fn session(conn: &Connection, token_hash: &str, now: i64) -> Result<Option<SessionUser>> {
    holder(conn, "sessions", token_hash, now)
}

pub fn grant(conn: &Connection, token_hash: &str, now: i64) -> Result<Option<SessionUser>> {
    holder(conn, "grants", token_hash, now)
}

fn holder(conn: &Connection, table: &str, token_hash: &str, now: i64) -> Result<Option<SessionUser>> {
    let row = conn
        .query_row(
            &format!(
                "SELECT u.id, u.email, u.admin, s.provider_id, p.org_id, p.trusted
             FROM {table} s JOIN users u ON u.id = s.user_id
             LEFT JOIN providers p ON p.id = s.provider_id
             WHERE s.token_hash = ?1 AND s.expires_at > ?2"
            ),
            (token_hash, now),
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<bool>>(5)?,
                ))
            },
        )
        .optional()?;
    Ok(row.and_then(|(user_id, email, admin, provider_id, org_id, trusted)| {
        let provider = match (provider_id, org_id, trusted) {
            (None, ..) => None,
            (Some(id), Some(org_id), Some(trusted)) => Some((id, org_id, trusted)),
            (Some(_), ..) => return None,
        };
        Some(SessionUser { user_id, email, admin, provider })
    }))
}

/// The role a session actually carries inside `org_id`. An external sign-in only
/// reaches its own org, never grants suite admin, and is capped at viewer unless trusted.
pub fn role_in(conn: &Connection, user: &SessionUser, org_id: i64) -> Result<Option<Role>> {
    let membership = || -> Result<Option<Role>> {
        let role: Option<String> = conn
            .query_row(
                "SELECT role FROM memberships WHERE user_id = ?1 AND org_id = ?2",
                (user.user_id, org_id),
                |row| row.get(0),
            )
            .optional()?;
        Ok(role.as_deref().and_then(Role::parse))
    };
    match user.provider {
        Some((_, provider_org, _)) if provider_org != org_id => Ok(None),
        Some((_, _, trusted)) => Ok(membership()?.map(|role| if trusted { role } else { Role::Viewer })),
        None if user.admin => Ok(Some(Role::Owner)),
        None => membership(),
    }
}

pub fn create_session(
    conn: &Connection,
    token_hash: &str,
    user_id: i64,
    provider_id: Option<i64>,
    expires_at: i64,
) -> Result<()> {
    conn.execute("DELETE FROM sessions WHERE expires_at < ?1", [crate::now()])?;
    conn.execute(
        "INSERT INTO sessions (token_hash, user_id, provider_id, expires_at) VALUES (?1, ?2, ?3, ?4)",
        (token_hash, user_id, provider_id, expires_at),
    )?;
    Ok(())
}

pub fn orgs_of(conn: &Connection, user: &SessionUser) -> Result<Vec<(Org, Role)>> {
    let mut mine = Vec::new();
    for org in orgs(conn)? {
        if let Some(role) = role_in(conn, user, org.id)? {
            mine.push((org, role));
        }
    }
    Ok(mine)
}

pub fn create_grant(
    conn: &Connection,
    token_hash: &str,
    user_id: i64,
    provider_id: Option<i64>,
    client_id: &str,
    expires_at: i64,
) -> Result<()> {
    conn.execute("DELETE FROM grants WHERE expires_at < ?1", [crate::now()])?;
    conn.execute(
        "INSERT INTO grants (token_hash, user_id, provider_id, client_id, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        (token_hash, user_id, provider_id, client_id, expires_at),
    )?;
    Ok(())
}

pub fn create_client(conn: &Connection, id: &str, name: &str, redirect_uris: &[String]) -> Result<()> {
    conn.execute(
        "INSERT INTO clients (id, name, redirect_uris, created_at) VALUES (?1, ?2, ?3, ?4)",
        (id, name, redirect_uris.join("\n"), crate::now()),
    )?;
    Ok(())
}

pub fn client(conn: &Connection, id: &str) -> Result<Option<(String, Vec<String>)>> {
    conn.query_row("SELECT name, redirect_uris FROM clients WHERE id = ?1", [id], |row| {
        let uris: String = row.get(1)?;
        Ok((row.get(0)?, uris.lines().map(str::to_owned).collect()))
    })
    .optional()
}

pub fn create_invite(conn: &Connection, token_hash: &str, org_id: Option<i64>, role: Role, expires_at: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO invites (token_hash, org_id, role, expires_at) VALUES (?1, ?2, ?3, ?4)",
        (token_hash, org_id, role.as_str(), expires_at),
    )?;
    Ok(())
}

pub fn providers(conn: &Connection) -> Result<Vec<Provider>> {
    conn.prepare("SELECT id, org_id, name, issuer, client_id FROM providers ORDER BY name")?
        .query_map([], |row| {
            Ok(Provider {
                id: row.get(0)?,
                org_id: row.get(1)?,
                name: row.get(2)?,
                issuer: row.get(3)?,
                client_id: row.get(4)?,
            })
        })?
        .collect()
}

fn repo_row(row: &rusqlite::Row) -> Result<Repo> {
    Ok(Repo { id: row.get(0)?, org_id: row.get(1)?, name: row.get(2)?, url: row.get(3)?, kind: row.get(4)? })
}

pub fn repos(conn: &Connection, org_id: i64) -> Result<Vec<Repo>> {
    conn.prepare("SELECT id, org_id, name, url, kind FROM repos WHERE org_id = ?1 ORDER BY name")?
        .query_map([org_id], repo_row)?
        .collect()
}

pub fn repo_by_token(conn: &Connection, token_hash: &str) -> Result<Option<Repo>> {
    conn.query_row("SELECT id, org_id, name, url, kind FROM repos WHERE token_hash = ?1", [token_hash], repo_row)
        .optional()
}

pub fn sign_in_external(conn: &mut Connection, provider_id: i64, org_id: i64, subject: &str, email: &str) -> Result<i64> {
    let tx = conn.transaction()?;
    let known: Option<i64> = tx
        .query_row(
            "SELECT user_id FROM identities WHERE provider_id = ?1 AND subject = ?2",
            (provider_id, subject),
            |row| row.get(0),
        )
        .optional()?;
    let user_id = match known {
        Some(user_id) => user_id,
        None => {
            tx.execute(
                "INSERT INTO users (email, created_at) VALUES (?1, ?2) ON CONFLICT (email) DO NOTHING",
                (email, crate::now()),
            )?;
            let user_id: i64 = tx.query_row("SELECT id FROM users WHERE email = ?1", [email], |row| row.get(0))?;
            tx.execute(
                "INSERT INTO identities (provider_id, subject, user_id) VALUES (?1, ?2, ?3)",
                (provider_id, subject, user_id),
            )?;
            user_id
        }
    };
    tx.execute(
        "INSERT INTO memberships (user_id, org_id, role) VALUES (?1, ?2, 'viewer') ON CONFLICT DO NOTHING",
        (user_id, org_id),
    )?;
    tx.commit()?;
    Ok(user_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        conn.execute_batch(
            "INSERT INTO orgs (id, slug, name) VALUES (1, 'a', 'A'), (2, 'b', 'B');
             INSERT INTO users (id, email, admin, created_at) VALUES (1, 'root@x', 1, 0), (2, 'dev@x', 0, 0);
             INSERT INTO memberships VALUES (2, 1, 'owner'), (2, 2, 'maintainer');
             INSERT INTO providers (id, org_id, name, issuer, client_id, trusted) VALUES (1, 1, 'prod', '', '', 1), (2, 1, 'staging', '', '', 0);",
        )
        .unwrap();
        conn
    }

    fn user(user_id: i64, admin: bool, provider: Option<(i64, i64, bool)>) -> SessionUser {
        SessionUser { user_id, email: String::new(), admin, provider }
    }

    #[test]
    fn external_sign_in_stays_in_its_org_and_untrusted_is_capped() {
        let conn = seeded();
        assert_eq!(role_in(&conn, &user(2, false, None), 1).unwrap(), Some(Role::Owner));
        assert_eq!(role_in(&conn, &user(2, false, Some((1, 1, true))), 1).unwrap(), Some(Role::Owner));
        assert_eq!(role_in(&conn, &user(2, false, Some((2, 1, false))), 1).unwrap(), Some(Role::Viewer));
        assert_eq!(role_in(&conn, &user(2, false, Some((1, 1, true))), 2).unwrap(), None);
        assert_eq!(role_in(&conn, &user(1, true, None), 2).unwrap(), Some(Role::Owner));
        assert_eq!(role_in(&conn, &user(1, true, Some((1, 1, true))), 2).unwrap(), None);
        assert_eq!(role_in(&conn, &user(1, true, Some((1, 1, true))), 1).unwrap(), None);
    }

    #[test]
    fn session_from_a_removed_provider_is_dead() {
        let conn = seeded();
        create_session(&conn, "h", 2, Some(2), i64::MAX).unwrap();
        assert!(session(&conn, "h", 0).unwrap().is_some());
        conn.execute_batch("PRAGMA foreign_keys = OFF; DELETE FROM providers WHERE id = 2;").unwrap();
        assert!(session(&conn, "h", 0).unwrap().is_none());
    }

    #[test]
    fn external_sign_in_links_by_email_and_adds_a_viewer_membership() {
        let mut conn = seeded();
        let existing = sign_in_external(&mut conn, 1, 1, "sub-1", "dev@x").unwrap();
        assert_eq!(existing, 2);
        assert_eq!(role_in(&conn, &user(2, false, None), 1).unwrap(), Some(Role::Owner));
        let fresh = sign_in_external(&mut conn, 1, 1, "sub-2", "new@x").unwrap();
        assert_eq!(sign_in_external(&mut conn, 1, 1, "sub-2", "renamed@x").unwrap(), fresh);
        assert_eq!(role_in(&conn, &user(fresh, false, None), 1).unwrap(), Some(Role::Viewer));
    }
}

pub struct Member {
    pub user_id: i64,
    pub email: String,
    pub role: String,
}

pub enum Joined {
    User(i64),
    BadInvite,
    EmailTaken,
}

pub enum Joiner {
    Existing(i64),
    New { email: String, password_hash: String },
}

pub fn password_hash(conn: &Connection, email: &str) -> Result<Option<(i64, String)>> {
    conn.query_row("SELECT id, password_hash FROM users WHERE email = ?1 AND password_hash IS NOT NULL", [email], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })
    .optional()
}

pub fn delete_session(conn: &Connection, token_hash: &str) -> Result<()> {
    conn.execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
    Ok(())
}

pub fn invite_exists(conn: &Connection, token_hash: &str, now: i64) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM invites WHERE token_hash = ?1 AND expires_at > ?2)",
        (token_hash, now),
        |row| row.get(0),
    )
}

pub fn accept_invite(conn: &mut Connection, token_hash: &str, now: i64, joiner: Joiner) -> Result<Joined> {
    let tx = conn.transaction()?;
    let invite: Option<(Option<i64>, String)> = tx
        .query_row(
            "SELECT org_id, role FROM invites WHERE token_hash = ?1 AND expires_at > ?2",
            (token_hash, now),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((org_id, role)) = invite else { return Ok(Joined::BadInvite) };
    let user_id = match joiner {
        Joiner::Existing(user_id) => user_id,
        Joiner::New { email, password_hash } => {
            let created = tx.execute(
                "INSERT INTO users (email, password_hash, created_at) VALUES (?1, ?2, ?3) ON CONFLICT (email) DO NOTHING",
                (&email, &password_hash, now),
            )?;
            if created == 0 {
                return Ok(Joined::EmailTaken);
            }
            tx.last_insert_rowid()
        }
    };
    match org_id {
        Some(org_id) => tx.execute(
            "INSERT INTO memberships (user_id, org_id, role) VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
            (user_id, org_id, &role),
        )?,
        None => tx.execute("UPDATE users SET admin = 1 WHERE id = ?1", [user_id])?,
    };
    tx.execute("DELETE FROM invites WHERE token_hash = ?1", [token_hash])?;
    tx.commit()?;
    Ok(Joined::User(user_id))
}

pub fn create_org(conn: &Connection, slug: &str, name: &str) -> Result<bool> {
    let created = conn.execute("INSERT INTO orgs (slug, name) VALUES (?1, ?2) ON CONFLICT DO NOTHING", (slug, name))?;
    Ok(created == 1)
}

pub fn members(conn: &Connection, org_id: i64) -> Result<Vec<Member>> {
    conn.prepare(
        "SELECT u.id, u.email, m.role FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.org_id = ?1 ORDER BY u.email",
    )?
    .query_map([org_id], |row| Ok(Member { user_id: row.get(0)?, email: row.get(1)?, role: row.get(2)? }))?
    .collect()
}

pub fn set_role(conn: &Connection, org_id: i64, user_id: i64, role: Option<Role>) -> Result<()> {
    match role {
        Some(role) => conn.execute(
            "UPDATE memberships SET role = ?3 WHERE org_id = ?1 AND user_id = ?2",
            (org_id, user_id, role.as_str()),
        )?,
        None => conn.execute("DELETE FROM memberships WHERE org_id = ?1 AND user_id = ?2", (org_id, user_id))?,
    };
    Ok(())
}

pub fn create_repo(conn: &Connection, org_id: i64, name: &str, url: &str, kind: &str, token_hash: &str) -> Result<bool> {
    let created = conn.execute(
        "INSERT INTO repos (org_id, name, url, kind, token_hash) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
        (org_id, name, url, kind, token_hash),
    )?;
    Ok(created == 1)
}

pub fn update_repo(conn: &Connection, org_id: i64, id: i64, url: &str, kind: &str) -> Result<()> {
    conn.execute("UPDATE repos SET url = ?3, kind = ?4 WHERE org_id = ?1 AND id = ?2", (org_id, id, url, kind))?;
    Ok(())
}

pub fn rotate_repo_token(conn: &Connection, org_id: i64, id: i64, token_hash: &str) -> Result<Option<String>> {
    conn.query_row(
        "UPDATE repos SET token_hash = ?3 WHERE org_id = ?1 AND id = ?2 RETURNING name",
        (org_id, id, token_hash),
        |row| row.get(0),
    )
    .optional()
}

pub fn create_provider(conn: &Connection, org_id: i64, name: &str, issuer: &str, client_id: &str, trusted: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO providers (org_id, name, issuer, client_id, trusted) VALUES (?1, ?2, ?3, ?4, ?5)",
        (org_id, name, issuer, client_id, trusted),
    )?;
    Ok(())
}

pub fn delete_provider(conn: &mut Connection, org_id: i64, id: i64) -> Result<()> {
    let tx = conn.transaction()?;
    let owned: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM providers WHERE id = ?1 AND org_id = ?2)",
        (id, org_id),
        |row| row.get(0),
    )?;
    if owned {
        tx.execute("DELETE FROM sessions WHERE provider_id = ?1", [id])?;
        tx.execute("DELETE FROM grants WHERE provider_id = ?1", [id])?;
        tx.execute("DELETE FROM identities WHERE provider_id = ?1", [id])?;
        tx.execute("DELETE FROM providers WHERE id = ?1", [id])?;
    }
    tx.commit()
}
