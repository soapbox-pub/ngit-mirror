mod config;
mod github;
mod mirror;
mod store;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use nostr_sdk::prelude::*;
use tokio::sync::{Semaphore, watch};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::github::GitHub;
use crate::mirror::Mirror;
use crate::store::{ANNOUNCEMENT, STATE, Store, Target};

/// Mirror ngit (NIP-34) repositories to GitHub as soon as their state changes.
#[derive(Parser)]
#[command(version)]
struct Args {
    #[arg(short, long, env = "NGIT_MIRROR_CONFIG", default_value = "config.toml")]
    config: PathBuf,
    /// Log what would change on GitHub without changing anything.
    #[arg(long)]
    dry_run: bool,
    /// Reconcile every repo once and exit.
    #[arg(long)]
    once: bool,
    /// Only mirror these repo identifiers (repeatable).
    #[arg(long)]
    only: Vec<String>,
}

const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_ATTEMPTS: u32 = 10;

/// Repo ids, maintainers and relays the live subscription was built from.
type SubscriptionKey = (BTreeSet<String>, BTreeSet<PublicKey>, BTreeSet<String>);

struct App {
    cfg: Config,
    only: Vec<String>,
    owner: PublicKey,
    client: Client,
    store: Mutex<Store>,
    mirror: Arc<Mirror>,
    workers: Mutex<HashMap<String, watch::Sender<Option<Target>>>>,
    subscribed: Mutex<Option<SubscriptionKey>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nostr_sdk=error".into()),
        )
        .init();

    // reqwest and nostr-sdk enable different rustls backends, so rustls
    // can't pick one on its own.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    let owner = PublicKey::parse(&cfg.owner).context("owner")?;
    let gh = GitHub::from_env(cfg.org.clone())?;
    if !gh.has_auth() && !args.dry_run {
        bail!("set GITHUB_TOKEN or GITHUB_APP_ID (or use --dry-run)");
    }
    std::fs::create_dir_all(&cfg.data_dir)?;

    let mirror = Arc::new(Mirror {
        gh,
        owner,
        data_dir: cfg.data_dir.clone(),
        prune: cfg.prune,
        dry_run: args.dry_run,
        permits: Semaphore::new(cfg.concurrency.max(1)),
    });
    let app = Arc::new(App {
        only: args.only,
        owner,
        client: Client::default(),
        store: Mutex::new(Store::new(owner)),
        mirror,
        workers: Mutex::new(HashMap::new()),
        subscribed: Mutex::new(None),
        cfg,
    });

    app.add_relays().await;
    app.fetch_all().await;

    if args.once {
        return app.run_once().await;
    }
    app.run().await
}

impl App {
    fn wanted(&self, id: &str) -> bool {
        if !self.only.is_empty() && !self.only.iter().any(|o| o == id) {
            return false;
        }
        if self.cfg.ignore.iter().any(|i| i == id) {
            return false;
        }
        let valid = !id.is_empty()
            && id.len() <= 100
            && id != "."
            && id != ".."
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if !valid {
            warn!(
                repo = id,
                "identifier is not a valid GitHub repo name, skipping"
            );
        }
        valid
    }

    fn wanted_ids(&self) -> Vec<String> {
        let ids = self.store.lock().unwrap().repo_ids();
        ids.into_iter().filter(|id| self.wanted(id)).collect()
    }

    async fn add_relays(&self) {
        let discovered = self.store.lock().unwrap().relays();
        let mut added = false;
        for url in self.cfg.relays.iter().chain(&discovered) {
            match self.client.add_relay(url.as_str()).await {
                Ok(new) => added |= new,
                Err(e) => warn!(%url, "bad relay: {e}"),
            }
        }
        if added {
            self.client.connect().and_wait(Duration::from_secs(5)).await;
        }
    }

    async fn fetch(&self, filters: Vec<Filter>) {
        match self
            .client
            .fetch_events(filters)
            .timeout(FETCH_TIMEOUT)
            .await
        {
            Ok(events) => {
                let mut store = self.store.lock().unwrap();
                for event in events {
                    store.insert(event);
                }
            }
            Err(e) => warn!("fetch failed: {e}"),
        }
    }

    /// Pulls every announcement and state we care about. Runs twice so that
    /// relays and maintainers learned in the first pass are queried too.
    async fn fetch_all(&self) {
        self.fetch(vec![Filter::new().kind(ANNOUNCEMENT).author(self.owner)])
            .await;
        for _ in 0..2 {
            self.add_relays().await;
            let (ids, maintainers) = {
                let store = self.store.lock().unwrap();
                (store.repo_ids(), store.all_maintainers())
            };
            self.fetch(vec![
                Filter::new().kind(ANNOUNCEMENT).author(self.owner),
                Filter::new().kind(ANNOUNCEMENT).identifiers(ids.clone()),
                Filter::new()
                    .kind(STATE)
                    .authors(maintainers)
                    .identifiers(ids),
            ])
            .await;
        }
        info!(
            repos = self.store.lock().unwrap().repo_ids().len(),
            "fetched repo announcements and states"
        );
    }

