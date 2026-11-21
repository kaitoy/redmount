//! WinFsp filesystem: `\<tracker>\<id subject>\{issue.md, comment_<journal>.md, attachments\*}`.
use crate::markdown;
use crate::redmine::{Api, Attachment, Issue, Journal, Meta};
use anyhow::{Context, anyhow, bail};
use reqwest::Method;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{
    NTSTATUS, STATUS_ACCESS_DENIED, STATUS_DIRECTORY_NOT_EMPTY, STATUS_END_OF_FILE, STATUS_IO_DEVICE_ERROR,
    STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
};
use winfsp::constants::FspCleanupFlags;
use winfsp::filesystem::{
    DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, U16CStr};

type Res<T> = winfsp::Result<T>;

const TTL: Duration = Duration::from_secs(30);
const DIR: u32 = 0x10; // FILE_ATTRIBUTE_DIRECTORY
const FILE: u32 = 0x20; // FILE_ATTRIBUTE_ARCHIVE
const FILE_DIRECTORY_FILE: u32 = 0x1;
const EPOCH: u64 = 116_444_736_000_000_000; // 1601-01-01 -> 1970-01-01 in 100ns

fn err(s: NTSTATUS) -> FspError {
    FspError::NTSTATUS(s.0)
}
fn nf() -> FspError {
    err(STATUS_OBJECT_NAME_NOT_FOUND)
}
fn io(e: anyhow::Error) -> FspError {
    eprintln!("error: {e:#}");
    err(STATUS_IO_DEVICE_ERROR)
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

pub struct Handle {
    path: Mutex<String>,
}

impl Handle {
    fn path(&self) -> String {
        self.path.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn set_path(&self, p: String) {
        *self.path.lock().unwrap_or_else(|e| e.into_inner()) = p;
    }
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
    sd: Vec<u8>,
    started: u64,
    // ponytail: one global lock, held across HTTP calls; per-path locks if it gets sluggish
    st: Mutex<State>,
}

impl RedmineFs {
    pub fn new(api: Api, meta: Meta) -> anyhow::Result<Self> {
        Ok(Self { api, meta, sd: security_descriptor()?, started: now(), st: Mutex::default() })
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

    fn node_of(&self, st: &mut State, h: &Handle) -> Res<(Node, String)> {
        self.resolve(st, &h.path())?.ok_or_else(nf)
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

    fn info(&self, st: &mut State, node: Node, key: &str) -> Res<FileInfo> {
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

    fn list_dir(&self, st: &mut State, node: Node, key: &str) -> Res<Vec<(String, FileInfo)>> {
        let mut out: Vec<(String, FileInfo)> = match node {
            Node::Root => {
                let fi = finfo(true, 0, self.started, self.started);
                self.meta.trackers.iter().map(|t| (sanitize(&t.name, 100), fi.clone())).collect()
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
            _ => return Err(err(STATUS_NOT_A_DIRECTORY)),
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
            _ => return Err(err(STATUS_ACCESS_DENIED)),
        }
        st.local.get_mut(key).filter(|e| !e.dir).ok_or_else(|| err(STATUS_ACCESS_DENIED))
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

    fn copy_sd(&self, buf: Option<&mut [c_void]>) {
        if let Some(buf) = buf
            && buf.len() >= self.sd.len()
        {
            // SAFETY: buf holds at least sd.len() bytes.
            unsafe { std::ptr::copy_nonoverlapping(self.sd.as_ptr(), buf.as_mut_ptr().cast::<u8>(), self.sd.len()) };
        }
    }
}

impl FileSystemContext for RedmineFs {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> Res<FileSecurity> {
        let path = file_name.to_string_lossy();
        let mut st = self.lock();
        let (node, key) = self.resolve(&mut st, &path)?.ok_or_else(nf)?;
        let dir = node.is_dir() || st.local.get(&key).is_some_and(|e| e.dir);
        self.copy_sd(security_descriptor);
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: self.sd.len() as u64,
            attributes: if dir { DIR } else { FILE },
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> Res<Handle> {
        let path = file_name.to_string_lossy();
        let mut st = self.lock();
        let (node, key) = self.resolve(&mut st, &path)?.ok_or_else(nf)?;
        if matches!(node, Node::IssueMd(_) | Node::Comment(..)) {
            // snapshot so readers/writers of this handle see one consistent text
            self.entry(&mut st, node, &key, &path)?;
        }
        if let Some(e) = st.local.get_mut(&key) {
            e.opens += 1;
        }
        *file_info.as_mut() = self.info(&mut st, node, &key)?;
        Ok(Handle { path: Mutex::new(path) })
    }

    fn close(&self, h: Handle) {
        let mut st = self.lock();
        let Ok(Some((_, key))) = self.resolve(&mut st, &h.path()) else { return };
        let idle = st.local.get_mut(&key).is_some_and(|e| {
            e.opens = e.opens.saturating_sub(1);
            e.opens == 0 && e.transient && !e.dirty
        });
        if idle {
            st.local.remove(&key);
        }
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> Res<Handle> {
        let path = file_name.to_string_lossy();
        let (parent, name) = split_parent(&path);
        let mut st = self.lock();
        let (pnode, pkey) = self.resolve(&mut st, parent)?.ok_or_else(|| err(STATUS_OBJECT_PATH_NOT_FOUND))?;
        let dir = create_options & FILE_DIRECTORY_FILE != 0;
        match (pnode, dir) {
            (Node::Tracker(_), true) | (Node::IssueDir(_) | Node::Pending(_) | Node::AttachDir(_), false) => {}
            _ => return Err(err(STATUS_ACCESS_DENIED)),
        }
        let key = format!("{pkey}\\{}", name.to_lowercase());
        if st.local.contains_key(&key) {
            return Err(err(STATUS_OBJECT_NAME_COLLISION));
        }
        let mut e = Entry::new(name, dir, Vec::new(), false);
        e.opens = 1;
        st.local.insert(key.clone(), e);
        if dir {
            let tpl = markdown::template(&self.meta).into_bytes();
            st.local.insert(format!("{key}\\issue.md"), Entry::new("issue.md", false, tpl, false));
        }
        *file_info.as_mut() = self.info(&mut st, Node::Local, &key)?;
        Ok(Handle { path: Mutex::new(path) })
    }

    fn cleanup(&self, h: &Handle, _file_name: Option<&U16CStr>, flags: u32) {
        let path = h.path();
        let mut st = self.lock();
        let Ok(Some((node, key))) = self.resolve(&mut st, &path) else { return };
        if FspCleanupFlags::FspCleanupDelete.is_flagged(flags) {
            self.remove(&mut st, node, &key);
        } else if st.local.get(&key).is_some_and(|e| e.dirty) {
            self.commit(&mut st, node, &path, &key);
        }
    }

    fn flush(&self, h: Option<&Handle>, file_info: &mut FileInfo) -> Res<()> {
        match h {
            Some(h) => self.get_file_info(h, file_info),
            None => Ok(()),
        }
    }

    fn get_file_info(&self, h: &Handle, file_info: &mut FileInfo) -> Res<()> {
        let mut st = self.lock();
        let (node, key) = self.node_of(&mut st, h)?;
        *file_info = self.info(&mut st, node, &key)?;
        Ok(())
    }

    fn get_security(&self, _h: &Handle, security_descriptor: Option<&mut [c_void]>) -> Res<u64> {
        self.copy_sd(security_descriptor);
        Ok(self.sd.len() as u64)
    }

    fn overwrite(
        &self,
        h: &Handle,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> Res<()> {
        let mut st = self.lock();
        let (node, key) = self.node_of(&mut st, h)?;
        let e = self.entry(&mut st, node, &key, &h.path())?;
        e.data.clear();
        e.dirty = true;
        *file_info = self.info(&mut st, node, &key)?;
        Ok(())
    }

    fn read_directory(
        &self,
        h: &Handle,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> Res<u32> {
        let mut st = self.lock();
        let (node, key) = self.node_of(&mut st, h)?;
        let mut entries = self.list_dir(&mut st, node, &key)?;
        drop(st);
        if node != Node::Root {
            let me = finfo(true, 0, self.started, self.started);
            entries.insert(0, ("..".to_string(), me.clone()));
            entries.insert(0, (".".to_string(), me));
        }
        let start = match marker.inner_as_cstr() {
            None => 0,
            Some(m) => {
                let m = m.to_string_lossy();
                match entries.iter().position(|(n, _)| *n == m) {
                    Some(i) => i + 1,
                    None => entries.iter().position(|(n, _)| n.to_lowercase() > m.to_lowercase()).unwrap_or(entries.len()),
                }
            }
        };
        let mut cursor = 0u32;
        for (name, fi) in &entries[start..] {
            let mut di = DirInfo::<255>::new();
            *di.file_info_mut() = fi.clone();
            di.set_name(name)?;
            if !di.append_to_buffer(buffer, &mut cursor) {
                return Ok(cursor);
            }
        }
        DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
        Ok(cursor)
    }

    fn rename(&self, h: &Handle, file_name: &U16CStr, new_file_name: &U16CStr, replace_if_exists: bool) -> Res<()> {
        let (from, to) = (file_name.to_string_lossy(), new_file_name.to_string_lossy());
        let (to_parent, to_name) = split_parent(&to);
        let mut st = self.lock();
        let (node, key) = self.resolve(&mut st, &from)?.ok_or_else(nf)?;
        let (pnode, pkey) = self.resolve(&mut st, to_parent)?.ok_or_else(|| err(STATUS_OBJECT_PATH_NOT_FOUND))?;
        let target = self.resolve(&mut st, &to)?;
        let new_key = match (node, pnode, &target) {
            // case-only rename
            (Node::Local | Node::Pending(_), _, Some((_, k))) if *k == key => key.clone(),
            (Node::Pending(t), Node::Tracker(pt), None) if t == pt => format!("{pkey}\\{}", to_name.to_lowercase()),
            (Node::Local, Node::IssueDir(_) | Node::Pending(_) | Node::AttachDir(_), None) => {
                format!("{pkey}\\{}", to_name.to_lowercase())
            }
            // atomic save: temp file -> issue.md / comment
            (Node::Local, _, Some((Node::Local | Node::IssueMd(_) | Node::Comment(..), k))) if replace_if_exists => k.clone(),
            (_, _, Some(_)) => return Err(err(STATUS_OBJECT_NAME_COLLISION)),
            _ => return Err(err(STATUS_ACCESS_DENIED)),
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
        h.set_path(to.clone());
        // A file landing on a meaningful name (issue.md, *.md, attachments\) is sent right away.
        if node == Node::Local
            && let Some(e) = st.local.get_mut(&new_key)
            && !e.data.is_empty()
        {
            e.dirty = true;
            let tnode = target.map_or(Node::Local, |(n, _)| n);
            self.commit(&mut st, tnode, &to, &new_key);
        }
        Ok(())
    }

    fn set_basic_info(
        &self,
        h: &Handle,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> Res<()> {
        self.get_file_info(h, file_info)
    }

    fn set_delete(&self, h: &Handle, _file_name: &U16CStr, delete_file: bool) -> Res<()> {
        if !delete_file {
            return Ok(());
        }
        let mut st = self.lock();
        let (node, key) = self.node_of(&mut st, h)?;
        match node {
            Node::Local | Node::Attachment(..) => Ok(()),
            Node::Pending(_) if local_children(&st, &key).is_empty() => Ok(()),
            Node::Pending(_) => Err(err(STATUS_DIRECTORY_NOT_EMPTY)),
            // only unsent edits can be "deleted" (= discarded)
            Node::IssueMd(_) | Node::Comment(..) if st.local.get(&key).is_some_and(|e| e.dirty) => Ok(()),
            _ => Err(err(STATUS_ACCESS_DENIED)),
        }
    }

    fn set_file_size(&self, h: &Handle, new_size: u64, set_allocation_size: bool, file_info: &mut FileInfo) -> Res<()> {
        let mut st = self.lock();
        let (node, key) = self.node_of(&mut st, h)?;
        let e = self.entry(&mut st, node, &key, &h.path())?;
        let n = new_size as usize;
        if (!set_allocation_size || n < e.data.len()) && n != e.data.len() {
            e.data.resize(n, 0);
            e.dirty = true;
        }
        *file_info = self.info(&mut st, node, &key)?;
        Ok(())
    }

    fn read(&self, h: &Handle, buffer: &mut [u8], offset: u64) -> Res<u32> {
        let mut st = self.lock();
        let (node, key) = self.node_of(&mut st, h)?;
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
        let off = offset as usize;
        if off >= data.len() {
            return Err(err(STATUS_END_OF_FILE));
        }
        let n = buffer.len().min(data.len() - off);
        buffer[..n].copy_from_slice(&data[off..off + n]);
        Ok(n as u32)
    }

    fn write(
        &self,
        h: &Handle,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> Res<u32> {
        let mut st = self.lock();
        let (node, key) = self.node_of(&mut st, h)?;
        let e = self.entry(&mut st, node, &key, &h.path())?;
        let off = if write_to_eof { e.data.len() } else { offset as usize };
        let len = if constrained_io { buffer.len().min(e.data.len().saturating_sub(off)) } else { buffer.len() };
        if len > 0 {
            if off + len > e.data.len() {
                e.data.resize(off + len, 0);
            }
            e.data[off..off + len].copy_from_slice(&buffer[..len]);
            e.dirty = true;
        }
        *file_info = self.info(&mut st, node, &key)?;
        Ok(len as u32)
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> Res<()> {
        out.total_size = 1 << 40;
        out.free_size = 1 << 39;
        out.set_volume_label("Redmine");
        Ok(())
    }
}

fn local_children(st: &State, key: &str) -> Vec<(String, FileInfo)> {
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

fn finfo(dir: bool, size: u64, created: u64, modified: u64) -> FileInfo {
    FileInfo {
        file_attributes: if dir { DIR } else { FILE },
        file_size: size,
        allocation_size: size.div_ceil(4096) * 4096,
        creation_time: created,
        last_access_time: modified,
        last_write_time: modified,
        change_time: modified,
        ..Default::default()
    }
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

fn security_descriptor() -> anyhow::Result<Vec<u8>> {
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{GetSecurityDescriptorLength, PSECURITY_DESCRIPTOR};
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: plain Win32 calls; the descriptor is read right after creation.
    // ponytail: the LocalAlloc'd descriptor is leaked once at startup
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            windows::core::w!("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)"),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )?;
        Ok(std::slice::from_raw_parts(sd.0 as *const u8, GetSecurityDescriptorLength(sd) as usize).to_vec())
    }
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
