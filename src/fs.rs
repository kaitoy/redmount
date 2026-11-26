//! OS-neutral filesystem core: `\<tracker>\<id subject>\{issue.md, comment_<journal>.md, attachments\*}`
//! and `\_wiki\<page title>\{page.md, attachments\*}`.
//! Paths always use `\`; `win.rs` (WinFsp) and `fuse.rs` (FUSE) adapt it to the OS.
use crate::markdown;
use crate::redmine::{Api, Attachment, Issue, Journal, Meta, WikiIndexEntry, WikiPage};
use anyhow::{Context, anyhow, bail};
use reqwest::Method;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(30);
/// Listings are refreshed by `updated_on` deltas; a full refetch every FULL also drops deleted / moved-out issues.
// ponytail: deletions and moves to another project show up only after FULL; shorten it if that bites
const FULL: Duration = Duration::from_secs(600);
/// Folder in each tracker folder listing its closed issues (the tracker folder itself lists open ones).
const CLOSED: &str = "_closed";
/// Top-level folder listing project wiki pages (when the wiki module is enabled).
const WIKI: &str = "_wiki";
/// 1601-01-01 -> 1970-01-01 in 100ns (times are FILETIME units)
pub const EPOCH: u64 = 116_444_736_000_000_000;

/// Errors the adapters map to NTSTATUS / errno.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum E {
    NotFound,
    PathNotFound,
    Denied,
    Exists,
    NotDir,
    NotEmpty,
    Io,
}

pub type Res<T> = Result<T, E>;

fn nf() -> E {
    E::NotFound
}
fn io(e: anyhow::Error) -> E {
    eprintln!("error: {e:#}");
    E::Io
}

/// File attributes; times in FILETIME units.
#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub ino: u64,
    pub dir: bool,
    pub size: u64,
    pub created: u64,
    pub modified: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum Node {
    Root,
    Tracker(u64),
    /// `\<tracker>\_closed`
    Closed(u64),
    IssueDir(u64),
    /// Folder made in a tracker folder; becomes an issue when its issue.md is saved.
    Pending(u64),
    IssueMd(u64),
    Comment(u64, u64),
    AttachDir(u64),
    Attachment(u64, u64),
    /// `\_wiki`
    WikiRoot,
    WikiDir(String),
    /// Folder made in `_wiki`; becomes a wiki page when its page.md is saved.
    WikiPending,
    WikiMd(String),
    WikiAttachDir(String),
    WikiAttachment(String, u64),
    /// Only exists in memory (new / scratch / error files).
    Local,
}

impl Node {
    #[cfg(windows)]
    fn is_dir(&self) -> bool {
        matches!(
            self,
            Node::Root
                | Node::Tracker(_)
                | Node::Closed(_)
                | Node::IssueDir(_)
                | Node::Pending(_)
                | Node::AttachDir(_)
                | Node::WikiRoot
                | Node::WikiDir(_)
                | Node::WikiPending
                | Node::WikiAttachDir(_)
        )
    }
}

/// In-memory file: scratch/new files, and edit buffers of issue.md / comments.
struct Entry {
    name: String,
    dir: bool,
    data: Vec<u8>,
    /// Written but not (successfully) sent to Redmine.
    dirty: bool,
    opens: u32,
    /// Dropped on last close once clean (server copy is the truth).
    transient: bool,
    time: u64,
}

impl Entry {
    fn new(name: &str, dir: bool, data: Vec<u8>, transient: bool) -> Self {
        Entry {
            name: name.into(),
            dir,
            data,
            dirty: false,
            opens: 0,
            transient,
            time: now(),
        }
    }
}

/// Cached issues of one tracker folder (open) or its `_closed` folder.
struct List {
    /// Last full fetch or delta; `None` = check for changes on next use.
    checked: Option<Instant>,
    full: Instant,
    /// Highest `updated_on` seen; the next delta asks for `updated_on >= cursor`.
    cursor: String,
    issues: BTreeMap<u64, Issue>,
}

impl List {
    /// Applies a project-wide delta: changed issues go in or out depending on tracker / status.
    /// Returns the ones that changed this list; a delta always repeats the issue at the cursor.
    fn merge(
        &mut self,
        changed: Vec<Issue>,
        tracker: u64,
        keep: impl Fn(&Issue) -> bool,
    ) -> Vec<u64> {
        let mut ids = Vec::new();
        for i in changed {
            if i.updated_on > self.cursor {
                self.cursor = i.updated_on.clone();
            }
            if i.tracker.id == tracker && keep(&i) {
                let (id, updated) = (i.id, i.updated_on.clone());
                if self
                    .issues
                    .insert(id, i)
                    .is_none_or(|old| old.updated_on != updated)
                {
                    ids.push(id);
                }
            } else if self.issues.remove(&i.id).is_some() {
                ids.push(i.id);
            }
        }
        ids
    }
}

/// Cached wiki page list (`GET /projects/:id/wiki/index.json`).
struct WikiList {
    checked: Option<Instant>,
    pages: Vec<WikiIndexEntry>,
}

/// `local` keys: `#<id>\<lowercased rest>` under issues, `wiki:<title>\<lowercased rest>` under wiki pages,
/// `\<tracker>\<pending>\...` and `\_wiki\<pending>\...` for pending folders.
#[derive(Default)]
struct State {
    issues: HashMap<u64, (Instant, Option<Issue>)>,
    /// (tracker id, closed)
    lists: HashMap<(u64, bool), List>,
    wiki_list: Option<WikiList>,
    /// lowercased title -> cached page
    wiki_pages: HashMap<String, (Instant, Option<WikiPage>)>,
    /// pending wiki folder key -> actual Redmine title created from it
    wiki_aliases: HashMap<String, String>,
    // ponytail: attachment bodies cached until unmount
    blobs: HashMap<u64, Vec<u8>>,
    local: BTreeMap<String, Entry>,
    /// pending folder key -> issue created from it
    aliases: HashMap<String, u64>,
}

enum Done {
    Updated(u64),
    /// The local file became server data (new note / upload) and goes away.
    Consumed(u64),
    Created(u64, String),
    WikiUpdated(String),
    WikiConsumed(String),
    WikiCreated(String, String),
    Nothing,
}

pub struct RedmineFs {
    api: Api,
    meta: Meta,
    started: u64,
    /// `--filter` query string, put before ours so our `tracker_id` / `status_id` win
    filter: String,
    // ponytail: one global lock, held across HTTP calls; per-path locks if it gets sluggish
    st: Mutex<State>,
}

