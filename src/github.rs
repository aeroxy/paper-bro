//! Publishing: each course is one page, `arxiv-<id>.html`, committed to the
//! paper-bro/paper-bro.github.io repo by the aero-oops-wtf GitHub App, and
//! GitHub Pages serves it at `https://paper-bro.github.io/arxiv-<id>`. Pages
//! replaced AERO in November 2026, after AERO's 90-day token lapsed unnoticed
//! and every build failed at publish for nine days. An App key doesn't
//! expire, and the server checks at startup that it still gets a token.
//!
//! Auth is the App flow finance-bro uses: an RS256 JWT signed with the App's
//! key, exchanged for an installation token that lasts an hour. The App has
//! contents:write and nothing more: enough to push, not to change Pages.
//!
//! A publish is one commit through the Git Data API: the files inline in a
//! tree on top of the branch's, a commit, then a fast-forward of the branch,
//! retried if someone pushed in between. Pages deploys it within a minute or
//! so, and the build waits for the page to answer before calling it ready.

use anyhow::{Context, Result, bail};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::GitHub;

const API: &str = "https://api.github.com";

/// A course's page in the repo. Pages answers `/<alias>` with it.
pub fn page_path(alias: &str) -> String {
    format!("{alias}.html")
}

pub fn page_url(gh: &GitHub, alias: &str) -> String {
    format!("{}/{alias}", gh.site_url)
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().user_agent("paper-bro").timeout(Duration::from_secs(60)).build()?)
}

struct Client {
    http: reqwest::Client,
    api: String,
    token: String,
}

impl Client {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<(reqwest::StatusCode, Value)> {
        let mut req = self
            .http
            .request(method.clone(), format!("{}{path}", self.api))
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.with_context(|| format!("GitHub {method} {path} failed"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::String(text))))
    }

    async fn ok(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let (status, v) = self.call(method.clone(), path, body).await?;
        if !status.is_success() {
            bail!("GitHub {method} {path}: {status} {}", v["message"].as_str().unwrap_or(&v.to_string()));
        }
        Ok(v)
    }

    /// One commit on `branch` that writes each `Some` file and deletes each
    /// `None` one, on top of what the branch holds. Returns the commit's sha.
    async fn commit(&self, repo: &str, branch: &str, files: &[(String, Option<&str>)], message: &str) -> Result<String> {
        let tree: Vec<Value> = files
            .iter()
            .map(|(path, text)| match text {
                Some(text) => json!({"path": path, "mode": "100644", "type": "blob", "content": text}),
                None => json!({"path": path, "mode": "100644", "type": "blob", "sha": null}),
            })
            .collect();
        // A push between reading the head and moving it fails the
        // fast-forward; read again and retry.
        for attempt in 1..=3 {
            let (status, head) = self.call(Method::GET, &format!("/repos/{repo}/git/ref/heads/{branch}"), None).await?;
            let parent = match status.as_u16() {
                200 => Some(head["object"]["sha"].as_str().context("branch ref without a sha")?.to_string()),
                404 => None,
                _ => bail!("GitHub GET ref {branch}: {status} {head}"),
            };
            let mut body = json!({"tree": tree});
            if let Some(parent) = &parent {
                let commit = self.ok(Method::GET, &format!("/repos/{repo}/git/commits/{parent}"), None).await?;
                body["base_tree"] = commit["tree"]["sha"].clone();
            }
            let new_tree = self.ok(Method::POST, &format!("/repos/{repo}/git/trees"), Some(body)).await?;
            let parents: Vec<&str> = parent.iter().map(String::as_str).collect();
            let commit = self
                .ok(Method::POST, &format!("/repos/{repo}/git/commits"), Some(json!({"message": message, "tree": new_tree["sha"], "parents": parents})))
                .await?;
            let sha = commit["sha"].as_str().context("commit without a sha")?.to_string();
            let (status, v) = match parent {
                Some(_) => self.call(Method::PATCH, &format!("/repos/{repo}/git/refs/heads/{branch}"), Some(json!({"sha": sha, "force": false}))).await?,
                None => self.call(Method::POST, &format!("/repos/{repo}/git/refs"), Some(json!({"ref": format!("refs/heads/{branch}"), "sha": sha}))).await?,
            };
            if status.is_success() {
                return Ok(sha);
            }
            if status.as_u16() != 422 || attempt == 3 {
                bail!("moving {branch} to the new commit failed: {status} {}", v["message"].as_str().unwrap_or(&v.to_string()));
            }
            log::warn!("{branch} moved while publishing; retrying");
        }
        unreachable!("the loop returns or bails on its last attempt")
    }
}

