"""Benchmark scenario definitions and Redmine REST API verification checks."""

from __future__ import annotations

import json
import re
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Callable


class RedmineClient:
    """Minimal stdlib Redmine REST API client for post-run verification."""

    def __init__(self, base_url: str, api_key: str) -> None:
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key

    def get(self, path: str, params: dict[str, str] | None = None) -> dict:
        url = f"{self.base_url}{path}"
        if params:
            url += "?" + urllib.parse.urlencode(params)
        req = urllib.request.Request(
            url,
            headers={
                "X-Redmine-API-Key": self.api_key,
                "Accept": "application/json",
            },
        )
        with urllib.request.urlopen(req, timeout=15) as resp:
            return json.loads(resp.read().decode("utf-8"))

    def get_issue(self, issue_id: int, include: str = "") -> dict:
        params = {"include": include} if include else None
        return self.get(f"/issues/{issue_id}.json", params)["issue"]

    def list_issues(self, project_id: str = "demo", status_id: str = "*", limit: int = 100) -> list[dict]:
        return self.get(
            "/issues.json",
            {"project_id": project_id, "status_id": status_id, "limit": str(limit)},
        )["issues"]

    def get_wiki_page(self, project_id: str, title: str) -> dict:
        return self.get(f"/projects/{project_id}/wiki/{urllib.parse.quote(title)}.json")["wiki_page"]


@dataclass
class Scenario:
    id: str
    name: str
    category: str
    task_prompt: str
    verify: Callable[[RedmineClient, str], tuple[bool, str]]


def _verify_s1(_api: RedmineClient, answer: str) -> tuple[bool, str]:
    """Expected Open Bug + priority=High: IDs 1, 2, 3, 4 (and NOT 9, 15, 17, 24, 31, 32)."""
    expected = {1, 2, 3, 4}
    forbidden = {9, 15, 17, 24, 31, 32}
    # Ignore explanatory lines noting excluded/closed issues
    relevant_lines = [
        ln for ln in answer.splitlines()
        if not re.search(r"\b(exclud|left out|omitted|ignored|skipped|not included)\b", ln, re.I)
    ]
    cleaned = "\n".join(relevant_lines)
    found = {int(m) for m in re.findall(r"(?:#|\bID[:\s]*|\b)(\d{1,2})\b", cleaned)}
    missing = expected - found
    extra = forbidden & found
    if missing:
        return False, f"missing expected IDs: {sorted(missing)}"
    if extra:
        return False, f"included non-matching IDs (closed/non-Bug/non-High): {sorted(extra)}"
    for name in ("Alice", "Bob", "Carol"):
        if name.lower() not in cleaned.lower():
            return False, f"missing assignee name: {name}"
    return True, "matched IDs {1,2,3,4} and assignees"


def _verify_s2(_api: RedmineClient, answer: str) -> tuple[bool, str]:
    """Expected ID 14 and root cause mentioning connection pool / unclosed transaction / BillingWorker."""
    if not re.search(r"(?:#|\b)14\b", answer):
        return False, "did not identify issue #14"
    low = answer.lower()
    if not any(k in low for k in ("connection pool", "unclosed transaction", "billingworker", "コネクションプール", "トランザクション")):
        return False, "did not explain root cause from comment"
    return True, "identified #14 and root cause"


def _verify_s3(api: RedmineClient, _answer: str) -> tuple[bool, str]:
    """Issue #7 must be In Progress, done_ratio=50, assigned_to=Alice Tanaka."""
    iss = api.get_issue(7)
    status = iss.get("status", {}).get("name")
    done = iss.get("done_ratio")
    assignee = iss.get("assigned_to", {}).get("name")
    errs = []
    if status != "In Progress":
        errs.append(f"status={status!r} (expected 'In Progress')")
    if done != 50:
        errs.append(f"done_ratio={done!r} (expected 50)")
    if assignee != "Alice Tanaka":
        errs.append(f"assigned_to={assignee!r} (expected 'Alice Tanaka')")
    if errs:
        return False, "; ".join(errs)
    return True, "issue #7 updated accurately"


def _verify_s4(api: RedmineClient, _answer: str) -> tuple[bool, str]:
    """Issue #10 must have a new 4th comment mentioning 8443 and 30s."""
    iss = api.get_issue(10, include="journals")
    journals = [j for j in iss.get("journals", []) if (j.get("notes") or "").strip()]
    if len(journals) < 4:
        return False, f"expected >=4 comments on #10, got {len(journals)}"
    last_note = journals[-1]["notes"]
    if "8443" not in last_note or "30" not in last_note:
        return False, f"last comment missing '8443' or '30': {last_note!r}"
    return True, f"added comment #{journals[-1]['id']} on #10"


def _verify_s5(api: RedmineClient, answer: str) -> tuple[bool, str]:
    """New Feature issue created with expected fields and its ID reported in answer."""
    issues = api.list_issues()
    created = [
        i for i in issues
        if i["id"] > 40 and "CSV export" in i.get("subject", "")
    ]
    if not created:
        return False, "no new issue with 'CSV export' in subject found (id > 40)"
    iss = created[0]
    errs = []
    if iss.get("tracker", {}).get("name") != "Feature":
        errs.append(f"tracker={iss.get('tracker', {}).get('name')!r}")
    if iss.get("priority", {}).get("name") != "High":
        errs.append(f"priority={iss.get('priority', {}).get('name')!r}")
    if iss.get("fixed_version", {}).get("name") != "v1.1":
        errs.append(f"fixed_version={iss.get('fixed_version', {}).get('name')!r}")
    if iss.get("assigned_to", {}).get("name") != "Bob Sato":
        errs.append(f"assigned_to={iss.get('assigned_to', {}).get('name')!r}")
    if str(iss["id"]) not in answer:
        errs.append(f"answer did not report created ID #{iss['id']}")
    if errs:
        return False, "; ".join(errs)
    return True, f"created Feature #{iss['id']} with all fields"


