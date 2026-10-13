//! In-memory view of NIP-34 repo announcements (30617), repo states (30618)
//! and the owner's deletion requests (5), and the logic that turns them into
//! what a GitHub mirror should look like.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use nostr_sdk::prelude::*;

pub const ANNOUNCEMENT: Kind = Kind::GitRepoAnnouncement;
pub const STATE: Kind = Kind::RepoState;
pub const DELETION: Kind = Kind::EventDeletion;

/// Everything a worker needs to bring one GitHub repo up to date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub id: String,
    /// The owner deleted the announcement, so the GitHub repo should go too.
    pub deleted: bool,
    pub description: Option<String>,
    pub web: Option<String>,
    /// First relay in the owner's announcement, for gitworkshop links.
    pub relay: Option<String>,
    pub clone_urls: Vec<String>,
    pub state: Option<State>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub event_id: EventId,
    pub author: PublicKey,
    /// `refs/heads/*` and `refs/tags/*` only, mapped to object ids.
    pub refs: BTreeMap<String, String>,
    /// Branch name HEAD points at, if it is one of `refs`.
    pub head: Option<String>,
}

pub struct Store {
    owner: PublicKey,
    announcements: HashMap<(PublicKey, String), Event>,
    states: HashMap<(PublicKey, String), Event>,
    /// Latest deletion request time for each of the owner's repo ids.
    deletions: HashMap<String, Timestamp>,
    /// Event ids the owner asked to delete by `e` tag.
    deleted_events: HashSet<EventId>,
}

impl Store {
    pub fn new(owner: PublicKey) -> Self {
        Self {
            owner,
            announcements: HashMap::new(),
            states: HashMap::new(),
            deletions: HashMap::new(),
            deleted_events: HashSet::new(),
        }
    }

    /// Keeps the event if it is newer than what we have for its author and
    /// identifier, or applies it if it is a deletion request, and returns the
    /// repo identifiers it may have changed.
    pub fn insert(&mut self, event: Event) -> Vec<String> {
        if event.kind == DELETION {
            return self.delete(&event);
        }
        if event.kind == ANNOUNCEMENT && self.is_deleted(&event) {
            self.mark_deleted(&event);
            return Vec::new();
        }
        let map = if event.kind == ANNOUNCEMENT {
            &mut self.announcements
        } else if event.kind == STATE {
            &mut self.states
        } else {
            return Vec::new();
        };
        let Some(id) = event.tags.identifier() else {
            return Vec::new();
        };
        let key = (event.pubkey, id.clone());
        if map
            .get(&key)
            .is_some_and(|old| old.created_at >= event.created_at)
        {
            return Vec::new();
        }
        map.insert(key, event);
        vec![id]
    }

    /// Applies one of the owner's deletion requests (NIP-09) to their repo
    /// announcements. Deletion requests from anyone else are ignored.
    fn delete(&mut self, event: &Event) -> Vec<String> {
        if event.pubkey != self.owner {
            return Vec::new();
        }
        let prefix = format!("{}:{}:", ANNOUNCEMENT.as_u16(), self.owner.to_hex());
        let mut ids = Vec::new();
        for tag in event.tags.iter() {
            match tag.as_slice() {
                [name, value, ..] if name == "a" => {
                    if let Some(id) = value.strip_prefix(&prefix) {
                        self.mark_deleted_at(id, event.created_at);
                        ids.push(id.to_string());
                    }
                }
                [name, value, ..] if name == "e" => {
                    if let Ok(id) = EventId::from_hex(value) {
                        self.deleted_events.insert(id);
                    }
                }
                _ => {}
            }
        }
        let gone: Vec<_> = self
            .announcements
            .iter()
            .filter(|(_, e)| self.is_deleted(e))
            .map(|(key, _)| key.clone())
            .collect();
        for key in gone {
            if let Some(event) = self.announcements.remove(&key) {
                self.mark_deleted(&event);
            }
            ids.push(key.1);
        }
        ids
    }

    /// Records a deleted announcement under its identifier, so that one
    /// deleted only by event id still takes its GitHub repo with it.
    fn mark_deleted(&mut self, event: &Event) {
        if let Some(id) = event.tags.identifier() {
            self.mark_deleted_at(&id, event.created_at);
        }
    }

    fn mark_deleted_at(&mut self, id: &str, at: Timestamp) {
        let latest = self.deletions.entry(id.to_string()).or_insert(at);
        *latest = (*latest).max(at);
    }

    /// Whether an owner announcement is covered by one of their deletion
    /// requests. A newer announcement brings the repo back.
    fn is_deleted(&self, event: &Event) -> bool {
        if event.pubkey != self.owner {
            return false;
        }
        self.deleted_events.contains(&event.id)
            || event.tags.identifier().is_some_and(|id| {
                self.deletions
                    .get(&id)
                    .is_some_and(|at| event.created_at <= *at)
            })
    }