impl RedmineFs {
    pub fn new(api: Api, meta: Meta, filter: String) -> Self {
        Self {
            api,
            meta,
            started: now(),
            filter,
            st: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn issue(&self, st: &mut State, id: u64) -> Res<Option<Issue>> {
        if let Some((t, i)) = st.issues.get(&id)
            && t.elapsed() < TTL
        {
            return Ok(i.clone());
        }
        let i = self.api.issue(id).map_err(io)?;
        match &i {
            Some(_) => log!("fetched #{id}"),
            None => log!("#{id} not found"),
        }
        st.issues.insert(id, (Instant::now(), i.clone()));
        Ok(i)
    }

    fn tracker_name(&self, id: u64) -> &str {
        let t = self.meta.trackers.iter().find(|t| t.id == id);
        t.map_or("?", |t| t.name.as_str())
    }

    fn must_issue(&self, st: &mut State, id: u64) -> Res<Issue> {
        self.issue(st, id)?.ok_or_else(nf)
    }

    fn list(&self, st: &mut State, tracker: u64, closed: bool) -> Res<Vec<Issue>> {
        let key = (tracker, closed);
        let pid = self.meta.project_id;
        let status = if closed { "closed" } else { "open" };
        match st.lists.get_mut(&key) {
            Some(l) if l.checked.is_some_and(|t| t.elapsed() < TTL) => {}
            // the cursor is the newest updated_on in this list, so nothing edited after the fetch is missed
            // ponytail: a filtered delta can't report issues that stopped matching `--filter`; they linger
            // until the next full fetch (FULL). Diff against an unfiltered delta if that matters.
            Some(l) if l.full.elapsed() < FULL && !l.cursor.is_empty() => {
                let q = format!("{}&status_id=*&updated_on=%3E%3D{}", self.filter, l.cursor);
                let changed = self.api.issues(pid, &q).map_err(io)?;
                let ids = l.merge(changed, tracker, |i| {
                    self.meta.closed_statuses.contains(&i.status.id) == closed
                });
                if !ids.is_empty() {
                    let list: Vec<_> = ids.iter().map(|i| format!("#{i}")).collect();
                    log!(
                        "{} ({status}): {} changed: {}",
                        self.tracker_name(tracker),
                        ids.len(),
                        list.join(", ")
                    );
                }
                l.checked = Some(Instant::now());
            }
            _ => {
                let v = self
                    .api
                    .issues(
                        pid,
                        &format!("{}&tracker_id={tracker}&status_id={status}", self.filter),
                    )
                    .map_err(io)?;
                log!(
                    "fetched {} {status} issues of {}",
                    v.len(),
                    self.tracker_name(tracker)
                );
                let now = Instant::now();
                let mut l = List {
                    checked: Some(now),
                    full: now,
                    cursor: String::new(),
                    issues: BTreeMap::new(),
                };
                l.merge(v, tracker, |_| true);
                st.lists.insert(key, l);
            }
        }
        Ok(st.lists[&key].issues.values().cloned().collect())
    }

    /// After a write: forget the issue and make listings fetch a delta next time.
    fn invalidate(&self, st: &mut State, id: u64) {
        st.issues.remove(&id);
        for l in st.lists.values_mut() {
            l.checked = None;
        }
    }

    fn blob(&self, st: &mut State, id: u64, att: u64) -> Res<()> {
        if !st.blobs.contains_key(&att) {
            let i = self.must_issue(st, id)?;
            let a = i.attachments.iter().find(|a| a.id == att).ok_or_else(nf)?;
            let data = self.api.download(&a.content_url).map_err(io)?;
            log!(
                "downloaded {:?} ({} bytes) from #{id}",
                a.filename,
                data.len()
            );
            st.blobs.insert(att, data);
        }
        Ok(())
    }

    fn wiki_list(&self, st: &mut State) -> Res<Vec<WikiIndexEntry>> {
        let fresh = st
            .wiki_list
            .as_ref()
            .is_some_and(|l| l.checked.is_some_and(|t| t.elapsed() < TTL));
        if !fresh {
            let pages = self.api.wiki_index(self.meta.project_id).map_err(io)?;
            log!("fetched {} wiki pages", pages.len());
            st.wiki_list = Some(WikiList {
                checked: Some(Instant::now()),
                pages,
            });
        }
        Ok(st
            .wiki_list
            .as_ref()
            .map_or_else(Vec::new, |l| l.pages.clone()))
    }

    fn wiki_page(&self, st: &mut State, title: &str) -> Res<Option<WikiPage>> {
        let key = title.to_lowercase();
        if let Some((t, p)) = st.wiki_pages.get(&key)
            && t.elapsed() < TTL
        {
            return Ok(p.clone());
        }
        let p = self
            .api
            .wiki_page(self.meta.project_id, title)
            .map_err(io)?;
        match &p {
            Some(_) => log!("fetched wiki/{title}"),
            None => log!("wiki/{title} not found"),
        }
        st.wiki_pages.insert(key, (Instant::now(), p.clone()));
        Ok(p)
    }

    fn must_wiki_page(&self, st: &mut State, title: &str) -> Res<WikiPage> {
        self.wiki_page(st, title)?.ok_or_else(nf)
    }

    fn invalidate_wiki(&self, st: &mut State, title: &str) {
        st.wiki_pages.remove(&title.to_lowercase());
        if let Some(l) = st.wiki_list.as_mut() {
            l.checked = None;
        }
    }

    fn wiki_blob(&self, st: &mut State, title: &str, att: u64) -> Res<()> {
        if !st.blobs.contains_key(&att) {
            let p = self.must_wiki_page(st, title)?;
            let a = p.attachments.iter().find(|a| a.id == att).ok_or_else(nf)?;
            let data = self.api.download(&a.content_url).map_err(io)?;
            log!(
                "downloaded {:?} ({} bytes) from wiki/{title}",
                a.filename,
                data.len()
            );
            st.blobs.insert(att, data);
        }
        Ok(())
    }

    /// Path -> node + overlay key. `None` = doesn't exist.
    fn resolve(&self, st: &mut State, path: &str) -> Res<Option<(Node, String)>> {
        let mut c: Vec<String> = path
            .split('\\')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase())
            .collect();
        let Some(first) = c.first() else {
            return Ok(Some((Node::Root, String::new())));
        };
        let local =
            |st: &State, key: String| st.local.contains_key(&key).then_some((Node::Local, key));
        if *first == WIKI {
            if !self.meta.wiki_enabled {
                return Ok(None);
            }
            let rkey = format!("\\{WIKI}");
            if c.len() == 1 {
                return Ok(Some((Node::WikiRoot, rkey)));
            }
            let pkey = format!("{rkey}\\{}", c[1]);
            if st.local.contains_key(&pkey) {
                if c.len() == 2 {
                    return Ok(Some((Node::WikiPending, pkey)));
                }
                return Ok(local(st, format!("{pkey}\\{}", c[2..].join("\\"))));
            }
            let title = match st.wiki_aliases.get(&pkey).cloned() {
                Some(t) => t,
                None => {
                    // ponytail: assumes sanitized titles are unique (case-insensitively) within a project
                    let Some(entry) = self
                        .wiki_list(st)?
                        .into_iter()
                        .find(|p| sanitize(&p.title, 100).to_lowercase() == c[1])
                    else {
                        return Ok(None);
                    };
                    entry.title
                }
            };
            let wkey = format!("wiki:{}", title.to_lowercase());
            if c.len() == 2 {
                return Ok(Some((Node::WikiDir(title), wkey)));
            }
            let Some(page) = self.wiki_page(st, &title)? else {
                return Ok(None);
            };
            let key = format!("{wkey}\\{}", c[2..].join("\\"));
            let node = match (c.len(), c[2].as_str()) {
                (3, "page.md") => Some(Node::WikiMd(title)),
                (3, "attachments") => Some(Node::WikiAttachDir(title)),
                (4, "attachments") => att_names(&page.attachments)
                    .into_iter()
                    .find(|(n, _)| n.to_lowercase() == c[3])
                    .map(|(_, a)| Node::WikiAttachment(title, a.id)),
                _ => None,
            };
            return Ok(match node {
                Some(n) => Some((n, key)),
                None => local(st, key),
            });
        }
        let Some(t) = self
            .meta
            .trackers
            .iter()
            .find(|t| sanitize(&t.name, 100).to_lowercase() == *first)
        else {
            return Ok(None);
        };
        let tkey = format!("\\{first}");
        if c.len() == 1 {
            return Ok(Some((Node::Tracker(t.id), tkey)));
        }
        // `\<tracker>\_closed\<issue>\...` is the same issue as `\<tracker>\<issue>\...`; no pending folders there
        let closed = c[1] == CLOSED;
        if closed {
            if c.len() == 2 {
                return Ok(Some((Node::Closed(t.id), format!("{tkey}\\{CLOSED}"))));
            }
            c.remove(1);
        }
        let pkey = format!("{tkey}\\{}", c[1]);
        if !closed && st.local.contains_key(&pkey) {
            if c.len() == 2 {
                return Ok(Some((Node::Pending(t.id), pkey)));
            }
            return Ok(local(st, format!("{pkey}\\{}", c[2..].join("\\"))));
        }
        let alias = if closed {
            None
        } else {
            st.aliases.get(&pkey).copied()
        };
        let Some(id) = alias.or_else(|| leading_id(&c[1])) else {
            return Ok(None);
        };
        let Some(issue) = self.issue(st, id)? else {
            return Ok(None);
        };
        if c.len() == 2 {
            return Ok(Some((Node::IssueDir(id), format!("#{id}"))));
        }
        let key = format!("#{id}\\{}", c[2..].join("\\"));
        let node = match (c.len(), c[2].as_str()) {
            (3, "issue.md") => Some(Node::IssueMd(id)),
            (3, "attachments") => Some(Node::AttachDir(id)),
            (3, name) => comment_id(name)
                .filter(|j| issue.journals.iter().any(|x| x.id == *j && has_notes(x)))
                .map(|j| Node::Comment(id, j)),
            (4, "attachments") => att_names(&issue.attachments)
                .into_iter()
                .find(|(n, _)| n.to_lowercase() == c[3])
                .map(|(_, a)| Node::Attachment(id, a.id)),
            _ => None,
        };
        Ok(match node {
            Some(n) => Some((n, key)),
            None => local(st, key),
        })
    }

    fn must_resolve(&self, st: &mut State, path: &str) -> Res<(Node, String)> {
        self.resolve(st, path)?.ok_or_else(nf)
    }

    fn render(&self, st: &mut State, node: &Node) -> Res<Vec<u8>> {
        Ok(match node {
            Node::IssueMd(id) => {
                markdown::render_issue(&self.must_issue(st, *id)?, &self.meta).into_bytes()
            }
            Node::Comment(id, j) => {
                let i = self.must_issue(st, *id)?;
                markdown::render_comment(i.journals.iter().find(|x| x.id == *j).ok_or_else(nf)?)
                    .into_bytes()
            }
            Node::WikiMd(title) => {
                markdown::render_wiki(&self.must_wiki_page(st, title)?).into_bytes()
            }
            _ => Vec::new(),
        })
    }

    fn info(&self, st: &mut State, node: &Node, key: &str) -> Res<Stat> {
        let ino = file_id(key);
        if let Some(e) = st.local.get(key) {
            return Ok(finfo(ino, e.dir, e.data.len() as u64, e.time, e.time));
        }
        Ok(match node {
            Node::Root | Node::Tracker(_) | Node::Closed(_) | Node::WikiRoot => {
                finfo(ino, true, 0, self.started, self.started)
            }
            Node::IssueDir(id) | Node::AttachDir(id) => {
                let i = self.must_issue(st, *id)?;
                finfo(ino, true, 0, ft(&i.created_on), ft(&i.updated_on))
            }
            Node::IssueMd(id) => {
                let len = self.render(st, node)?.len() as u64;
                let i = self.must_issue(st, *id)?;
                finfo(ino, false, len, ft(&i.created_on), ft(&i.updated_on))
            }
            Node::Comment(id, j) => {
                let len = self.render(st, node)?.len() as u64;
                let i = self.must_issue(st, *id)?;
                let t = i
                    .journals
                    .iter()
                    .find(|x| x.id == *j)
                    .map_or(EPOCH, |x| ft(&x.created_on));
                finfo(ino, false, len, t, t)
            }
            Node::Attachment(id, att) => {
                let i = self.must_issue(st, *id)?;
                let a = i.attachments.iter().find(|a| a.id == *att).ok_or_else(nf)?;
                let t = ft(&a.created_on);
                finfo(ino, false, a.filesize, t, t)
            }
            Node::WikiDir(title) | Node::WikiAttachDir(title) => {
                if let Some(p) = st
                    .wiki_list
                    .as_ref()
                    .and_then(|l| l.pages.iter().find(|p| p.title.eq_ignore_ascii_case(title)))
                {
                    finfo(ino, true, 0, ft(&p.created_on), ft(&p.updated_on))
                } else {
                    let p = self.must_wiki_page(st, title)?;
                    finfo(ino, true, 0, ft(&p.created_on), ft(&p.updated_on))
                }
            }
            Node::WikiMd(title) => {
                let len = self.render(st, node)?.len() as u64;
                let p = self.must_wiki_page(st, title)?;
                finfo(ino, false, len, ft(&p.created_on), ft(&p.updated_on))
            }
            Node::WikiAttachment(title, att) => {
                let p = self.must_wiki_page(st, title)?;
                let a = p.attachments.iter().find(|a| a.id == *att).ok_or_else(nf)?;
                let t = ft(&a.created_on);
                finfo(ino, false, a.filesize, t, t)
            }
            Node::Pending(_) | Node::WikiPending | Node::Local => return Err(nf()),
        })
    }

    fn list_dir(&self, st: &mut State, node: &Node, key: &str) -> Res<Vec<(String, Stat)>> {
        let mut out: Vec<(String, Stat)> = match node {
            Node::Root => {
                let mut v: Vec<_> = self
                    .meta
                    .trackers
                    .iter()
                    .map(|t| {
                        let name = sanitize(&t.name, 100);
                        let ino = file_id(&format!("\\{}", name.to_lowercase()));
                        (name, finfo(ino, true, 0, self.started, self.started))
                    })
                    .collect();
                if self.meta.wiki_enabled {
                    v.push((
                        WIKI.to_string(),
                        finfo(
                            file_id(&format!("\\{WIKI}")),
                            true,
                            0,
                            self.started,
                            self.started,
                        ),
                    ));
                }
                v
            }
            Node::Tracker(tid) | Node::Closed(tid) => {
                let closed = matches!(node, Node::Closed(_));
                let mut v: Vec<_> = self
                    .list(st, *tid, closed)?
                    .iter()
                    .map(|i| {
                        (
                            folder(i),
                            finfo(
                                file_id(&format!("#{}", i.id)),
                                true,
                                0,
                                ft(&i.created_on),
                                ft(&i.updated_on),
                            ),
                        )
                    })
                    .collect();
                if !closed {
                    v.push((
                        CLOSED.to_string(),
                        finfo(
                            file_id(&format!("{key}\\{CLOSED}")),
                            true,
                            0,
                            self.started,
                            self.started,
                        ),
                    ));
                }
                v
            }
            Node::IssueDir(id) => {
                let id = *id;
                let i = self.must_issue(st, id)?;
                let mut v = vec![
                    (
                        "issue.md".to_string(),
                        self.info(st, &Node::IssueMd(id), &format!("#{id}\\issue.md"))?,
                    ),
                    (
                        "attachments".to_string(),
                        self.info(st, &Node::AttachDir(id), &format!("#{id}\\attachments"))?,
                    ),
                ];
                for j in i.journals.iter().filter(|j| has_notes(j)) {
                    let name = format!("comment_{}.md", j.id);
                    v.push((
                        name.clone(),
                        self.info(st, &Node::Comment(id, j.id), &format!("#{id}\\{name}"))?,
                    ));
                }
                v
            }
            Node::AttachDir(id) => {
                let i = self.must_issue(st, *id)?;
                att_names(&i.attachments)
                    .into_iter()
                    .map(|(n, a)| {
                        let t = ft(&a.created_on);
                        let ino = file_id(&format!("{key}\\{}", n.to_lowercase()));
                        (n, finfo(ino, false, a.filesize, t, t))
                    })
                    .collect()
            }
            Node::WikiRoot => self
                .wiki_list(st)?
                .iter()
                .map(|p| {
                    (
                        sanitize(&p.title, 100),
                        finfo(
                            file_id(&format!("wiki:{}", p.title.to_lowercase())),
                            true,
                            0,
                            ft(&p.created_on),
                            ft(&p.updated_on),
                        ),
                    )
                })
                .collect(),
            Node::WikiDir(title) => {
                self.must_wiki_page(st, title)?;
                let wkey = format!("wiki:{}", title.to_lowercase());
                vec![
                    (
                        "page.md".to_string(),
                        self.info(
                            st,
                            &Node::WikiMd(title.clone()),
                            &format!("{wkey}\\page.md"),
                        )?,
                    ),
                    (
                        "attachments".to_string(),
                        self.info(
                            st,
                            &Node::WikiAttachDir(title.clone()),
                            &format!("{wkey}\\attachments"),
                        )?,
                    ),
                ]
            }
            Node::WikiAttachDir(title) => {
                let p = self.must_wiki_page(st, title)?;
                att_names(&p.attachments)
                    .into_iter()
                    .map(|(n, a)| {
                        let t = ft(&a.created_on);
                        let ino = file_id(&format!("{key}\\{}", n.to_lowercase()));
                        (n, finfo(ino, false, a.filesize, t, t))
                    })
                    .collect()
            }
            Node::Pending(_) | Node::WikiPending => Vec::new(),
            _ => return Err(E::NotDir),
        };
        for (name, fi) in local_children(st, key) {
            if !out
                .iter()
                .any(|(n, _)| n.to_lowercase() == name.to_lowercase())
            {
                out.push((name, fi));
            }
        }
        out.sort_by_key(|(n, _)| n.to_lowercase());
        Ok(out)
    }

    /// Writable buffer for a file (creates the edit buffer of issue.md / comments / page.md on demand).
    fn entry<'a>(
        &self,
        st: &'a mut State,
        node: &Node,
        key: &str,
        path: &str,
    ) -> Res<&'a mut Entry> {
        match node {
            Node::IssueMd(_) | Node::Comment(..) | Node::WikiMd(_)
                if !st.local.contains_key(key) =>
            {
                let data = self.render(st, node)?;
                st.local.insert(
                    key.to_string(),
                    Entry::new(split_parent(path).1, false, data, true),
                );
            }
            Node::IssueMd(_) | Node::Comment(..) | Node::WikiMd(_) | Node::Local => {}
            _ => return Err(E::Denied),
        }
        st.local.get_mut(key).filter(|e| !e.dir).ok_or(E::Denied)
    }

    /// Sends a dirty buffer to Redmine. On failure the buffer stays dirty and `<name>.error.txt` explains why.
    /// An empty buffer stays dirty unsent: it is a truncate-then-write save in progress (PowerShell's
    /// Set-Content closes the truncating handle first), and sending it would create an issue named
    /// after the folder or blank out a comment.
    fn commit(&self, st: &mut State, node: &Node, path: &str, key: &str) {
        let Some(e) = st.local.get(key).filter(|e| !e.data.is_empty()) else {
            return;
        };
        let (data, name) = (e.data.clone(), e.name.clone());
        let err_key = format!("{key}.error.txt");
        match self.send(st, node, path, &name, data) {
            Ok(done) => {
                st.local.remove(&err_key);
                if let Some(e) = st.local.get_mut(key) {
                    e.dirty = false;
                }
                match done {
                    Done::Updated(id) => self.invalidate(st, id),
                    Done::Consumed(id) => {
                        self.invalidate(st, id);
                        let idle = st.local.get_mut(key).is_some_and(|e| {
                            e.transient = true;
                            e.opens == 0
                        });
                        if idle {
                            st.local.remove(key);
                        }
                    }
                    Done::Created(id, pkey) => {
                        remove_tree(st, &pkey);
                        st.aliases.insert(pkey, id);
                        self.invalidate(st, id);
                    }
                    Done::WikiUpdated(title) => self.invalidate_wiki(st, &title),
                    Done::WikiConsumed(title) => {
                        self.invalidate_wiki(st, &title);
                        let idle = st.local.get_mut(key).is_some_and(|e| {
                            e.transient = true;
                            e.opens == 0
                        });
                        if idle {
                            st.local.remove(key);
                        }
                    }
                    Done::WikiCreated(title, pkey) => {
                        remove_tree(st, &pkey);
                        st.wiki_aliases.insert(pkey, title.clone());
                        self.invalidate_wiki(st, &title);
                    }
                    Done::Nothing => {}
                }
            }
            Err(e) => {
                eprintln!("{path}: {e:#}");
                let msg = format!(
                    "Failed to send {path}. Your edits remain on this drive (they will be lost on unmount).\r\n\r\n{e:#}\r\n"
                );
                st.local.insert(
                    err_key,
                    Entry::new(&format!("{name}.error.txt"), false, msg.into_bytes(), false),
                );
            }
        }
    }

    fn send(
        &self,
        st: &mut State,
        node: &Node,
        path: &str,
        name: &str,
        data: Vec<u8>,
    ) -> anyhow::Result<Done> {
        match node {
            Node::IssueMd(id) => {
                let id = *id;
                let cur = self.api.issue(id)?.context("Issue not found")?;
                let p = markdown::parse_issue(utf8(&data)?, &self.meta, Some(&cur))?;
                if let Some(u) = p.updated_on.filter(|u| *u != cur.updated_on) {
                    bail!(
                        "Conflict: the issue was updated on the server while you were editing (updated_on: {u} -> {now}).\r\n\
                         To overwrite, change updated_on to {now} and save; to discard your edits, delete issue.md.",
                        now = cur.updated_on
                    );
                }
                let body = p.payload.diff(Some(&markdown::payload_of(&cur)));
                if body.is_empty() {
                    log!("#{id}: no changes");
                } else {
                    let fields = body.keys().cloned().collect::<Vec<_>>().join(", ");
                    self.api.send(
                        Method::PUT,
                        &format!("/issues/{id}.json"),
                        Some(json!({ "issue": body })),
                    )?;
                    log!("updated #{id}: {fields}");
                }
                Ok(Done::Updated(id))
            }
            Node::Comment(id, j) => {
                let (id, j) = (*id, *j);
                let notes = markdown::parse_comment(utf8(&data)?);
                self.api.send(
                    Method::PUT,
                    &format!("/journals/{j}.json"),
                    Some(json!({ "journal": { "notes": notes } })),
                )?;
                log!("updated comment_{j} of #{id}");
                Ok(Done::Updated(id))
            }
            Node::WikiMd(title) => {
                let pid = self.meta.project_id;
                let cur = self
                    .api
                    .wiki_page(pid, title)?
                    .context("Wiki page not found")?;
                let p = markdown::parse_wiki(utf8(&data)?)?;
                if let Some(v) = p.version.filter(|v| *v != cur.version) {
                    bail!(
                        "Conflict: the wiki page was updated on the server while you were editing (version: {v} -> {now}).\r\n\
                         To overwrite, change version to {now} and save; to discard your edits, delete page.md.",
                        now = cur.version
                    );
                }
                let text_changed = p.text != markdown::norm_text(&cur.text);
                let cur_parent = cur.parent.as_ref().map_or("", |x| x.title.as_str());
                let parent_changed = p.parent != cur_parent;
                let has_comments = !p.comments.is_empty();
                if !text_changed && !parent_changed && !has_comments {
                    log!("wiki/{title}: no changes");
                } else {
                    let mut body = serde_json::Map::new();
                    body.insert("text".into(), json!(p.text));
                    body.insert("version".into(), json!(cur.version));
                    if parent_changed {
                        body.insert("parent_title".into(), json!(p.parent));
                    }
                    if has_comments {
                        body.insert("comments".into(), json!(p.comments));
                    }
                    self.api
                        .put_wiki_page(pid, title, json!({ "wiki_page": body }))?;
                    log!("updated wiki/{title}");
                }
                Ok(Done::WikiUpdated(title.clone()))
            }
            Node::Local => {
                let Some((parent, pkey)) = self
                    .resolve(st, split_parent(path).0)
                    .map_err(|_| anyhow!("resolve failed"))?
                else {
                    return Ok(Done::Nothing);
                };
                let lname = name.to_lowercase();
                match parent {
                    Node::Pending(tracker) if lname == "issue.md" => {
                        let mut p = markdown::parse_issue(utf8(&data)?, &self.meta, None)?.payload;
                        if p.fields.get("subject").is_none_or(|s| s.is_empty()) {
                            let folder = st
                                .local
                                .get(&pkey)
                                .map(|e| e.name.clone())
                                .unwrap_or_default();
                            p.fields.insert("subject", folder);
                        }
                        let mut body = p.diff(None);
                        body.insert("project_id".into(), json!(self.meta.project_id));
                        body.insert("tracker_id".into(), json!(tracker));
                        let v = self.api.send(
                            Method::POST,
                            "/issues.json",
                            Some(json!({ "issue": body })),
                        )?;
                        let id = v["issue"]["id"].as_u64().context("No id in response")?;
                        let subject = v["issue"]["subject"].as_str().unwrap_or_default();
                        log!(
                            "created #{id} {subject:?} in {}",
                            self.tracker_name(tracker)
                        );
                        Ok(Done::Created(id, pkey))
                    }
                    Node::WikiPending if lname == "page.md" => {
                        let folder = st
                            .local
                            .get(&pkey)
                            .map(|e| e.name.clone())
                            .unwrap_or_default();
                        let p = markdown::parse_wiki(utf8(&data)?)?;
                        let mut body = serde_json::Map::new();
                        body.insert("text".into(), json!(p.text));
                        if !p.parent.is_empty() {
                            body.insert("parent_title".into(), json!(p.parent));
                        }
                        if !p.comments.is_empty() {
                            body.insert("comments".into(), json!(p.comments));
                        }
                        let v = self.api.put_wiki_page(
                            self.meta.project_id,
                            &folder,
                            json!({ "wiki_page": body }),
                        )?;
                        let actual_title = v["wiki_page"]["title"]
                            .as_str()
                            .map(String::from)
                            .unwrap_or(folder);
                        log!("created wiki/{actual_title}");
                        Ok(Done::WikiCreated(actual_title, pkey))
                    }
                    Node::IssueDir(id) if is_new_comment(name) => {
                        let notes = markdown::parse_comment(utf8(&data)?);
                        if notes.is_empty() {
                            return Ok(Done::Nothing);
                        }
                        self.api.send(
                            Method::PUT,
                            &format!("/issues/{id}.json"),
                            Some(json!({ "issue": { "notes": notes } })),
                        )?;
                        log!("added a comment to #{id}");
                        Ok(Done::Consumed(id))
                    }
                    Node::AttachDir(id) if !data.is_empty() => {
                        let token = self.api.upload(name, data)?;
                        let up = json!({ "issue": { "uploads": [{ "token": token, "filename": name }] } });
                        self.api
                            .send(Method::PUT, &format!("/issues/{id}.json"), Some(up))?;
                        log!("attached {name:?} to #{id}");
                        Ok(Done::Consumed(id))
                    }
                    Node::WikiAttachDir(title) if !data.is_empty() => {
                        let pid = self.meta.project_id;
                        let token = self.api.upload(name, data)?;
                        let cur = self
                            .api
                            .wiki_page(pid, &title)?
                            .context("Wiki page not found")?;
                        let up = json!({
                            "wiki_page": {
                                "text": cur.text,
                                "version": cur.version,
                                "uploads": [{ "token": token, "filename": name }]
                            }
                        });
                        self.api.put_wiki_page(pid, &title, up)?;
                        log!("attached {name:?} to wiki/{title}");
                        Ok(Done::WikiConsumed(title))
                    }
                    _ => Ok(Done::Nothing),
                }
            }
            _ => Ok(Done::Nothing),
        }
    }

    /// Local file / pending folder / discarding unsent edits of issue.md or a comment.
    fn remove(st: &mut State, key: &str) {
        remove_tree(st, key);
        st.local.remove(&format!("{key}.error.txt"));
    }
}

