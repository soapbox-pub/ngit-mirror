//! Brings one GitHub repo in line with a nostr repo state, using a local
//! bare repo as the object cache and the `git` binary for transport.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use nostr_sdk::prelude::*;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::github::GitHub;
use crate::store::Target;

const GIT_TIMEOUT: Duration = Duration::from_secs(3600);

pub struct Mirror {
    pub gh: GitHub,
    pub owner: PublicKey,
    pub data_dir: PathBuf,
    pub prune: bool,
    pub dry_run: bool,
    pub permits: Semaphore,
}

impl Mirror {
    #[tracing::instrument(skip_all, fields(repo = %target.id))]
    pub async fn sync(&self, target: &Target) -> Result<()> {
        let Some(state) = &target.state else {
            debug!("no state event from a maintainer yet");
            return Ok(());
        };
        let _permit = self.permits.acquire().await?;

        let dir = self.data_dir.join(format!("{}.git", target.id));
        if !dir.join("HEAD").exists() {
            std::fs::create_dir_all(&dir)?;
            self.git(&dir, &["init", "--bare", "--quiet"], None).await?;
        }

        let mut missing = self.missing(&dir, state.refs.values()).await?;
        for (i, url) in target.clone_urls.iter().enumerate() {
            if missing.is_empty() {
                break;
            }
            debug!(%url, missing = missing.len(), "fetching");
            let heads = format!("+refs/heads/*:refs/sources/{i}/heads/*");
            let tags = format!("+refs/tags/*:refs/sources/{i}/tags/*");
            let args = [
                "fetch",
                "--quiet",
                "--prune",
                "--no-tags",
                "--no-write-fetch-head",
                url,
                &heads,
                &tags,
            ];
            if let Err(e) = self.git(&dir, &args, None).await {
                warn!(%url, "fetch failed: {e:#}");
                continue;
            }
            missing = self.missing(&dir, state.refs.values()).await?;
        }
        if !missing.is_empty() {
            bail!(
                "{} of {} ref targets not available from any clone URL yet (e.g. {})",
                missing.len(),
                state.refs.len(),
                missing[0]
            );
        }
        self.pin(&dir, &state.refs).await?;

        let name = &target.id;
        let mut repo = self.gh.repo(name).await?;
        if repo.as_ref().is_some_and(|r| r.archived) {
            bail!("{}/{name} is archived on GitHub", self.gh.org);
        }
        if repo.is_none() {
            if self.dry_run {
                info!("would create {}/{name}", self.gh.org);
            } else {
                let homepage = target.web.clone().or_else(|| self.gitworkshop_url(name));
                repo = Some(
                    self.gh
                        .create_repo(name, target.description.as_deref(), homepage.as_deref())
                        .await?,
                );
                info!("created {}/{name}", self.gh.org);
            }
        }

        let url = format!("https://github.com/{}/{name}.git", self.gh.org);
        let remote = if repo.is_some() {
            self.ls_remote(&dir, &url).await?
        } else {
            BTreeMap::new()
        };

        let mut refspecs = Vec::new();
        for (r, oid) in &state.refs {
            if remote.get(r) != Some(oid) {
                refspecs.push(format!("+{oid}:{r}"));
            }
        }
        if self.prune {
            for r in remote.keys().filter(|r| !state.refs.contains_key(*r)) {
                refspecs.push(format!(":{r}"));
            }
        }

        if refspecs.is_empty() {
            debug!("up to date");
        } else if self.dry_run {
            info!(state = %state.event_id, "would push {}", refspecs.join(" "));
        } else {
            let mut args = vec!["push", "--porcelain", "--atomic", &url];
            args.extend(refspecs.iter().map(String::as_str));
            self.git(&dir, &args, None).await?;
            info!(state = %state.event_id, updated = refspecs.len(), "pushed");
        }

        if let (Some(head), Some(repo)) = (&state.head, &repo)
            && repo.default_branch.as_ref() != Some(head)
        {
            if self.dry_run {
                info!("would set default branch to {head}");
            } else {
                self.gh.set_default_branch(name, head).await?;
                info!("set default branch to {head}");
            }
        }
        Ok(())
    }

    fn gitworkshop_url(&self, id: &str) -> Option<String> {
        let npub = self.owner.to_bech32().ok()?;
        Some(format!("https://gitworkshop.dev/{npub}/{id}"))
    }

    /// Object ids from `oids` that are not in the local repo.
    async fn missing<'a>(
        &self,
        dir: &Path,
        oids: impl Iterator<Item = &'a String>,
    ) -> Result<Vec<String>> {
        let input: String = oids.map(|o| format!("{o}\n")).collect();
        let out = self
            .git(dir, &["cat-file", "--batch-check"], Some(input))
            .await?;
        Ok(out
            .lines()
            .filter_map(|l| l.strip_suffix(" missing"))
            .map(String::from)
            .collect())
    }

    /// Points the cache's own heads and tags at the state, so the objects
    /// stay reachable whatever the clone URLs do later.
    async fn pin(&self, dir: &Path, refs: &BTreeMap<String, String>) -> Result<()> {
        let existing = self
            .git(
                dir,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/heads",
                    "refs/tags",
                ],
                None,
            )
            .await?;
        let mut input = String::new();
        for r in existing.lines().filter(|r| !refs.contains_key(*r)) {
            input.push_str(&format!("delete {r}\n"));
        }
        for (r, oid) in refs {
            input.push_str(&format!("update {r} {oid}\n"));
        }
        self.git(dir, &["update-ref", "--stdin"], Some(input))
            .await?;
        Ok(())
    }

    async fn ls_remote(&self, dir: &Path, url: &str) -> Result<BTreeMap<String, String>> {
        let out = self.git(dir, &["ls-remote", "--refs", url], None).await?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .filter(|(_, r)| r.starts_with("refs/heads/") || r.starts_with("refs/tags/"))
            .map(|(oid, r)| (r.to_string(), oid.to_string()))
            .collect())
    }

    async fn git(&self, dir: &Path, args: &[&str], stdin: Option<String>) -> Result<String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("LC_ALL", "C")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Passed through the environment rather than argv so it stays out of `ps`.
        if let Some(token) = self.gh.token().await? {
            let basic = BASE64.encode(format!("x-access-token:{token}"));
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
                .env(
                    "GIT_CONFIG_VALUE_0",
                    format!("AUTHORIZATION: basic {basic}"),
                );
        }

        let mut child = cmd.spawn().context("running git")?;
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().expect("piped stdin");
            pipe.write_all(input.as_bytes()).await?;
        }
        let out = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
            .await
            .with_context(|| format!("git {} timed out", args[0]))??;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args[0],
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}
