use anyhow::{Context, Result, bail};
use reqwest::Method;
use reqwest::blocking::{Client, Response};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Deserialize, Clone, Debug, Default)]
pub struct IdName {
    pub id: u64,
    #[serde(default)]
    pub name: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct CustomValue {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub value: Value,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Journal {
    pub id: u64,
    pub user: IdName,
    pub notes: Option<String>,
    pub created_on: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Attachment {
    pub id: u64,
    pub filename: String,
    pub filesize: u64,
    pub content_url: String,
    pub created_on: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Issue {
    pub id: u64,
    pub tracker: IdName,
    pub status: IdName,
    pub priority: IdName,
    pub author: IdName,
    pub assigned_to: Option<IdName>,
    pub category: Option<IdName>,
    pub fixed_version: Option<IdName>,
    pub parent: Option<IdName>,
    pub subject: String,
    pub description: Option<String>,
    pub start_date: Option<String>,
    pub due_date: Option<String>,
    pub done_ratio: Option<i64>,
    pub estimated_hours: Option<f64>,
    #[serde(default)]
    pub custom_fields: Vec<CustomValue>,
    pub created_on: String,
    pub updated_on: String,
    #[serde(default)]
    pub journals: Vec<Journal>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

/// Name <-> id tables, loaded once at mount.
// ponytail: loaded once; restart to pick up new members/versions.
#[derive(Default, Debug)]
pub struct Meta {
    pub project_id: u64,
    pub trackers: Vec<IdName>,
    pub statuses: Vec<IdName>,
    pub priorities: Vec<IdName>,
    pub users: Vec<IdName>,
    pub categories: Vec<IdName>,
    pub versions: Vec<IdName>,
    pub custom_fields: Vec<IdName>,
}

pub struct Api {
    base: String,
    http: Client,
}

impl Api {
    pub fn new(url: &str, key: &str) -> Result<Self> {
        let mut h = HeaderMap::new();
        h.insert("X-Redmine-API-Key", HeaderValue::from_str(key)?);
        Ok(Self {
            base: url.trim_end_matches('/').to_string(),
            http: Client::builder().default_headers(h).build()?,
        })
    }

    fn check(r: Response) -> Result<Response> {
        let s = r.status();
        if s.is_success() {
            Ok(r)
        } else {
            let url = r.url().to_string();
            bail!("{url} {s} {}", r.text().unwrap_or_default())
        }
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let r = self.http.get(format!("{}{}", self.base, path)).send()?;
        Self::check(r)?.json().with_context(|| path.to_string())
    }

    /// Fetches every page (limit=100) of a paginated endpoint.
    fn paged<T, P: DeserializeOwned>(
        &self,
        path: &str,
        items: impl Fn(P) -> (Vec<T>, usize),
    ) -> Result<Vec<T>> {
        let mut out = Vec::new();
        loop {
            let sep = if path.contains('?') { '&' } else { '?' };
            let (mut page, total) = items(self.get(&format!("{path}{sep}limit=100&offset={}", out.len()))?);
            let empty = page.is_empty();
            out.append(&mut page);
            if empty || out.len() >= total {
                return Ok(out);
            }
        }
    }

    pub fn meta(&self, project: &str) -> Result<Meta> {
        #[derive(Deserialize)]
        struct Proj {
            id: u64,
            #[serde(default)]
            trackers: Vec<IdName>,
            #[serde(default)]
            issue_categories: Vec<IdName>,
            #[serde(default)]
            issue_custom_fields: Vec<IdName>,
        }
        #[derive(Deserialize)]
        struct P {
            project: Proj,
        }
        #[derive(Deserialize)]
        struct S {
            issue_statuses: Vec<IdName>,
        }
        #[derive(Deserialize)]
        struct Pr {
            issue_priorities: Vec<IdName>,
        }
        #[derive(Deserialize)]
        struct V {
            versions: Vec<IdName>,
        }
        #[derive(Deserialize)]
        struct M {
            user: Option<IdName>,
            group: Option<IdName>,
        }
        #[derive(Deserialize)]
        struct Ms {
            memberships: Vec<M>,
            total_count: usize,
        }

        let p: P = self.get(&format!(
            "/projects/{project}.json?include=trackers,issue_categories,issue_custom_fields"
        ))?;
        let users = self
            .paged(&format!("/projects/{project}/memberships.json"), |m: Ms| {
                (m.memberships, m.total_count)
            })?
            .into_iter()
            .filter_map(|m| m.user.or(m.group))
            .collect();
        Ok(Meta {
            project_id: p.project.id,
            trackers: p.project.trackers,
            categories: p.project.issue_categories,
            custom_fields: p.project.issue_custom_fields,
            statuses: self.get::<S>("/issue_statuses.json")?.issue_statuses,
            priorities: self.get::<Pr>("/enumerations/issue_priorities.json")?.issue_priorities,
            versions: self.get::<V>(&format!("/projects/{project}/versions.json"))?.versions,
            users,
        })
    }

    pub fn issues(&self, project_id: u64, tracker_id: u64) -> Result<Vec<Issue>> {
        #[derive(Deserialize)]
        struct L {
            issues: Vec<Issue>,
            total_count: usize,
        }
        self.paged(
            &format!("/issues.json?project_id={project_id}&tracker_id={tracker_id}&status_id=*&subproject_id=!*"),
            |l: L| (l.issues, l.total_count),
        )
    }

    /// `None` when the issue doesn't exist or isn't visible.
    pub fn issue(&self, id: u64) -> Result<Option<Issue>> {
        #[derive(Deserialize)]
        struct W {
            issue: Issue,
        }
        let r = self
            .http
            .get(format!("{}/issues/{id}.json?include=journals,attachments", self.base))
            .send()?;
        if matches!(r.status().as_u16(), 403 | 404) {
            return Ok(None);
        }
        Ok(Some(Self::check(r)?.json::<W>()?.issue))
    }

    pub fn send(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut rb = self.http.request(method, format!("{}{}", self.base, path));
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let text = Self::check(rb.send()?)?.text()?;
        Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text)? })
    }

    pub fn upload(&self, filename: &str, data: Vec<u8>) -> Result<String> {
        let r = self
            .http
            .post(format!("{}/uploads.json", self.base))
            .query(&[("filename", filename)])
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(data)
            .send()?;
        let v: Value = Self::check(r)?.json()?;
        v["upload"]["token"].as_str().map(String::from).context("upload token missing")
    }

    pub fn download(&self, url: &str) -> Result<Vec<u8>> {
        Ok(Self::check(self.http.get(url).send()?)?.bytes()?.to_vec())
    }
}