/// Operations the OS adapters call. Each one takes the global lock once.
impl RedmineFs {
    #[cfg(windows)]
    pub fn is_dir(&self, path: &str) -> Res<bool> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        Ok(node.is_dir() || st.local.get(&key).is_some_and(|e| e.dir))
    }

    pub fn stat(&self, path: &str) -> Res<Stat> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        self.info(&mut st, &node, &key)
    }

    /// Counts a handle; issue.md / comments / page.md get a snapshot so the handle sees one consistent text.
    pub fn open(&self, path: &str) -> Res<Stat> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        if matches!(node, Node::IssueMd(_) | Node::Comment(..) | Node::WikiMd(_)) {
            self.entry(&mut st, &node, &key, path)?;
        }
        if let Some(e) = st.local.get_mut(&key) {
            e.opens += 1;
        }
        self.info(&mut st, &node, &key)
    }

    pub fn close(&self, path: &str) {
        let mut st = self.lock();
        let Ok(Some((_, key))) = self.resolve(&mut st, path) else {
            return;
        };
        let idle = st.local.get_mut(&key).is_some_and(|e| {
            e.opens = e.opens.saturating_sub(1);
            e.opens == 0 && e.transient && !e.dirty
        });
        if idle {
            st.local.remove(&key);
        }
    }

    /// New in-memory file or folder, counted as open once (`close` it).
    pub fn create(&self, path: &str, dir: bool) -> Res<Stat> {
        let (parent, name) = split_parent(path);
        let mut st = self.lock();
        let (pnode, pkey) = self.resolve(&mut st, parent)?.ok_or(E::PathNotFound)?;
        match (&pnode, dir) {
            (Node::Tracker(_) | Node::WikiRoot, true)
            | (
                Node::IssueDir(_)
                | Node::Pending(_)
                | Node::AttachDir(_)
                | Node::WikiDir(_)
                | Node::WikiPending
                | Node::WikiAttachDir(_),
                false,
            ) => {}
            _ => return Err(E::Denied),
        }
        // Reject creating any file or folder that already exists (including existing issues,
        // _closed, wiki pages, and local entries) so `mkdir -p` never shadows a real issue folder.
        if self.resolve(&mut st, path)?.is_some() {
            return Err(E::Exists);
        }
        let lname = name.to_lowercase();
        let key = format!("{pkey}\\{lname}");
        let mut e = Entry::new(name, dir, Vec::new(), false);
        e.opens = 1;
        st.local.insert(key.clone(), e);
        if dir {
            let (fname, tpl) = if matches!(pnode, Node::WikiRoot) {
                ("page.md", markdown::wiki_template().into_bytes())
            } else {
                ("issue.md", markdown::template(&self.meta).into_bytes())
            };
            st.local.insert(
                format!("{key}\\{fname}"),
                Entry::new(fname, false, tpl, false),
            );
        }
        self.info(&mut st, &Node::Local, &key)
    }

    /// A handle is done: delete the file (`check_delete` passed) or send its unsent edits.
    pub fn cleanup(&self, path: &str, delete: bool) {
        let mut st = self.lock();
        let Ok(Some((node, key))) = self.resolve(&mut st, path) else {
            return;
        };
        if delete {
            Self::remove(&mut st, &key);
        } else if st.local.get(&key).is_some_and(|e| e.dirty) {
            self.commit(&mut st, &node, path, &key);
        }
    }

    pub fn check_delete(&self, path: &str) -> Res<()> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        match node {
            Node::Local => Ok(()),
            Node::Pending(_) | Node::WikiPending if local_children(&st, &key).is_empty() => Ok(()),
            Node::Pending(_) | Node::WikiPending => Err(E::NotEmpty),
            // only unsent edits can be "deleted" (= discarded)
            Node::IssueMd(_) | Node::Comment(..) | Node::WikiMd(_)
                if st.local.get(&key).is_some_and(|e| e.dirty) =>
            {
                Ok(())
            }
            _ => Err(E::Denied),
        }
    }

    /// `alloc_only`: just an allocation hint; it may shrink but never grow the file.
    pub fn set_size(&self, path: &str, size: u64, alloc_only: bool) -> Res<Stat> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        let e = self.entry(&mut st, &node, &key, path)?;
        let n = size as usize;
        if (!alloc_only || n < e.data.len()) && n != e.data.len() {
            e.data.resize(n, 0);
            e.dirty = true;
        }
        self.info(&mut st, &node, &key)
    }

    /// Sorted entries, without `.` / `..`.
    pub fn read_dir(&self, path: &str) -> Res<Vec<(String, Stat)>> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        self.list_dir(&mut st, &node, &key)
    }

    pub fn rename(&self, from: &str, to: &str, replace_if_exists: bool) -> Res<()> {
        let (to_parent, to_name) = split_parent(to);
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, from)?;
        let (pnode, pkey) = self.resolve(&mut st, to_parent)?.ok_or(E::PathNotFound)?;
        let target = self.resolve(&mut st, to)?;
        let new_key = match (&node, &pnode, &target) {
            // case-only rename
            (Node::Local | Node::Pending(_) | Node::WikiPending, _, Some((_, k))) if *k == key => {
                key.clone()
            }
            (Node::Pending(t), Node::Tracker(pt), None) if t == pt => {
                format!("{pkey}\\{}", to_name.to_lowercase())
            }
            (Node::WikiPending, Node::WikiRoot, None) => {
                format!("{pkey}\\{}", to_name.to_lowercase())
            }
            (
                Node::Local,
                Node::IssueDir(_)
                | Node::Pending(_)
                | Node::AttachDir(_)
                | Node::WikiDir(_)
                | Node::WikiPending
                | Node::WikiAttachDir(_),
                None,
            ) => {
                format!("{pkey}\\{}", to_name.to_lowercase())
            }
            // atomic save: temp file -> issue.md / comment / page.md
            (
                Node::Local,
                _,
                Some((Node::Local | Node::IssueMd(_) | Node::Comment(..) | Node::WikiMd(_), k)),
            ) if replace_if_exists => k.clone(),
            (_, _, Some(_)) => return Err(E::Exists),
            _ => return Err(E::Denied),
        };
        let replaced_opens = if new_key == key {
            0
        } else {
            st.local.remove(&new_key).map_or(0, |e| e.opens)
        };
        let prefix = format!("{key}\\");
        let moved: Vec<String> = st
            .local
            .keys()
            .filter(|k| **k == key || k.starts_with(&prefix))
            .cloned()
            .collect();
        for k in moved {
            let mut e = st.local.remove(&k).expect("key listed above");
            if k == key {
                e.name = to_name.to_string();
                e.opens += replaced_opens;
                e.transient |= matches!(
                    target,
                    Some((Node::IssueMd(_) | Node::Comment(..) | Node::WikiMd(_), _))
                );
            }
            st.local.insert(format!("{new_key}{}", &k[key.len()..]), e);
        }
        // A file landing on a meaningful name (issue.md, comment.md, page.md, attachments\) is sent right away.
        if node == Node::Local
            && let Some(e) = st.local.get_mut(&new_key)
            && !e.data.is_empty()
        {
            e.dirty = true;
            let tnode = target.map_or(Node::Local, |(n, _)| n);
            self.commit(&mut st, &tnode, to, &new_key);
        }
        Ok(())
    }

    /// Bytes copied; 0 at or past the end.
    pub fn read(&self, path: &str, buffer: &mut [u8], offset: u64) -> Res<usize> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        let rendered = match &node {
            _ if st.local.contains_key(&key) => None,
            Node::Attachment(id, att) => {
                self.blob(&mut st, *id, *att)?;
                None
            }
            Node::WikiAttachment(title, att) => {
                self.wiki_blob(&mut st, title, *att)?;
                None
            }
            Node::IssueMd(_) | Node::Comment(..) | Node::WikiMd(_) => {
                Some(self.render(&mut st, &node)?)
            }
            _ => None,
        };
        let data: &[u8] = match (&rendered, st.local.get(&key), &node) {
            (Some(r), _, _) => r.as_slice(),
            (None, Some(e), _) => e.data.as_slice(),
            (None, None, Node::Attachment(_, att) | Node::WikiAttachment(_, att)) => {
                st.blobs[att].as_slice()
            }
            _ => &[],
        };
        let off = (offset as usize).min(data.len());
        let n = buffer.len().min(data.len() - off);
        buffer[..n].copy_from_slice(&data[off..off + n]);
        Ok(n)
    }

    /// `constrained`: never grow the file (Windows paging I/O).
    pub fn write(
        &self,
        path: &str,
        buffer: &[u8],
        offset: u64,
        to_eof: bool,
        constrained: bool,
    ) -> Res<(usize, Stat)> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        let e = self.entry(&mut st, &node, &key, path)?;
        let off = if to_eof {
            e.data.len()
        } else {
            offset as usize
        };
        let len = if constrained {
            buffer.len().min(e.data.len().saturating_sub(off))
        } else {
            buffer.len()
        };
        if len > 0 {
            if off + len > e.data.len() {
                e.data.resize(off + len, 0);
            }
            e.data[off..off + len].copy_from_slice(&buffer[..len]);
            e.dirty = true;
        }
        Ok((len, self.info(&mut st, &node, &key)?))
    }
}

