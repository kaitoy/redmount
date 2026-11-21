//! OS-neutral filesystem core: `\<tracker>\<id subject>\{issue.md, comment_<journal>.md, attachments\*}`.
//! Paths always use `\`; `win.rs` (WinFsp) and `fuse.rs` (FUSE) adapt it to the OS.
use crate::markdown;
use crate::redmine::{Api, Attachment, Issue, Journal, Meta};
use anyhow::{Context, anyhow, bail};
use reqwest::Method;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(30);
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
    pub dir: bool,
    pub size: u64,
    pub created: u64,
    pub modified: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Node {
    Root,
    Tracker(u64),
    IssueDir(u64),
    /// Folder made in a tracker folder; becomes an issue when its issue.md is saved.
    Pending(u64),
    IssueMd(u64),
    Comment(u64, u64),
    AttachDir(u64),
    Attachment(u64, u64),
    /// Only exists in memory (new / scratch / error files).
    Local,
}

impl Node {
    #[cfg(windows)]
    fn is_dir(self) -> bool {
        matches!(self, Node::Root | Node::Tracker(_) | Node::IssueDir(_) | Node::Pending(_) | Node::AttachDir(_))
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
        Entry { name: name.into(), dir, data, dirty: false, opens: 0, transient, time: now() }
    }
}

/// `local` keys: `#<id>\<lowercased rest>` under issues, `\<tracker>\<pending>\...` for pending folders.
#[derive(Default)]
struct State {
    issues: HashMap<u64, (Instant, Option<Issue>)>,
    lists: HashMap<u64, (Instant, Vec<Issue>)>,
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
    Nothing,
}

pub struct RedmineFs {
    api: Api,
    meta: Meta,
    started: u64,
    // ponytail: one global lock, held across HTTP calls; per-path locks if it gets sluggish
    st: Mutex<State>,
}

impl RedmineFs {
    pub fn new(api: Api, meta: Meta) -> Self {
        Self { api, meta, started: now(), st: Mutex::default() }
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
        st.issues.insert(id, (Instant::now(), i.clone()));
        Ok(i)
    }

    fn must_issue(&self, st: &mut State, id: u64) -> Res<Issue> {
        self.issue(st, id)?.ok_or_else(nf)
    }

    fn list(&self, st: &mut State, tracker: u64) -> Res<Vec<Issue>> {
        if let Some((t, l)) = st.lists.get(&tracker)
            && t.elapsed() < TTL
        {
            return Ok(l.clone());
        }
        let l = self.api.issues(self.meta.project_id, tracker).map_err(io)?;
        st.lists.insert(tracker, (Instant::now(), l.clone()));
        Ok(l)
    }

    fn invalidate(&self, st: &mut State, id: u64) {
        st.issues.remove(&id);
        st.lists.clear();
    }

    fn blob(&self, st: &mut State, id: u64, att: u64) -> Res<()> {
        if !st.blobs.contains_key(&att) {
            let i = self.must_issue(st, id)?;
            let a = i.attachments.iter().find(|a| a.id == att).ok_or_else(nf)?;
            let data = self.api.download(&a.content_url).map_err(io)?;
            st.blobs.insert(att, data);
        }
        Ok(())
    }