/// An installation token for `gh.repo`, valid for an hour.
async fn token(gh: &GitHub, http: &reqwest::Client) -> Result<String> {
    #[derive(Serialize)]
    struct Claims<'a> {
        iat: u64,
        exp: u64,
        iss: &'a str,
    }
    let path = gh.key_path.as_ref().context("GH_APP_KEY_PATH is not set: publishing needs the aero-oops-wtf GitHub App's private key")?;
    let pem = std::fs::read(path).with_context(|| format!("can't read the GitHub App key at {} (GH_APP_KEY_PATH)", path.display()))?;
    let key = EncodingKey::from_rsa_pem(&pem).context("the GitHub App key is not an RSA PEM")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    // Back-dated a minute for clock drift; GitHub allows ten minutes at most.
    let jwt = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &Claims { iat: now - 60, exp: now + 540, iss: &gh.client_id }, &key)?;
    let app = Client { http: http.clone(), api: API.into(), token: jwt };
    let installation = app
        .ok(Method::GET, &format!("/repos/{}/installation", gh.repo), None)
        .await
        .with_context(|| format!("is the App installed on {}?", gh.repo))?;
    let id = installation["id"].as_u64().context("installation without an id")?;
    let token = app.ok(Method::POST, &format!("/app/installations/{id}/access_tokens"), None).await?;
    Ok(token["token"].as_str().context("access token response without a token")?.to_string())
}

/// Whether the App can publish to `gh.repo`: a token, and nothing written.
pub async fn check(gh: &GitHub) -> Result<()> {
    token(gh, &http()?).await.map(|_| ())
}

/// Write (`Some`) or delete (`None`) each file in one commit on `gh.branch`,
/// and return the commit's sha.
pub async fn commit(gh: &GitHub, files: &[(String, Option<&str>)], message: &str) -> Result<String> {
    let http = http()?;
    let client = Client { token: token(gh, &http).await?, api: API.into(), http };
    client.commit(&gh.repo, &gh.branch, files, message).await
}