fn local_children(st: &State, key: &str) -> Vec<(String, Stat)> {
    let prefix = format!("{key}\\");
    st.local
        .range(prefix.clone()..)
        .take_while(|(k, _)| k.starts_with(&prefix))
        .filter(|(k, _)| !k[prefix.len()..].contains('\\'))
        .map(|(k, e)| {
            (
                e.name.clone(),
                finfo(file_id(k), e.dir, e.data.len() as u64, e.time, e.time),
            )
        })
        .collect()
}

fn remove_tree(st: &mut State, key: &str) {
    let prefix = format!("{key}\\");
    st.local.retain(|k, _| k != key && !k.starts_with(&prefix));
}

fn split_parent(path: &str) -> (&str, &str) {
    path.rsplit_once('\\').unwrap_or(("", path))
}

fn utf8(d: &[u8]) -> anyhow::Result<&str> {
    std::str::from_utf8(d).map_err(|_| anyhow!("Please save as UTF-8"))
}

fn has_notes(j: &Journal) -> bool {
    j.notes.as_deref().is_some_and(|n| !n.trim().is_empty())
}

fn sanitize(s: &str, max: usize) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_control() || r#"\/:*?"<>|"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(max)
        .collect();
    s.trim_end_matches(['.', ' ']).to_string()
}

