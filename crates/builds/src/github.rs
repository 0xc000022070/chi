use std::{collections::HashMap, path::Path, time::Duration};

use rusqlite::OptionalExtension;
use serde::Deserialize;
use shell::{Failure, Repo, now};

use crate::{Builds, MAX_ARTIFACT, MAX_ARTIFACTS_PER_BUILD, file_sha256, safe_name, verify};

const EVERY: Duration = Duration::from_secs(600);

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    target_commitish: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    browser_download_url: String,
}

impl Release {
    fn version(&self) -> Option<String> {
        let version = self.tag_name.trim_start_matches('v');
        let fits = !self.assets.is_empty()
            && self.assets.len() <= MAX_ARTIFACTS_PER_BUILD
            && self.assets.iter().all(|a| safe_name(&a.name) && a.size <= MAX_ARTIFACT);
        (fits && !version.is_empty() && version.len() <= 200).then(|| version.to_owned())
    }
}

fn slug(url: &str) -> Option<String> {
    let path = shell::same_repo(url);
    let path = path.strip_prefix("github.com/")?;
    let mut parts = path.split('/');
    let fine = |part: Option<&str>| part.is_some_and(|p| !p.is_empty() && !p.contains(['?', '#']));
    (fine(parts.next()) && fine(parts.next()) && parts.next().is_none()).then(|| path.to_owned())
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(120))
        .user_agent("chi")
        .build()
}

fn latest(slug: &str) -> Result<Option<Release>, String> {
    match agent()
        .get(&format!(
            "https://api.github.com/repos/{slug}/releases/latest"
        ))
        .call()
    {
        Ok(response) => response.into_json().map(Some).map_err(|e| e.to_string()),
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn download(assets: &[Asset], dir: &Path) -> Result<HashMap<String, String>, String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut sums = HashMap::new();
    for asset in assets {
        let part = dir.join(format!("{}{}", asset.name, crate::PART));
        let mut file = std::fs::File::create(&part).map_err(|e| e.to_string())?;
        let body = agent()
            .get(&asset.browser_download_url)
            .call()
            .map_err(|e| e.to_string())?;
        std::io::copy(
            &mut std::io::Read::take(body.into_reader(), MAX_ARTIFACT),
            &mut file,
        )
        .map_err(|e| e.to_string())?;
        sums.insert(
            asset.name.clone(),
            file_sha256(&part).map_err(|e| e.to_string())?,
        );
    }
    Ok(sums)
}

impl Builds {
    pub(crate) fn follow_github(&self) {
        let builds = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(EVERY);
            loop {
                tick.tick().await;
                let repos = builds.shell.all_repos().await.unwrap_or_default();
                for repo in repos {
                    let Some(slug) = slug(&repo.url) else {
                        continue;
                    };
                    if let Err(Failure(_, message)) = builds.import_release(&repo, slug).await {
                        eprintln!("builds: github release of {}: {message}", repo.url);
                    }
                }
            }
        });
    }

    /// Imports the newest GitHub release of `repo` as a finished `release` build,
    /// once per release tag, so `/app.apk` and pipelines see it like an upload.
    async fn import_release(&self, repo: &Repo, slug: String) -> Result<(), Failure> {
        let fetch = move || latest(&slug);
        let Some(release) = tokio::task::spawn_blocking(fetch).await?.map_err(failure)? else {
            return Ok(());
        };
        let Some(version) = release.version() else {
            return Ok(());
        };
        if self.imported(repo.id, version.clone()).await? {
            return Ok(());
        }
        self.store(repo, version, release).await
    }

    async fn imported(&self, repo_id: i64, version: String) -> Result<bool, Failure> {
        self.db
            .call(move |conn| {
                conn.query_row(
                    "SELECT 1 FROM builds WHERE repo_id = ?1 AND builder = 'github' AND version = ?2 AND status = 'finished'",
                    (repo_id, version),
                    |_| Ok(()),
                )
                .optional()
            })
            .await
            .map(|found| found.is_some())
    }

    async fn store(&self, repo: &Repo, version: String, release: Release) -> Result<(), Failure> {
        let staging = self.dir.join(format!("github-{}{}", repo.id, crate::PART));
        let (to, assets) = (staging.clone(), release.assets);
        let sums = tokio::task::spawn_blocking(move || download(&assets, &to))
            .await?
            .map_err(failure)?;
        let (org_id, repo_id, commit) = (repo.org_id, repo.id, release.target_commitish);
        let id = self
            .db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO builds (org_id, repo_id, version, commit_sha, branch, tag, builder, status, started_at)
                     VALUES (?1, ?2, ?3, ?4, '', 'release', 'github', 'uploading', ?5)",
                    (org_id, repo_id, version, commit, now()),
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await?;
        let dir = self.build_dir(id);
        tokio::fs::rename(&staging, &dir).await?;
        let files = tokio::task::spawn_blocking(move || verify(&dir, &sums))
            .await??
            .map_err(failure)?;
        let build = self.build(id).await?.ok_or_else(Failure::not_found)?;
        self.seal(build, files).await
    }
}

fn failure(message: String) -> Failure {
    Failure(axum::http::StatusCode::BAD_GATEWAY, message)
}

#[cfg(test)]
mod tests {
    use super::slug;

    #[test]
    fn only_github_repo_urls_have_a_slug() {
        assert_eq!(
            slug("https://github.com/0xc000022070/chi").as_deref(),
            Some("0xc000022070/chi")
        );
        assert_eq!(
            slug("git@github.com:Org/Repo.git").as_deref(),
            Some("org/repo")
        );
        for bad in [
            "",
            "https://gitlab.com/a/b",
            "https://github.com/a",
            "https://github.com/a/b/c",
        ] {
            assert_eq!(slug(bad), None, "{bad}");
        }
    }
}