    /// Path -> node + overlay key. `None` = doesn't exist.
    fn resolve(&self, st: &mut State, path: &str) -> Res<Option<(Node, String)>> {
        let c: Vec<String> = path.split('\\').filter(|s| !s.is_empty()).map(|s| s.to_lowercase()).collect();
        let Some(first) = c.first() else { return Ok(Some((Node::Root, String::new()))) };
        let Some(t) = self.meta.trackers.iter().find(|t| sanitize(&t.name, 100).to_lowercase() == *first) else {
            return Ok(None);
        };
        let tkey = format!("\\{first}");
        if c.len() == 1 {
            return Ok(Some((Node::Tracker(t.id), tkey)));
        }
        let local = |st: &State, key: String| st.local.contains_key(&key).then_some((Node::Local, key));
        let pkey = format!("{tkey}\\{}", c[1]);
        if st.local.contains_key(&pkey) {
            if c.len() == 2 {
                return Ok(Some((Node::Pending(t.id), pkey)));
            }
            return Ok(local(st, format!("{pkey}\\{}", c[2..].join("\\"))));
        }
        let Some(id) = st.aliases.get(&pkey).copied().or_else(|| leading_id(&c[1])) else { return Ok(None) };
        let Some(issue) = self.issue(st, id)? else { return Ok(None) };
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
            (4, "attachments") => att_names(&issue)
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

    fn render(&self, st: &mut State, node: Node) -> Res<Vec<u8>> {
        Ok(match node {
            Node::IssueMd(id) => markdown::render_issue(&self.must_issue(st, id)?, &self.meta).into_bytes(),
            Node::Comment(id, j) => {
                let i = self.must_issue(st, id)?;
                markdown::render_comment(i.journals.iter().find(|x| x.id == j).ok_or_else(nf)?).into_bytes()
            }
            _ => Vec::new(),
        })
    }

    fn info(&self, st: &mut State, node: Node, key: &str) -> Res<Stat> {
        if let Some(e) = st.local.get(key) {
            return Ok(finfo(e.dir, e.data.len() as u64, e.time, e.time));
        }
        Ok(match node {
            Node::Root | Node::Tracker(_) => finfo(true, 0, self.started, self.started),
            Node::IssueDir(id) | Node::AttachDir(id) => {
                let i = self.must_issue(st, id)?;
                finfo(true, 0, ft(&i.created_on), ft(&i.updated_on))
            }
            Node::IssueMd(id) => {
                let len = self.render(st, node)?.len() as u64;
                let i = self.must_issue(st, id)?;
                finfo(false, len, ft(&i.created_on), ft(&i.updated_on))
            }
            Node::Comment(id, j) => {
                let len = self.render(st, node)?.len() as u64;
                let i = self.must_issue(st, id)?;
                let t = i.journals.iter().find(|x| x.id == j).map_or(EPOCH, |x| ft(&x.created_on));
                finfo(false, len, t, t)
            }
            Node::Attachment(id, att) => {
                let i = self.must_issue(st, id)?;
                let a = i.attachments.iter().find(|a| a.id == att).ok_or_else(nf)?;
                let t = ft(&a.created_on);
                finfo(false, a.filesize, t, t)
            }
            Node::Pending(_) | Node::Local => return Err(nf()),
        })
    }

    fn list_dir(&self, st: &mut State, node: Node, key: &str) -> Res<Vec<(String, Stat)>> {
        let mut out: Vec<(String, Stat)> = match node {
            Node::Root => {
                let fi = finfo(true, 0, self.started, self.started);
                self.meta.trackers.iter().map(|t| (sanitize(&t.name, 100), fi)).collect()
            }
            Node::Tracker(tid) => self
                .list(st, tid)?
                .iter()
                .map(|i| (folder(i), finfo(true, 0, ft(&i.created_on), ft(&i.updated_on))))
                .collect(),
            Node::IssueDir(id) => {
                let i = self.must_issue(st, id)?;
                let mut v = vec![
                    ("issue.md".to_string(), self.info(st, Node::IssueMd(id), &format!("#{id}\\issue.md"))?),
                    ("attachments".to_string(), self.info(st, Node::AttachDir(id), "")?),
                ];
                for j in i.journals.iter().filter(|j| has_notes(j)) {
                    let name = format!("comment_{}.md", j.id);
                    v.push((name.clone(), self.info(st, Node::Comment(id, j.id), &format!("#{id}\\{name}"))?));
                }
                v
            }
            Node::AttachDir(id) => {
                let i = self.must_issue(st, id)?;
                att_names(&i)
                    .into_iter()
                    .map(|(n, a)| {
                        let t = ft(&a.created_on);
                        (n, finfo(false, a.filesize, t, t))
                    })
                    .collect()
            }
            Node::Pending(_) => Vec::new(),
            _ => return Err(E::NotDir),
        };
        for (name, fi) in local_children(st, key) {
            if !out.iter().any(|(n, _)| n.to_lowercase() == name.to_lowercase()) {
                out.push((name, fi));
            }
        }
        out.sort_by_key(|(n, _)| n.to_lowercase());
        Ok(out)
    }

    /// Writable buffer for a file (creates the edit buffer of issue.md / comments on demand).
    fn entry<'a>(&self, st: &'a mut State, node: Node, key: &str, path: &str) -> Res<&'a mut Entry> {
        match node {
            Node::IssueMd(_) | Node::Comment(..) if !st.local.contains_key(key) => {
                let data = self.render(st, node)?;
                st.local.insert(key.to_string(), Entry::new(split_parent(path).1, false, data, true));
            }
            Node::IssueMd(_) | Node::Comment(..) | Node::Local => {}
            _ => return Err(E::Denied),
        }
        st.local.get_mut(key).filter(|e| !e.dir).ok_or(E::Denied)
    }

    /// Sends a dirty buffer to Redmine. On failure the buffer stays dirty and `<name>.error.txt` explains why.
    fn commit(&self, st: &mut State, node: Node, path: &str, key: &str) {
        let Some(e) = st.local.get(key) else { return };
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
                    Done::Nothing => {}
                }
            }
            Err(e) => {
                eprintln!("{path}: {e:#}");
                let msg = format!(
                    "Failed to send {path}. Your edits remain on this drive (they will be lost on unmount).\r\n\r\n{e:#}\r\n"
                );
                st.local.insert(err_key, Entry::new(&format!("{name}.error.txt"), false, msg.into_bytes(), false));
            }
        }
    }

    fn send(&self, st: &mut State, node: Node, path: &str, name: &str, data: Vec<u8>) -> anyhow::Result<Done> {
        match node {
            Node::IssueMd(id) => {
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
                if !body.is_empty() {
                    self.api.send(Method::PUT, &format!("/issues/{id}.json"), Some(json!({ "issue": body })))?;
                }
                Ok(Done::Updated(id))
            }
            Node::Comment(id, j) => {
                let notes = markdown::parse_comment(utf8(&data)?);
                self.api.send(Method::PUT, &format!("/journals/{j}.json"), Some(json!({ "journal": { "notes": notes } })))?;
                Ok(Done::Updated(id))
            }
            Node::Local => {
                let Some((parent, pkey)) = self.resolve(st, split_parent(path).0).map_err(|_| anyhow!("resolve failed"))?
                else {
                    return Ok(Done::Nothing);
                };
                let lname = name.to_lowercase();
                match parent {
                    Node::Pending(tracker) if lname == "issue.md" => {
                        let mut p = markdown::parse_issue(utf8(&data)?, &self.meta, None)?.payload;
                        if p.fields.get("subject").is_none_or(|s| s.is_empty()) {
                            let folder = st.local.get(&pkey).map(|e| e.name.clone()).unwrap_or_default();
                            p.fields.insert("subject", folder);
                        }
                        let mut body = p.diff(None);
                        body.insert("project_id".into(), json!(self.meta.project_id));
                        body.insert("tracker_id".into(), json!(tracker));
                        let v = self.api.send(Method::POST, "/issues.json", Some(json!({ "issue": body })))?;
                        let id = v["issue"]["id"].as_u64().context("No id in response")?;
                        Ok(Done::Created(id, pkey))
                    }
                    Node::IssueDir(id) if lname.ends_with(".md") => {
                        let notes = markdown::parse_comment(utf8(&data)?);
                        if notes.is_empty() {
                            return Ok(Done::Nothing);
                        }
                        self.api.send(Method::PUT, &format!("/issues/{id}.json"), Some(json!({ "issue": { "notes": notes } })))?;
                        Ok(Done::Consumed(id))
                    }
                    Node::AttachDir(id) if !data.is_empty() => {
                        let token = self.api.upload(name, data)?;
                        let up = json!({ "issue": { "uploads": [{ "token": token, "filename": name }] } });
                        self.api.send(Method::PUT, &format!("/issues/{id}.json"), Some(up))?;
                        Ok(Done::Consumed(id))
                    }
                    _ => Ok(Done::Nothing),
                }
            }
            _ => Ok(Done::Nothing),
        }
    }

    fn remove(&self, st: &mut State, node: Node, key: &str) {
        if let Node::Attachment(id, att) = node {
            if let Err(e) = self.api.send(Method::DELETE, &format!("/attachments/{att}.json"), None) {
                eprintln!("attachment {att}: {e:#}");
            }
            st.blobs.remove(&att);
            self.invalidate(st, id);
        } else {
            // local file / pending folder / discarding unsent edits of issue.md or a comment
            remove_tree(st, key);
            st.local.remove(&format!("{key}.error.txt"));
        }
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
        self.info(&mut st, node, &key)
    }

    /// Counts a handle; issue.md / comments get a snapshot so the handle sees one consistent text.
    pub fn open(&self, path: &str) -> Res<Stat> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        if matches!(node, Node::IssueMd(_) | Node::Comment(..)) {
            self.entry(&mut st, node, &key, path)?;
        }
        if let Some(e) = st.local.get_mut(&key) {
            e.opens += 1;
        }
        self.info(&mut st, node, &key)
    }

    pub fn close(&self, path: &str) {
        let mut st = self.lock();
        let Ok(Some((_, key))) = self.resolve(&mut st, path) else { return };
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
        match (pnode, dir) {
            (Node::Tracker(_), true) | (Node::IssueDir(_) | Node::Pending(_) | Node::AttachDir(_), false) => {}
            _ => return Err(E::Denied),
        }
        let key = format!("{pkey}\\{}", name.to_lowercase());
        if st.local.contains_key(&key) {
            return Err(E::Exists);
        }
        let mut e = Entry::new(name, dir, Vec::new(), false);
        e.opens = 1;
        st.local.insert(key.clone(), e);
        if dir {
            let tpl = markdown::template(&self.meta).into_bytes();
            st.local.insert(format!("{key}\\issue.md"), Entry::new("issue.md", false, tpl, false));
        }
        self.info(&mut st, Node::Local, &key)
    }

    /// A handle is done: delete the file (`check_delete` passed) or send its unsent edits.
    pub fn cleanup(&self, path: &str, delete: bool) {
        let mut st = self.lock();
        let Ok(Some((node, key))) = self.resolve(&mut st, path) else { return };
        if delete {
            self.remove(&mut st, node, &key);
        } else if st.local.get(&key).is_some_and(|e| e.dirty) {
            self.commit(&mut st, node, path, &key);
        }
    }

    pub fn check_delete(&self, path: &str) -> Res<()> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        match node {
            Node::Local | Node::Attachment(..) => Ok(()),
            Node::Pending(_) if local_children(&st, &key).is_empty() => Ok(()),
            Node::Pending(_) => Err(E::NotEmpty),
            // only unsent edits can be "deleted" (= discarded)
            Node::IssueMd(_) | Node::Comment(..) if st.local.get(&key).is_some_and(|e| e.dirty) => Ok(()),
            _ => Err(E::Denied),
        }
    }

    /// `alloc_only`: just an allocation hint; it may shrink but never grow the file.
    pub fn set_size(&self, path: &str, size: u64, alloc_only: bool) -> Res<Stat> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        let e = self.entry(&mut st, node, &key, path)?;
        let n = size as usize;
        if (!alloc_only || n < e.data.len()) && n != e.data.len() {
            e.data.resize(n, 0);
            e.dirty = true;
        }
        self.info(&mut st, node, &key)
    }

    /// Sorted entries, without `.` / `..`.
    pub fn read_dir(&self, path: &str) -> Res<Vec<(String, Stat)>> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        self.list_dir(&mut st, node, &key)
    }

    pub fn rename(&self, from: &str, to: &str, replace_if_exists: bool) -> Res<()> {
        let (to_parent, to_name) = split_parent(to);
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, from)?;
        let (pnode, pkey) = self.resolve(&mut st, to_parent)?.ok_or(E::PathNotFound)?;
        let target = self.resolve(&mut st, to)?;
        let new_key = match (node, pnode, &target) {
            // case-only rename
            (Node::Local | Node::Pending(_), _, Some((_, k))) if *k == key => key.clone(),
            (Node::Pending(t), Node::Tracker(pt), None) if t == pt => format!("{pkey}\\{}", to_name.to_lowercase()),
            (Node::Local, Node::IssueDir(_) | Node::Pending(_) | Node::AttachDir(_), None) => {
                format!("{pkey}\\{}", to_name.to_lowercase())
            }
            // atomic save: temp file -> issue.md / comment
            (Node::Local, _, Some((Node::Local | Node::IssueMd(_) | Node::Comment(..), k))) if replace_if_exists => k.clone(),
            (_, _, Some(_)) => return Err(E::Exists),
            _ => return Err(E::Denied),
        };
        let replaced_opens = if new_key == key { 0 } else { st.local.remove(&new_key).map_or(0, |e| e.opens) };
        let prefix = format!("{key}\\");
        let moved: Vec<String> = st.local.keys().filter(|k| **k == key || k.starts_with(&prefix)).cloned().collect();
        for k in moved {
            let mut e = st.local.remove(&k).expect("key listed above");
            if k == key {
                e.name = to_name.to_string();
                e.opens += replaced_opens;
                e.transient |= matches!(target, Some((Node::IssueMd(_) | Node::Comment(..), _)));
            }
            st.local.insert(format!("{new_key}{}", &k[key.len()..]), e);
        }
        // A file landing on a meaningful name (issue.md, *.md, attachments\) is sent right away.
        if node == Node::Local
            && let Some(e) = st.local.get_mut(&new_key)
            && !e.data.is_empty()
        {
            e.dirty = true;
            let tnode = target.map_or(Node::Local, |(n, _)| n);
            self.commit(&mut st, tnode, to, &new_key);
        }
        Ok(())
    }

    /// Bytes copied; 0 at or past the end.
    pub fn read(&self, path: &str, buffer: &mut [u8], offset: u64) -> Res<usize> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        let rendered = match node {
            _ if st.local.contains_key(&key) => None,
            Node::Attachment(id, att) => {
                self.blob(&mut st, id, att)?;
                None
            }
            Node::IssueMd(_) | Node::Comment(..) => Some(self.render(&mut st, node)?),
            _ => None,
        };
        let data: &[u8] = match (&rendered, st.local.get(&key), node) {
            (Some(r), _, _) => r.as_slice(),
            (None, Some(e), _) => e.data.as_slice(),
            (None, None, Node::Attachment(_, att)) => st.blobs[&att].as_slice(),
            _ => &[],
        };
        let off = (offset as usize).min(data.len());
        let n = buffer.len().min(data.len() - off);
        buffer[..n].copy_from_slice(&data[off..off + n]);
        Ok(n)
    }

    /// `constrained`: never grow the file (Windows paging I/O).
    pub fn write(&self, path: &str, buffer: &[u8], offset: u64, to_eof: bool, constrained: bool) -> Res<(usize, Stat)> {
        let mut st = self.lock();
        let (node, key) = self.must_resolve(&mut st, path)?;
        let e = self.entry(&mut st, node, &key, path)?;
        let off = if to_eof { e.data.len() } else { offset as usize };
        let len = if constrained { buffer.len().min(e.data.len().saturating_sub(off)) } else { buffer.len() };
        if len > 0 {
            if off + len > e.data.len() {
                e.data.resize(off + len, 0);
            }
            e.data[off..off + len].copy_from_slice(&buffer[..len]);
            e.dirty = true;
        }
        Ok((len, self.info(&mut st, node, &key)?))
    }
}