fn folder(i: &Issue) -> String {
    format!("{} {}", i.id, sanitize(&i.subject, 60))
        .trim_end()
        .to_string()
}

fn leading_id(s: &str) -> Option<u64> {
    s.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

fn comment_id(name: &str) -> Option<u64> {
    name.strip_prefix("comment_")?
        .strip_suffix(".md")?
        .parse()
        .ok()
}

/// Only this name in an issue folder is posted as a new comment, so copied-in `.md` files aren't.
fn is_new_comment(name: &str) -> bool {
    name.eq_ignore_ascii_case("comment.md")
}

/// Attachment file names; duplicates get an `<id>_` prefix.
fn att_names(atts: &[Attachment]) -> Vec<(String, &Attachment)> {
    atts.iter()
        .map(|a| {
            let dup = atts
                .iter()
                .filter(|b| b.filename.eq_ignore_ascii_case(&a.filename))
                .count()
                > 1;
            (
                if dup {
                    format!("{}_{}", a.id, a.filename)
                } else {
                    a.filename.clone()
                },
                a,
            )
        })
        .collect()
}

/// Stable non-zero 64-bit file ID derived from a normalized overlay key (FNV-1a).
fn file_id(key: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h.max(1)
}

fn finfo(ino: u64, dir: bool, size: u64, created: u64, modified: u64) -> Stat {
    Stat {
        ino,
        dir,
        size,
        created,
        modified,
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64 / 100)
        + EPOCH
}

/// "2026-11-20T09:00:00Z" -> FILETIME (offset suffix ignored; Redmine's API returns UTC).
fn ft(s: &str) -> u64 {
    let n = |a: usize, b: usize| s.get(a..b).and_then(|x| x.parse::<i64>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(se)) =
        (n(0, 4), n(5, 7), n(8, 10), n(11, 13), n(14, 16), n(17, 19))
    else {
        return EPOCH;
    };
    // days_from_civil (Howard Hinnant)
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    (days * 86_400 + h * 3600 + mi * 60 + se) as u64 * 10_000_000 + EPOCH
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redmine::IdName;

    #[test]
    fn names_and_times() {
        assert_eq!(ft("1970-01-01T00:00:00Z"), EPOCH);
        assert_eq!(ft("2000-03-01T00:00:00Z"), 951_868_800 * 10_000_000 + EPOCH);
        assert_eq!(leading_id("123 Login fails"), Some(123));
        assert_eq!(leading_id("New folder"), None);
        assert_eq!(comment_id("comment_45.md"), Some(45));
        assert!(is_new_comment("comment.md") && is_new_comment("Comment.MD"));
        assert!(
            !is_new_comment("memo.md")
                && !is_new_comment("comment_45.md")
                && !is_new_comment("._comment.md")
        );
        assert_eq!(sanitize("a/b: c?. ", 60), "a_b_ c_");
    }

    #[test]
    fn list_delta_merge() {
        let issue = |id: u64, tracker: u64, status: u64, updated: &str| -> Issue {
            serde_json::from_value(json!({
                "id": id, "tracker": {"id": tracker}, "status": {"id": status}, "priority": {"id": 1},
                "author": {"id": 1}, "subject": "s", "created_on": updated, "updated_on": updated
            }))
            .unwrap()
        };
        let open = |i: &Issue| i.status.id != 5;
        let mut l = List {
            checked: None,
            full: Instant::now(),
            cursor: String::new(),
            issues: BTreeMap::new(),
        };
        l.merge(
            vec![
                issue(1, 1, 1, "2026-01-01T00:00:00Z"),
                issue(2, 1, 1, "2026-01-03T00:00:00Z"),
            ],
            1,
            |_| true,
        );
        assert_eq!(l.cursor, "2026-01-03T00:00:00Z");
        // the issue at the cursor comes back unchanged
        assert!(
            l.merge(vec![issue(2, 1, 1, "2026-01-03T00:00:00Z")], 1, open)
                .is_empty()
        );
        // 1 closed, 2 moved to another tracker, 3 new, 4 belongs elsewhere
        let ids = l.merge(
            vec![
                issue(1, 1, 5, "2026-01-04T00:00:00Z"),
                issue(2, 2, 1, "2026-01-05T00:00:00Z"),
                issue(3, 1, 1, "2026-01-04T12:00:00Z"),
                issue(4, 2, 1, "2026-01-04T00:00:00Z"),
            ],
            1,
            open,
        );
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(l.issues.keys().copied().collect::<Vec<_>>(), vec![3]);
        assert_eq!(l.cursor, "2026-01-05T00:00:00Z");
    }

    /// PowerShell's Set-Content truncates in one handle and writes in the next; the empty
    /// intermediate state must not be sent (it would create an issue named after the folder).
    #[test]
    fn empty_buffer_is_not_sent() {
        // nothing listens here, so any send attempt leaves an .error.txt
        let api = Api::new("http://127.0.0.1:9", "k").unwrap();
        let meta = Meta {
            trackers: vec![IdName {
                id: 1,
                name: "Bug".into(),
            }],
            ..Default::default()
        };
        let fs = RedmineFs::new(api, meta, String::new());
        fs.create("\\Bug\\new", true).unwrap();
        fs.close("\\Bug\\new");
        let md = "\\Bug\\new\\issue.md";
        fs.open(md).unwrap();
        fs.set_size(md, 0, true).unwrap();
        fs.cleanup(md, false);
        fs.close(md);
        assert!(fs.stat("\\Bug\\new\\issue.md.error.txt").is_err());
        assert_eq!(fs.stat(md).unwrap().size, 0);
    }

    #[test]
    fn multibyte_names() {
        assert_eq!(leading_id("123 ログイン不具合"), Some(123));
        assert_eq!(leading_id("新しいフォルダー"), None);
        // max counts chars, not bytes
        assert_eq!(sanitize("日本語：テスト?", 5), "日本語：テ");
        assert_eq!(sanitize("件名/です。 ", 60), "件名_です。");
    }

    #[test]
    fn wiki_disabled_hides_folder() {
        let api = Api::new("http://127.0.0.1:9", "k").unwrap();
        let meta = Meta {
            wiki_enabled: false,
            trackers: vec![IdName {
                id: 1,
                name: "Bug".into(),
            }],
            ..Default::default()
        };
        let fs = RedmineFs::new(api, meta, String::new());
        let root: Vec<_> = fs
            .read_dir("")
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(root, vec!["Bug"]);
        assert_eq!(fs.stat("\\_wiki").unwrap_err(), E::NotFound);
    }

    #[test]
    fn wiki_pending_folder_and_empty_buffer_guard() {
        let api = Api::new("http://127.0.0.1:9", "k").unwrap();
        let meta = Meta {
            wiki_enabled: true,
            trackers: vec![IdName {
                id: 1,
                name: "Bug".into(),
            }],
            ..Default::default()
        };
        let fs = RedmineFs::new(api, meta, String::new());
        fs.lock().wiki_list = Some(WikiList {
            checked: Some(Instant::now()),
            pages: vec![WikiIndexEntry {
                title: "Home".into(),
                parent: None,
                version: 1,
                created_on: "2026-01-01T00:00:00Z".into(),
                updated_on: "2026-01-01T00:00:00Z".into(),
            }],
        });
        let root: Vec<_> = fs
            .read_dir("")
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(root, vec!["_wiki", "Bug"]);

        // Cannot create a pending folder with the same name as an existing wiki page
        assert_eq!(fs.create("\\_wiki\\home", true).unwrap_err(), E::Exists);

        // Create pending wiki folder -> page.md template appears inside
        fs.create("\\_wiki\\DraftPage", true).unwrap();
        fs.close("\\_wiki\\DraftPage");
        let md = "\\_wiki\\DraftPage\\page.md";
        let mut buf = vec![0u8; 256];
        let n = fs.read(md, &mut buf, 0).unwrap();
        let text = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(text.contains("parent:"));
        assert!(text.contains("comments:"));

        // Non-empty pending folder cannot be deleted; deleting page.md first lets it be deleted
        assert_eq!(
            fs.check_delete("\\_wiki\\DraftPage").unwrap_err(),
            E::NotEmpty
        );

        // Truncating page.md to empty must not send to Redmine
        fs.open(md).unwrap();
        fs.set_size(md, 0, true).unwrap();
        fs.cleanup(md, false);
        fs.close(md);
        assert!(fs.stat("\\_wiki\\DraftPage\\page.md.error.txt").is_err());

        // Rename pending folder within _wiki (and reject collision with existing page)
        assert_eq!(
            fs.rename("\\_wiki\\DraftPage", "\\_wiki\\Home", false)
                .unwrap_err(),
            E::Exists
        );
        fs.rename("\\_wiki\\DraftPage", "\\_wiki\\RenamedPage", false)
            .unwrap();
        assert!(fs.stat("\\_wiki\\RenamedPage\\page.md").is_ok());

        // Delete page.md then delete now-empty pending folder
        fs.check_delete("\\_wiki\\RenamedPage\\page.md").unwrap();
        fs.cleanup("\\_wiki\\RenamedPage\\page.md", true);
        fs.check_delete("\\_wiki\\RenamedPage").unwrap();
        fs.cleanup("\\_wiki\\RenamedPage", true);
        assert!(fs.stat("\\_wiki\\RenamedPage").is_err());
    }

    /// `mkdir -p` on an existing issue folder must return E::Exists instead of creating a
    /// pending folder that shadows the real issue and breaks `comment.md` writes.
    #[test]
    fn create_dir_does_not_shadow_existing_issue_and_ino_is_distinct() {
        let api = Api::new("http://127.0.0.1:9", "k").unwrap();
        let meta = Meta {
            trackers: vec![IdName {
                id: 1,
                name: "Bug".into(),
            }],
            ..Default::default()
        };
        let fs = RedmineFs::new(api, meta, String::new());
        let issue: Issue = serde_json::from_value(json!({
            "id": 10,
            "tracker": {"id": 1, "name": "Bug"},
            "status": {"id": 1, "name": "New"},
            "priority": {"id": 2, "name": "Normal"},
            "author": {"id": 1, "name": "Admin"},
            "subject": "Existing issue",
            "description": "line1\r\n\r\nline2",
            "created_on": "2026-01-01T00:00:00Z",
            "updated_on": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        fs.lock().issues.insert(10, (Instant::now(), Some(issue)));

        // Creating a directory with an existing issue ID must fail with E::Exists
        assert_eq!(
            fs.create("\\Bug\\10 Existing issue", true).unwrap_err(),
            E::Exists
        );
        assert_eq!(fs.create("\\Bug\\10", true).unwrap_err(), E::Exists);

        // Rendered issue.md normalizes CRLF -> LF so edit size verification stays consistent
        let md = "\\Bug\\10 Existing issue\\issue.md";
        let mut buf = vec![0u8; 1024];
        let n = fs.read(md, &mut buf, 0).unwrap();
        let text = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(!text.contains("\r\n"));
        assert!(text.contains("line1\n\nline2"));

        // Distinct non-zero file IDs across root, tracker, issue dir, issue.md, attachments
        let inos = [
            fs.stat("").unwrap().ino,
            fs.stat("\\Bug").unwrap().ino,
            fs.stat("\\Bug\\10 Existing issue").unwrap().ino,
            fs.stat(md).unwrap().ino,
            fs.stat("\\Bug\\10 Existing issue\\attachments")
                .unwrap()
                .ino,
        ];
        assert!(inos.iter().all(|&x| x != 0));
        let unique: std::collections::HashSet<u64> = inos.iter().copied().collect();
        assert_eq!(unique.len(), inos.len());
    }
}
