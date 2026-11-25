//! issue.md / comment_*.md / page.md <-> Redmine payloads.
use crate::redmine::{IdName, Issue, Journal, Meta, WikiPage};
use anyhow::{Result, anyhow};
use serde::Serialize;
use serde_json::{Map, Value as J, json};
use serde_yaml::{Mapping, Value as Y};
use std::collections::BTreeMap;

const RO_MARK: &str = "# --- Read-only below (edits are ignored) ---\n";

/// Issue fields as Redmine API strings ("" = empty), used both to diff and to send.
#[derive(Default, Debug, PartialEq)]
pub struct Payload {
    pub fields: BTreeMap<&'static str, String>,
    pub cfs: BTreeMap<u64, J>,
}

pub struct Parsed {
    pub updated_on: Option<String>,
    pub payload: Payload,
}

impl Payload {
    /// Fields that differ from `base` (or are non-empty when creating).
    pub fn diff(&self, base: Option<&Payload>) -> Map<String, J> {
        let mut out = Map::new();
        for (k, v) in &self.fields {
            if base.map_or(!v.is_empty(), |b| b.fields.get(k) != Some(v)) {
                out.insert(k.to_string(), J::String(v.clone()));
            }
        }
        let cfs: Vec<J> = self
            .cfs
            .iter()
            .filter(|(id, v)| {
                base.map_or(*v != &J::String(String::new()), |b| {
                    b.cfs.get(*id) != Some(*v)
                })
            })
            .map(|(id, v)| json!({ "id": id, "value": v }))
            .collect();
        if !cfs.is_empty() {
            out.insert("custom_fields".into(), J::Array(cfs));
        }
        out
    }
}

fn y<T: Serialize>(v: T) -> Y {
    serde_yaml::to_value(v).unwrap_or(Y::Null)
}

fn yaml(pairs: Vec<(&str, Y)>) -> String {
    let m: Mapping = pairs.into_iter().map(|(k, v)| (y(k), v)).collect();
    // `key: null` -> `key:` so empty fields look empty.
    serde_yaml::to_string(&m)
        .unwrap_or_default()
        .lines()
        .map(|l| {
            format!(
                "{}\n",
                l.strip_suffix(": null")
                    .map_or(l.to_string(), |k| format!("{k}:"))
            )
        })
        .collect()
}

fn editable(i: Option<&Issue>, meta: &Meta) -> Vec<(&'static str, Y)> {
    let name = |f: fn(&Issue) -> Option<&IdName>| y(i.and_then(f).map(|x| &x.name));
    let cf: Mapping = match i {
        Some(i) => i
            .custom_fields
            .iter()
            .map(|c| (y(&c.name), y(&c.value)))
            .collect(),
        None => meta
            .custom_fields
            .iter()
            .map(|c| (y(&c.name), Y::Null))
            .collect(),
    };
    vec![
        ("subject", y(i.map(|i| &i.subject))),
        ("status", name(|i| Some(&i.status))),
        ("priority", name(|i| Some(&i.priority))),
        ("assigned_to", name(|i| i.assigned_to.as_ref())),
        ("category", name(|i| i.category.as_ref())),
        ("fixed_version", name(|i| i.fixed_version.as_ref())),
        (
            "parent_issue_id",
            y(i.and_then(|i| i.parent.as_ref()).map(|p| p.id)),
        ),
        ("start_date", y(i.and_then(|i| i.start_date.as_ref()))),
        ("due_date", y(i.and_then(|i| i.due_date.as_ref()))),
        ("done_ratio", y(i.and_then(|i| i.done_ratio))),
        ("estimated_hours", y(i.and_then(|i| i.estimated_hours))),
        ("custom_fields", Y::Mapping(cf)),
    ]
}

pub fn render_issue(i: &Issue, meta: &Meta) -> String {
    let ro = vec![
        ("id", y(i.id)),
        ("tracker", y(&i.tracker.name)),
        ("author", y(&i.author.name)),
        ("created_on", y(&i.created_on)),
        ("updated_on", y(&i.updated_on)),
    ];
    format!(
        "---\n{}{RO_MARK}{}---\n{}",
        yaml(editable(Some(i), meta)),
        yaml(ro),
        i.description.as_deref().unwrap_or("")
    )
}