fn local_children(st: &State, key: &str) -> Vec<(String, Stat)> {
    let prefix = format!("{key}\\");
    st.local
        .range(prefix.clone()..)
        .take_while(|(k, _)| k.starts_with(&prefix))
        .filter(|(k, _)| !k[prefix.len()..].contains('\\'))
        .map(|(_, e)| (e.name.clone(), finfo(e.dir, e.data.len() as u64, e.time, e.time)))
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
        .map(|c| if c.is_control() || r#"\/:*?"<>|"#.contains(c) { '_' } else { c })
        .take(max)
        .collect();
    s.trim_end_matches(['.', ' ']).to_string()
}

fn folder(i: &Issue) -> String {
    format!("{} {}", i.id, sanitize(&i.subject, 60)).trim_end().to_string()
}

fn leading_id(s: &str) -> Option<u64> {
    s.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

fn comment_id(name: &str) -> Option<u64> {
    name.strip_prefix("comment_")?.strip_suffix(".md")?.parse().ok()
}

/// Attachment file names; duplicates get an `<id>_` prefix.
fn att_names(i: &Issue) -> Vec<(String, &Attachment)> {
    i.attachments
        .iter()
        .map(|a| {
            let dup = i.attachments.iter().filter(|b| b.filename.eq_ignore_ascii_case(&a.filename)).count() > 1;
            (if dup { format!("{}_{}", a.id, a.filename) } else { a.filename.clone() }, a)
        })
        .collect()
}

fn finfo(dir: bool, size: u64, created: u64, modified: u64) -> Stat {
    Stat { dir, size, created, modified }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64 / 100) + EPOCH
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

    #[test]
    fn names_and_times() {
        assert_eq!(ft("1970-01-01T00:00:00Z"), EPOCH);
        assert_eq!(ft("2000-03-01T00:00:00Z"), 951_868_800 * 10_000_000 + EPOCH);
        assert_eq!(leading_id("123 Login fails"), Some(123));
        assert_eq!(leading_id("New folder"), None);
        assert_eq!(comment_id("comment_45.md"), Some(45));
        assert_eq!(sanitize("a/b: c?. ", 60), "a_b_ c_");
    }

    #[test]
    fn multibyte_names() {
        assert_eq!(leading_id("123 ログイン不具合"), Some(123));
        assert_eq!(leading_id("新しいフォルダー"), None);
        // max counts chars, not bytes
        assert_eq!(sanitize("日本語：テスト?", 5), "日本語：テ");
        assert_eq!(sanitize("件名/です。 ", 60), "件名_です。");
    }
}