    /// Repos the owner deleted and has not announced again.
    pub fn deleted_ids(&self) -> BTreeSet<String> {
        self.deletions
            .keys()
            .filter(|id| !self.is_repo(id))
            .cloned()
            .collect()
    }

    pub fn repo_ids(&self) -> BTreeSet<String> {
        self.announcements
            .keys()
            .filter(|(pk, _)| *pk == self.owner)
            .map(|(_, id)| id.clone())
            .collect()
    }

    pub fn is_repo(&self, id: &str) -> bool {
        self.announcements
            .contains_key(&(self.owner, id.to_string()))
    }

    /// The owner plus everyone they list as a maintainer, followed
    /// transitively through the maintainers' own announcements, as ngit does.
    pub fn maintainers(&self, id: &str) -> BTreeSet<PublicKey> {
        let mut seen = BTreeSet::from([self.owner]);
        let mut queue = VecDeque::from([self.owner]);
        while let Some(pk) = queue.pop_front() {
            let Some(event) = self.announcements.get(&(pk, id.to_string())) else {
                continue;
            };
            for value in tag_values(event, "maintainers") {
                if let Ok(m) = PublicKey::parse(value)
                    && seen.insert(m)
                {
                    queue.push_back(m);
                }
            }
        }
        seen
    }

    pub fn all_maintainers(&self) -> BTreeSet<PublicKey> {
        self.repo_ids()
            .iter()
            .flat_map(|id| self.maintainers(id))
            .collect()
    }

    pub fn relays(&self) -> BTreeSet<String> {
        let mut relays = BTreeSet::new();
        for id in self.repo_ids() {
            for pk in self.maintainers(&id) {
                if let Some(event) = self.announcements.get(&(pk, id.clone())) {
                    relays.extend(tag_values(event, "relays").map(String::from));
                }
            }
        }
        relays
    }

    pub fn target(&self, id: &str) -> Target {
        let maintainers = self.maintainers(id);
        let owner_ann = self.announcements.get(&(self.owner, id.to_string()));

        // Owner's clone URLs first, then any extra ones from co-maintainers.
        let mut clone_urls = Vec::new();
        let anns = std::iter::once(self.owner)
            .chain(maintainers.iter().copied().filter(|pk| *pk != self.owner))
            .filter_map(|pk| self.announcements.get(&(pk, id.to_string())));
        for event in anns {
            for url in tag_values(event, "clone") {
                let fetchable = url.starts_with("https://") || url.starts_with("http://");
                if fetchable && !clone_urls.iter().any(|u| u == url) {
                    clone_urls.push(url.to_string());
                }
            }
        }

        let state = maintainers
            .iter()
            .filter_map(|pk| self.states.get(&(*pk, id.to_string())))
            .max_by_key(|e| e.created_at)
            .map(parse_state);

        Target {
            id: id.to_string(),
            deleted: owner_ann.is_none() && self.deletions.contains_key(id),
            description: owner_ann
                .and_then(|e| tag_values(e, "description").next())
                .map(String::from),
            web: owner_ann
                .and_then(|e| tag_values(e, "web").next())
                .map(String::from),
            relay: owner_ann
                .and_then(|e| tag_values(e, "relays").next())
                .map(String::from),
            clone_urls,
            state,
        }
    }
}

/// Values of every tag named `name`, flattened (NIP-34 lets `clone`,
/// `relays` and `maintainers` carry several values in one tag).
fn tag_values<'a>(event: &'a Event, name: &'a str) -> impl Iterator<Item = &'a str> {
    event
        .tags
        .iter()
        .map(|t| t.as_slice())
        .filter(move |s| s.first().is_some_and(|n| n == name))
        .flat_map(|s| s[1..].iter().map(String::as_str))
        .filter(|v| !v.is_empty())
}

fn parse_state(event: &Event) -> State {
    let mut refs = BTreeMap::new();
    let mut head = None;
    for tag in event.tags.iter() {
        let [name, value, ..] = tag.as_slice() else {
            continue;
        };
        if name == "HEAD" {
            head = value.strip_prefix("ref: refs/heads/").map(String::from);
        } else if (name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
            && !name.ends_with("^{}")
            && is_object_id(value)
        {
            refs.insert(name.clone(), value.clone());
        }
    }
    if head
        .as_ref()
        .is_some_and(|b| !refs.contains_key(&format!("refs/heads/{b}")))
    {
        head = None;
    }
    State {
        event_id: event.id,
        author: event.pubkey,
        refs,
        head,
    }
}

fn is_object_id(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}