/// issue.md placed in a freshly created ticket folder.
pub fn template(meta: &Meta) -> String {
    format!("---\n{}---\n", yaml(editable(None, meta)))
}

pub fn render_comment(j: &Journal) -> String {
    let ro = vec![
        ("author", y(&j.user.name)),
        ("created_on", y(&j.created_on)),
    ];
    format!(
        "---\n{RO_MARK}{}---\n{}",
        yaml(ro),
        j.notes.as_deref().unwrap_or("")
    )
}

pub fn parse_comment(text: &str) -> String {
    norm(split(text).1)
}

#[derive(Debug, PartialEq)]
pub struct ParsedWiki {
    pub parent: String,
    pub comments: String,
    pub version: Option<u64>,
    pub text: String,
}

pub fn render_wiki(p: &WikiPage) -> String {
    let ed = vec![
        ("parent", y(p.parent.as_ref().map(|x| &x.title))),
        ("comments", Y::Null),
    ];
    let ro = vec![
        ("title", y(&p.title)),
        ("version", y(p.version)),
        ("author", y(&p.author.name)),
        ("created_on", y(&p.created_on)),
        ("updated_on", y(&p.updated_on)),
    ];
    format!("---\n{}{RO_MARK}{}---\n{}", yaml(ed), yaml(ro), p.text)
}

/// page.md placed in a freshly created wiki folder.
pub fn wiki_template() -> String {
    let ed = vec![("parent", Y::Null), ("comments", Y::Null)];
    format!("---\n{}---\n", yaml(ed))
}

pub fn parse_wiki(text: &str) -> Result<ParsedWiki> {
    let (fm, body) = split(text);
    let m: Mapping = match fm {
        Some(f) => serde_yaml::from_str::<Option<Mapping>>(f)?.unwrap_or_default(),
        None => Mapping::new(),
    };
    let g = |k: &str| m.get(k).map(scalar).unwrap_or_default();
    let ver = g("version");
    let version = if ver.is_empty() {
        None
    } else {
        Some(
            ver.parse::<u64>()
                .map_err(|_| anyhow!("version: '{ver}' must be a number"))?,
        )
    };
    Ok(ParsedWiki {
        parent: g("parent"),
        comments: g("comments"),
        version,
        text: norm(body),
    })
}

pub fn norm_text(s: &str) -> String {
    norm(s)
}

/// Splits `---` front matter from the body.
fn split(text: &str) -> (Option<&str>, &str) {
    let t = text.trim_start_matches('\u{feff}');
    let mut lines = t.split_inclusive('\n');
    let Some(first) = lines.next().filter(|l| l.trim_end() == "---") else {
        return (None, t);
    };
    let mut pos = first.len();
    for l in lines {
        if l.trim_end() == "---" {
            return (Some(&t[first.len()..pos]), &t[pos + l.len()..]);
        }
        pos += l.len();
    }
    (None, t)
}

fn norm(s: &str) -> String {
    s.replace("\r\n", "\n").trim_end().to_string()
}