    async fn run_once(&self) -> Result<()> {
        let mut failed = 0;
        for id in self.wanted_ids() {
            let target = self.store.lock().unwrap().target(&id);
            if let Err(e) = self.mirror.sync(&target).await {
                error!(repo = %id, "{e:#}");
                failed += 1;
            }
        }
        if failed > 0 {
            bail!("{failed} repo(s) failed");
        }
        Ok(())
    }

    async fn run(self: &Arc<Self>) -> Result<()> {
        let mut notifications = self.client.notifications();
        self.resubscribe().await?;
        for id in self.wanted_ids() {
            self.update(&id, true);
        }

        let period = Duration::from_secs(self.cfg.reconcile_interval_secs.max(60));
        let mut reconcile = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        loop {
            tokio::select! {
                n = notifications.next() => match n {
                    Some(ClientNotification::Event { event, .. }) => self.handle(*event).await,
                    Some(_) => {}
                    None => bail!("relay notification stream ended"),
                },
                _ = reconcile.tick() => {
                    info!("periodic reconcile");
                    self.fetch_all().await;
                    if let Err(e) = self.resubscribe().await {
                        warn!("resubscribe failed: {e:#}");
                    }
                    for id in self.wanted_ids() {
                        self.update(&id, true);
                    }
                }
            }
        }
    }

    async fn handle(self: &Arc<Self>, event: Event) {
        let kind = event.kind;
        let author = event.pubkey;
        let Some(id) = self.store.lock().unwrap().insert(event) else {
            return;
        };
        if kind == ANNOUNCEMENT {
            // May have added a repo, maintainers or relays.
            if let Err(e) = self.resubscribe().await {
                warn!("resubscribe failed: {e:#}");
            }
        }
        if self.store.lock().unwrap().is_repo(&id) && self.wanted(&id) {
            info!(repo = %id, kind = kind.as_u16(), author = %author.to_hex(), "new event");
            self.update(&id, false);
        }
    }

    /// Re-issues the live subscription when the set of repos, maintainers or
    /// relays has changed. Reusing the id replaces the old REQ on each relay.
    async fn resubscribe(&self) -> Result<()> {
        self.add_relays().await;
        let key = {
            let store = self.store.lock().unwrap();
            (store.repo_ids(), store.all_maintainers(), store.relays())
        };
        if self.subscribed.lock().unwrap().as_ref() == Some(&key) {
            return Ok(());
        }
        let (ids, maintainers, _) = key.clone();
        let since = Timestamp::now() - Duration::from_secs(600);
        let filters = vec![
            Filter::new()
                .kind(ANNOUNCEMENT)
                .author(self.owner)
                .since(since),
            Filter::new()
                .kind(ANNOUNCEMENT)
                .identifiers(ids.clone())
                .since(since),
            Filter::new()
                .kind(STATE)
                .authors(maintainers)
                .identifiers(ids)
                .since(since),
        ];
        self.client
            .subscribe(filters)
            .with_id(SubscriptionId::new("ngit-mirror"))
            .await?;
        *self.subscribed.lock().unwrap() = Some(key);
        Ok(())
    }

    /// Hands the repo's current target to its worker, starting one if needed.
    /// Unless `force`, an unchanged target does not wake the worker.
    fn update(&self, id: &str, force: bool) {
        let target = self.store.lock().unwrap().target(id);
        let mut workers = self.workers.lock().unwrap();
        if let Some(tx) = workers.get(id) {
            if force {
                tx.send_replace(Some(target));
            } else {
                tx.send_if_modified(|cur| {
                    let changed = cur.as_ref() != Some(&target);
                    if changed {
                        *cur = Some(target);
                    }
                    changed
                });
            }
            return;
        }
        let (tx, mut rx) = watch::channel(Some(target));
        rx.mark_changed();
        tokio::spawn(worker(self.mirror.clone(), rx));
        workers.insert(id.to_string(), tx);
    }
}

/// Syncs a repo every time its target changes. Failures (typically a state
/// event that arrived before its commits reached the git servers) are
/// retried with backoff, restarting immediately if a newer target arrives.
async fn worker(mirror: Arc<Mirror>, mut rx: watch::Receiver<Option<Target>>) {
    while rx.changed().await.is_ok() {
        let mut attempt = 0;
        loop {
            let Some(target) = rx.borrow_and_update().clone() else {
                break;
            };
            let Err(e) = mirror.sync(&target).await else {
                break;
            };
            attempt += 1;
            if attempt >= MAX_ATTEMPTS {
                error!(repo = %target.id, "giving up until the next change or reconcile: {e:#}");
                break;
            }
            let delay = Duration::from_secs(15 << (attempt - 1).min(5));
            warn!(repo = %target.id, attempt, "sync failed, retrying in {delay:?}: {e:#}");
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                r = rx.changed() => if r.is_err() { return },
            }
        }
    }
}
