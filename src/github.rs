//! Minimal GitHub REST client: auth (PAT or GitHub App) and the few repo
//! operations the mirror needs.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

const API: &str = "https://api.github.com";

enum Auth {
    None,
    Token(String),
    App {
        app_id: String,
        key: EncodingKey,
        installation_id: Option<u64>,
    },
}

pub struct GitHub {
    pub org: String,
    http: reqwest::Client,
    auth: Auth,
    cached: Mutex<Option<(String, Instant)>>,
}

#[derive(Debug, Deserialize)]
pub struct Repo {
    pub default_branch: Option<String>,
    #[serde(default)]
    pub archived: bool,
}

#[derive(Serialize)]
struct AppClaims {
    iat: u64,
    exp: u64,
    iss: String,
}

impl GitHub {
    /// Auth comes from the environment: `GITHUB_TOKEN`, or `GITHUB_APP_ID`
    /// plus `GITHUB_APP_PRIVATE_KEY` / `GITHUB_APP_PRIVATE_KEY_PATH` (and
    /// optionally `GITHUB_APP_INSTALLATION_ID`). With neither, only public
    /// reads work, which is enough for `--dry-run`.
    pub fn from_env(org: String) -> Result<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let auth = if let Some(token) = var("GITHUB_TOKEN") {
            Auth::Token(token)
        } else if let Some(app_id) = var("GITHUB_APP_ID") {
            let pem = match (
                var("GITHUB_APP_PRIVATE_KEY"),
                var("GITHUB_APP_PRIVATE_KEY_PATH"),
            ) {
                (Some(pem), _) => pem,
                (None, Some(path)) => {
                    std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?
                }
                (None, None) => bail!("GITHUB_APP_ID is set but no private key was given"),
            };
            Auth::App {
                app_id,
                key: EncodingKey::from_rsa_pem(pem.as_bytes())
                    .context("parsing app private key")?,
                installation_id: var("GITHUB_APP_INSTALLATION_ID")
                    .map(|v| v.parse())
                    .transpose()
                    .context("GITHUB_APP_INSTALLATION_ID")?,
            }
        } else {
            Auth::None
        };
        Ok(Self {
            org,
            http: reqwest::Client::builder()
                .user_agent(concat!("ngit-mirror/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(60))
                .build()?,
            auth,
            cached: Mutex::new(None),
        })
    }

    pub fn has_auth(&self) -> bool {
        !matches!(self.auth, Auth::None)
    }

    /// A token usable for both the REST API and git over HTTPS.
    pub async fn token(&self) -> Result<Option<String>> {
        let (app_id, key, installation_id) = match &self.auth {
            Auth::None => return Ok(None),
            Auth::Token(t) => return Ok(Some(t.clone())),
            Auth::App {
                app_id,
                key,
                installation_id,
            } => (app_id, key, installation_id),
        };

        let mut cached = self.cached.lock().await;
        if let Some((token, expires)) = &*cached
            && Instant::now() + Duration::from_secs(300) < *expires
        {
            return Ok(Some(token.clone()));
        }

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let claims = AppClaims {
            iat: now - 60,
            exp: now + 540,
            iss: app_id.clone(),
        };
        let jwt = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, key)?;

        let installation_id = match installation_id {
            Some(id) => *id,
            None => {
                #[derive(Deserialize)]
                struct Installation {
                    id: u64,
                }
                let path = format!("/orgs/{}/installation", self.org);
                let i: Installation = send(self.request(Method::GET, &path, &jwt)).await?;
                i.id
            }
        };

        #[derive(Deserialize)]
        struct AccessToken {
            token: String,
        }
        let path = format!("/app/installations/{installation_id}/access_tokens");
        let t: AccessToken = send(self.request(Method::POST, &path, &jwt)).await?;
        // Installation tokens last an hour.
        *cached = Some((t.token.clone(), Instant::now() + Duration::from_secs(3600)));
        Ok(Some(t.token))
    }

    fn request(&self, method: Method, path: &str, token: &str) -> RequestBuilder {
        let req = self
            .http
            .request(method, format!("{API}{path}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if token.is_empty() {
            req
        } else {
            req.bearer_auth(token)
        }
    }

    async fn api(&self, method: Method, path: &str) -> Result<RequestBuilder> {
        let token = self.token().await?.unwrap_or_default();
        Ok(self.request(method, path, &token))
    }

    pub async fn repo(&self, name: &str) -> Result<Option<Repo>> {
        let path = format!("/repos/{}/{name}", self.org);
        let resp = self.api(Method::GET, &path).await?.send().await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(check(resp).await?.json().await?))
    }

    pub async fn create_repo(
        &self,
        name: &str,
        description: Option<&str>,
        homepage: Option<&str>,
    ) -> Result<Repo> {
        let path = format!("/orgs/{}/repos", self.org);
        let body = json!({
            "name": name,
            "description": description.map(|d| truncate(d, 350)),
            "homepage": homepage,
        });
        send(self.api(Method::POST, &path).await?.json(&body)).await
    }

    pub async fn delete_repo(&self, name: &str) -> Result<()> {
        let path = format!("/repos/{}/{name}", self.org);
        check(self.api(Method::DELETE, &path).await?.send().await?).await?;
        Ok(())
    }

    pub async fn set_default_branch(&self, name: &str, branch: &str) -> Result<()> {
        let path = format!("/repos/{}/{name}", self.org);
        let body = json!({ "default_branch": branch });
        let _: serde_json::Value = send(self.api(Method::PATCH, &path).await?.json(&body)).await?;
        Ok(())
    }
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let url = resp.url().path().to_string();
    let body = resp.text().await.unwrap_or_default();
    bail!("GitHub {status} on {url}: {}", body.trim())
}

async fn send<T: for<'de> Deserialize<'de>>(req: RequestBuilder) -> Result<T> {
    Ok(check(req.send().await?).await?.json().await?)
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