fn scalar(v: &Y) -> String {
    match v {
        Y::Null => String::new(),
        Y::String(s) => s.trim().to_string(),
        Y::Bool(b) => b.to_string(),
        Y::Number(n) => n.to_string(),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

fn js(v: &J) -> String {
    match v {
        J::Null => String::new(),
        J::String(s) => s.clone(),
        x => x.to_string(),
    }
}

/// Custom field values as strings (or arrays of strings for multi-value fields).
fn cf_json(v: &J) -> J {
    match v {
        J::Array(a) => J::Array(a.iter().map(|x| J::String(js(x))).collect()),
        x => J::String(js(x)),
    }
}

fn num(s: &str) -> String {
    s.parse::<f64>().map_or(s.to_string(), |f| f.to_string())
}

fn lookup(list: &[IdName], name: &str, cur: Option<&IdName>, field: &str) -> Result<String> {
    if name.is_empty() {
        return Ok(String::new());
    }
    if let Some(c) = cur.filter(|c| c.name == name) {
        return Ok(c.id.to_string());
    }
    list.iter()
        .find(|x| x.name.eq_ignore_ascii_case(name))
        .map(|x| x.id.to_string())
        .or_else(|| name.parse::<u64>().ok().map(|n| n.to_string()))
        .ok_or_else(|| anyhow!("{field}: '{name}' not found"))
}

pub fn payload_of(i: &Issue) -> Payload {
    let id = |o: Option<&IdName>| o.map_or(String::new(), |x| x.id.to_string());
    let fields = BTreeMap::from([
        ("subject", i.subject.clone()),
        ("description", norm(i.description.as_deref().unwrap_or(""))),
        ("status_id", i.status.id.to_string()),
        ("priority_id", i.priority.id.to_string()),
        ("assigned_to_id", id(i.assigned_to.as_ref())),
        ("category_id", id(i.category.as_ref())),
        ("fixed_version_id", id(i.fixed_version.as_ref())),
        ("parent_issue_id", id(i.parent.as_ref())),
        ("start_date", i.start_date.clone().unwrap_or_default()),
        ("due_date", i.due_date.clone().unwrap_or_default()),
        (
            "done_ratio",
            i.done_ratio.map_or(String::new(), |d| d.to_string()),
        ),
        (
            "estimated_hours",
            i.estimated_hours.map_or(String::new(), |h| h.to_string()),
        ),
    ]);
    let cfs = i
        .custom_fields
        .iter()
        .map(|c| (c.id, cf_json(&c.value)))
        .collect();
    Payload { fields, cfs }
}

/// `cur` is the server's current issue (None when creating).
pub fn parse_issue(text: &str, meta: &Meta, cur: Option<&Issue>) -> Result<Parsed> {
    let (fm, body) = split(text);
    let m: Mapping = match fm {
        Some(f) => serde_yaml::from_str::<Option<Mapping>>(f)?.unwrap_or_default(),
        None => Mapping::new(),
    };
    let g = |k: &str| m.get(k).map(scalar).unwrap_or_default();
    let fields = BTreeMap::from([
        ("subject", g("subject")),
        ("description", norm(body)),
        (
            "status_id",
            lookup(
                &meta.statuses,
                &g("status"),
                cur.map(|i| &i.status),
                "status",
            )?,
        ),
        (
            "priority_id",
            lookup(
                &meta.priorities,
                &g("priority"),
                cur.map(|i| &i.priority),
                "priority",
            )?,
        ),
        (
            "assigned_to_id",
            lookup(
                &meta.users,
                &g("assigned_to"),
                cur.and_then(|i| i.assigned_to.as_ref()),
                "assigned_to",
            )?,
        ),
        (
            "category_id",
            lookup(
                &meta.categories,
                &g("category"),
                cur.and_then(|i| i.category.as_ref()),
                "category",
            )?,
        ),
        (
            "fixed_version_id",
            lookup(
                &meta.versions,
                &g("fixed_version"),
                cur.and_then(|i| i.fixed_version.as_ref()),
                "fixed_version",
            )?,
        ),
        (
            "parent_issue_id",
            g("parent_issue_id").trim_start_matches('#').to_string(),
        ),
        ("start_date", g("start_date")),
        ("due_date", g("due_date")),
        ("done_ratio", g("done_ratio")),
        ("estimated_hours", num(&g("estimated_hours"))),
    ]);
    let mut cfs = BTreeMap::new();
    if let Some(Y::Mapping(cm)) = m.get("custom_fields") {
        for (k, v) in cm {
            let n = scalar(k);
            let id = cur
                .and_then(|i| i.custom_fields.iter().find(|c| c.name == n).map(|c| c.id))
                .or_else(|| {
                    meta.custom_fields
                        .iter()
                        .find(|c| c.name == n)
                        .map(|c| c.id)
                })
                .ok_or_else(|| anyhow!("custom_fields: '{n}' not found"))?;
            cfs.insert(id, cf_json(&serde_json::to_value(v)?));
        }
    }
    let updated_on = Some(g("updated_on")).filter(|s| !s.is_empty());
    Ok(Parsed {
        updated_on,
        payload: Payload { fields, cfs },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips an issue through render/parse using the given text values.
    fn check_roundtrip(subject: &str, desc: &str, cf: &str, status: &str, note: &str, no_fm: &str) {
        let i: Issue = serde_json::from_value(json!({
            "id": 7, "tracker": {"id": 1, "name": "Bug"}, "status": {"id": 1, "name": "New"},
            "priority": {"id": 2, "name": "Normal"}, "author": {"id": 1, "name": "Admin"},
            "subject": subject, "description": desc,
            "start_date": null, "done_ratio": 0, "estimated_hours": 1.5,
            "custom_fields": [{"id": 3, "name": cf, "value": "ACME"}, {"id": 4, "name": "OS", "value": ["Win", "Mac"]}],
            "created_on": "2026-11-01T10:00:00Z", "updated_on": "2026-11-20T09:00:00Z",
            "journals": [{"id": 10, "user": {"id": 1, "name": "Admin"}, "notes": note, "created_on": "2026-11-02T00:00:00Z"}]
        }))
        .unwrap();
        let meta = Meta {
            statuses: vec![
                IdName {
                    id: 1,
                    name: "New".into(),
                },
                IdName {
                    id: 2,
                    name: status.into(),
                },
            ],
            ..Default::default()
        };
        let base = payload_of(&i);

        let text = render_issue(&i, &meta);
        let p = parse_issue(&text, &meta, Some(&i)).unwrap();
        assert_eq!(p.updated_on.as_deref(), Some("2026-11-20T09:00:00Z"));
        assert!(p.payload.diff(Some(&base)).is_empty(), "{text}");

        let edited = text
            .replace("status: New", &format!("status: {status}"))
            .replace(&format!("{cf}: ACME"), &format!("{cf}: Foo"));
        let d = parse_issue(&edited, &meta, Some(&i))
            .unwrap()
            .payload
            .diff(Some(&base));
        assert_eq!(
            J::Object(d),
            json!({"status_id": "2", "custom_fields": [{"id": 3, "value": "Foo"}]})
        );

        assert!(parse_issue("---\nstatus: Unknown\n---\n", &meta, Some(&i)).is_err());
        assert_eq!(parse_comment(&render_comment(&i.journals[0])), note);
        assert_eq!(parse_comment(&format!("{no_fm}\n")), no_fm);
    }

    #[test]
    fn roundtrip() {
        check_roundtrip(
            "Login: fails",
            "Desc\r\nLine 2\r\n",
            "Customer",
            "In Progress",
            "Hello",
            "no front matter",
        );
    }

    #[test]
    fn roundtrip_multibyte() {
        check_roundtrip(
            "ログイン: できない",
            "説明\r\n2行目\r\n",
            "顧客名",
            "進行中",
            "こんにちは",
            "front matterなし",
        );
    }

    fn check_wiki_roundtrip(title: &str, parent: &str, body: &str, comment: &str) {
        let p: WikiPage = serde_json::from_value(json!({
            "title": title,
            "parent": { "title": parent },
            "text": body,
            "version": 4,
            "author": { "id": 1, "name": "Admin" },
            "comments": "old edit",
            "created_on": "2026-11-01T10:00:00Z",
            "updated_on": "2026-11-20T09:00:00Z"
        }))
        .unwrap();

        let rendered = render_wiki(&p);
        let parsed = parse_wiki(&rendered).unwrap();
        assert_eq!(parsed.parent, parent);
        assert_eq!(parsed.comments, "");
        assert_eq!(parsed.version, Some(4));
        assert_eq!(parsed.text, norm_text(body));

        let edited = rendered.replace("comments:", &format!("comments: {comment}"));
        let parsed2 = parse_wiki(&edited).unwrap();
        assert_eq!(parsed2.comments, comment);

        // plain markdown without front matter
        let plain = parse_wiki("# Heading\r\nBody\r\n").unwrap();
        assert_eq!(plain.parent, "");
        assert_eq!(plain.comments, "");
        assert_eq!(plain.version, None);
        assert_eq!(plain.text, "# Heading\nBody");

        // template parses cleanly
        let tpl = parse_wiki(&wiki_template()).unwrap();
        assert_eq!(tpl.parent, "");
        assert_eq!(tpl.comments, "");
        assert_eq!(tpl.version, None);
        assert_eq!(tpl.text, "");

        assert!(parse_wiki("---\nversion: abc\n---\nbody").is_err());
    }

    #[test]
    fn wiki_roundtrip() {
        check_wiki_roundtrip("Getting_Started", "Wiki", "Hello\r\nWorld\r\n", "Fix typo");
    }

    #[test]
    fn wiki_roundtrip_multibyte() {
        check_wiki_roundtrip("手順書", "ホーム", "見出し\r\n本文\r\n", "誤字修正");
    }
}