/// Ask for `url` up to `tries` times, `every` apart, until it answers 200.
/// Each ask carries a fresh query string, so a cache can't answer for Pages
/// and none ends up holding a 404 for the real address.
pub async fn wait_until_live(url: &str, every: Duration, tries: u32) -> bool {
    let Ok(http) = http() else { return false };
    for n in 0..tries {
        if n > 0 {
            tokio::time::sleep(every).await;
        }
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or_default();
        if http.get(format!("{url}?live={stamp}-{n}")).send().await.is_ok_and(|r| r.status().is_success()) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Bytes;
    use axum::http::{StatusCode, Uri};
    use std::sync::{Arc, Mutex};

    /// A stand-in for the Git Data API: `main` is at commit c0 with tree t0,
    /// and the first `pushes` fast-forwards fail as if someone pushed first.
    /// Returns a client for it and the requests it saw.
    async fn fake_github(pushes: usize) -> (Client, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let pushes = Arc::new(Mutex::new(pushes));
        let log = seen.clone();
        let api = Router::new().fallback(move |method: axum::http::Method, uri: Uri, body: Bytes| {
            let (log, pushes) = (log.clone(), pushes.clone());
            async move {
                let path = uri.path().to_string();
                log.lock().unwrap().push(format!("{method} {path} {}", String::from_utf8_lossy(&body)));
                let (status, v) = match (method.as_str(), path.as_str()) {
                    ("GET", "/repos/o/r/git/ref/heads/main") => (StatusCode::OK, json!({"object": {"sha": "c0"}})),
                    ("GET", "/repos/o/r/git/commits/c0") => (StatusCode::OK, json!({"tree": {"sha": "t0"}})),
                    ("POST", "/repos/o/r/git/trees") => (StatusCode::CREATED, json!({"sha": "t1"})),
                    ("POST", "/repos/o/r/git/commits") => (StatusCode::CREATED, json!({"sha": "c1"})),
                    ("PATCH", "/repos/o/r/git/refs/heads/main") => {
                        let mut left = pushes.lock().unwrap();
                        if *left > 0 {
                            *left -= 1;
                            (StatusCode::UNPROCESSABLE_ENTITY, json!({"message": "Update is not a fast forward"}))
                        } else {
                            (StatusCode::OK, json!({}))
                        }
                    }
                    _ => (StatusCode::NOT_FOUND, json!({"message": "Not Found"})),
                };
                (status, axum::Json(v))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
        (Client { http: reqwest::Client::new(), api: format!("http://{addr}"), token: "t".into() }, seen)
    }

    #[tokio::test]
    async fn a_page_and_a_removal_go_up_as_one_commit_on_top_of_the_branch() {
        let (client, seen) = fake_github(0).await;
        let files = [("arxiv-1.html".to_string(), Some("<p>hi</p>")), ("arxiv-2.html".to_string(), None)];
        assert_eq!(client.commit("o/r", "main", &files, "Publish").await.unwrap(), "c1");
        let seen = seen.lock().unwrap();
        let tree = seen.iter().find(|r| r.starts_with("POST /repos/o/r/git/trees")).unwrap();
        let tree: Value = serde_json::from_str(tree.splitn(3, ' ').nth(2).unwrap()).unwrap();
        assert_eq!(tree["base_tree"], "t0");
        assert_eq!(tree["tree"][0]["content"], "<p>hi</p>");
        assert!(tree["tree"][1]["sha"].is_null() && tree["tree"][1].get("content").is_none());
        assert!(seen.iter().any(|r| r.starts_with("POST /repos/o/r/git/commits") && r.contains(r#""parents":["c0"]"#)));
        assert!(seen.iter().any(|r| r.starts_with("PATCH /repos/o/r/git/refs/heads/main")));
    }

    #[tokio::test]
    async fn a_branch_that_moved_is_read_again_and_the_commit_retried() {
        let (client, seen) = fake_github(1).await;
        let files = [("arxiv-1.html".to_string(), Some("<p>hi</p>"))];
        assert_eq!(client.commit("o/r", "main", &files, "Publish").await.unwrap(), "c1");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().filter(|r| r.starts_with("GET /repos/o/r/git/ref/heads/main")).count(), 2);
        assert_eq!(seen.iter().filter(|r| r.starts_with("PATCH")).count(), 2);
    }

    #[tokio::test]
    async fn waiting_for_a_page_stops_when_it_answers_or_after_the_last_try() {
        let asks = Arc::new(Mutex::new(0));
        let n = asks.clone();
        let site = Router::new().fallback(move || {
            let n = n.clone();
            async move {
                let mut n = n.lock().unwrap();
                *n += 1;
                if *n >= 3 { StatusCode::OK } else { StatusCode::NOT_FOUND }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/arxiv-1", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, site).await.unwrap() });
        assert!(!wait_until_live(&url, Duration::from_millis(5), 2).await);
        assert!(wait_until_live(&url, Duration::from_millis(5), 5).await);
        assert_eq!(*asks.lock().unwrap(), 3);
    }
}