def _verify_s6(api: RedmineClient, _answer: str) -> tuple[bool, str]:
    """Open Bug + v1.0 (IDs 2, 5, 8) all set to Feedback + comment with 'v1.0 RC2'.
    Non-targets (#18 Feature v1.0, #33 Closed Bug v1.0) must remain unchanged."""
    target_ids = (2, 5, 8)
    errs = []
    for iid in target_ids:
        iss = api.get_issue(iid, include="journals")
        if iss.get("status", {}).get("name") != "Feedback":
            errs.append(f"#{iid} status={iss.get('status', {}).get('name')!r} (expected Feedback)")
        notes = [j.get("notes", "") for j in iss.get("journals", [])]
        if not any("v1.0 RC2" in n for n in notes):
            errs.append(f"#{iid} missing comment containing 'v1.0 RC2'")

    # Guard against over-updating non-targets
    iss18 = api.get_issue(18)
    if iss18.get("status", {}).get("name") == "Feedback":
        errs.append("#18 (Feature, not Bug) was mistakenly updated to Feedback")
    iss33 = api.get_issue(33)
    if iss33.get("status", {}).get("name") != "Closed":
        errs.append("#33 (Closed Bug) was mistakenly modified")

    if errs:
        return False, "; ".join(errs)
    return True, "updated #2, #5, #8 without touching #18/#33"


def _verify_s7(api: RedmineClient, _answer: str) -> tuple[bool, str]:
    """Wiki page 'Architecture' must preserve existing content and append '## Caching Strategy'."""
    page = api.get_wiki_page("demo", "Architecture")
    text = page.get("text", "")
    errs = []
    if "## Overview" not in text or "## Data Flow" not in text:
        errs.append("existing sections were lost")
    if "## Caching Strategy" not in text:
        errs.append("missing '## Caching Strategy' heading")
    if "300" not in text and "TTL" not in text:
        errs.append("missing caching TTL details")
    if errs:
        return False, "; ".join(errs)
    return True, f"updated Architecture wiki (v{page.get('version')})"


SCENARIOS: list[Scenario] = [
    Scenario(
        id="S1",
        name="Filtered Search",
        category="Search",
        task_prompt=(
            "Find all OPEN issues in project `demo` with tracker `Bug` and priority `High`. "
            "List ONLY the matching issues (ID, subject, and assignee name). Do not list or mention excluded/closed issues."
        ),
        verify=_verify_s1,
    ),
    Scenario(
        id="S2",
        name="Full-text Cross Search",
        category="Search",
        task_prompt=(
            "Search project `demo` (including issue descriptions and comments) for the error code `ERR-4092`. "
            "Report the issue ID where it appears and summarize the root cause described there."
        ),
        verify=_verify_s2,
    ),
    Scenario(
        id="S3",
        name="Single Issue Update",
        category="Edit",
        task_prompt=(
            "Inspect issue #7 in project `demo`, then update it as follows: "
            "set status to `In Progress`, done ratio to `50`, and assignee to `Alice Tanaka`. "
            "Confirm once saved."
        ),
        verify=_verify_s3,
    ),
    Scenario(
        id="S4",
        name="Read Comments & Reply",
        category="Comment",
        task_prompt=(
            "Read issue #10 and its existing comments in project `demo`. "
            "Add a new comment replying to the latest question with the following staging parameters: "
            "TLS port is `8443` and handshake timeout is `30s`."
        ),
        verify=_verify_s4,
    ),
    Scenario(
        id="S5",
        name="Create New Issue",
        category="Create",
        task_prompt=(
            "Create a new `Feature` issue in project `demo` with:\n"
            "- Subject: `Add CSV export for monthly report`\n"
            "- Description: `Allow users to export the monthly summary table as a UTF-8 CSV file.`\n"
            "- Priority: `High`\n"
            "- Target version: `v1.1`\n"
            "- Assignee: `Bob Sato`\n"
            "Report the newly created issue ID."
        ),
        verify=_verify_s5,
    ),
    Scenario(
        id="S6",
        name="Batch Update (3 Issues)",
        category="Batch",
        task_prompt=(
            "Find all OPEN `Bug` issues in project `demo` assigned to target version `v1.0` "
            "(do not modify closed issues or non-Bug trackers). "
            "For each of them, change the status to `Feedback` and add a comment: `Please verify in v1.0 RC2.`"
        ),
        verify=_verify_s6,
    ),
    Scenario(
        id="S7",
        name="Wiki Read & Append",
        category="Wiki",
        task_prompt=(
            "Read the `Architecture` wiki page in project `demo`, and append the following section to the end "
            "while preserving all existing content:\n\n"
            "## Caching Strategy\n"
            "API responses are cached in Redis with a 300-second TTL and invalidated on write.\n"
        ),
        verify=_verify_s7,
    ),
]
