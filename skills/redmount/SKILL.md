---
name: redmount
description: Use when reading, searching, creating, updating, closing or commenting on Redmine issues through a redmount-mounted drive or directory (e.g. `R:\`, `~/redmine`), where each issue is a folder holding `issue.md` and `comment_N.md` files.
---

# Operating Redmine through redmount

redmount mounts one Redmine project as files. Reading a file fetches from Redmine; saving and closing a file with a meaningful name sends it to Redmine. Use your normal file tools (read, write, edit, glob, grep); no Redmine API calls are needed.

If you don't know the mount path, ask the user. Don't start or stop redmount yourself.

## Layout

```
<mount>\<tracker>\<id> <subject>\issue.md           Fields (front matter) + description
<mount>\<tracker>\<id> <subject>\comment_<N>.md     One file per existing comment
<mount>\<tracker>\<id> <subject>\attachments\       Attachments
<mount>\<tracker>\_closed\<id> <subject>\           Closed issues
```

- A tracker folder lists open issues only; closed ones are under `_closed`.
- Folders resolve by their leading ID: `<mount>\<tracker>\123\issue.md` reaches issue #123 even if it isn't listed (e.g. hidden by the user's `--filter`, or in another tracker folder). The subject part of the folder name is cosmetic.
- Names are case-insensitive. Linux/macOS use `/` instead of `\`.
- Listings refresh about every 30 seconds. To search, glob/grep over the mount, but prefer narrowing to a tracker folder: every listed issue is fetched from Redmine.

## issue.md format

```markdown
---
subject: Login fails
status: New
priority: Normal
assigned_to: Jane Doe
category:
fixed_version: v1.2
parent_issue_id:
start_date: 2026-01-10
due_date:
done_ratio: 0
estimated_hours:
custom_fields:
  Severity: High
# --- Read-only below (edits are ignored) ---
id: 123
tracker: Bug
author: John Smith
created_on: ...
updated_on: 2026-01-10T09:00:00Z
---
Description in Markdown/Textile, as Redmine stores it.
```

- Only the fields above the read-only marker and the description (body) are sent. The tracker cannot be changed.
- `status`, `priority`, `assigned_to`, `category`, `fixed_version` take the **display names** used in Redmine (case-insensitive) or numeric IDs. An unknown name is an error. Look at other issues' `issue.md` to learn valid names.
- An empty value clears the field. Dates are `YYYY-MM-DD`. Custom field names must match exactly (case-sensitive); multi-value custom fields take a YAML list.
- `updated_on` is used for conflict detection. Leave it as you read it. See the conflict rules below.

## Operations

| Goal | Do this |
|---|---|
| Update an issue | Read `issue.md`, edit it in place, save. Only changed fields are sent. |
| Close / change status | Set `status:` to the target status. A closed issue moves under `_closed`. |
| Add a comment | Write a new file `comment.md` in the issue folder with the comment text (no front matter needed). After sending it is replaced by `comment_<N>.md`. |
| Edit a comment | Edit the body of `comment_<N>.md` (Redmine 5.0+). |
| Attach a file | Copy the file into the issue's `attachments\` folder. |
| Create an issue | Create a new folder in the tracker folder (e.g. `<mount>\Bug\new-login-bug`), read the `issue.md` template that appears inside, fill it in, save. If `subject` is empty the folder name is used. The folder path keeps working afterwards; re-read its `issue.md` to get the new `id`. |

Write each file once with the full content. Every save of `issue.md` or `comment.md` sends to Redmine, so don't build them up with several partial writes.

## After every write: check for errors

Saving blocks until Redmine answers. Then check whether `<file name>.error.txt` exists next to the file you wrote (e.g. `issue.md.error.txt`, `comment.md.error.txt`):

- **No error file**: sent. Re-read the file to confirm the result if it matters.
- **Error file exists**: read it, fix the cause in the same file and save again. Your edits stay on the drive until they are sent; the error file disappears on success.
- **Conflict** (someone else updated the issue while you were editing): the error message tells you how to overwrite by changing `updated_on`. **Do not follow that instruction on your own.** Overwriting silently discards the other person's changes. Instead, either:
  1. Re-apply: delete your `issue.md` to discard your unsent edits, re-read it, check what the other person changed, and make your change again on the fresh text; or
  2. Ask the user, showing what conflicts, and change `updated_on` only if they explicitly tell you to overwrite.

## Don't

- Don't create scratch or note files in the mount expecting them to be saved: any file other than `issue.md`, `comment.md`, `comment_<N>.md` and attachments is kept in memory only and lost on unmount.
- Don't try to delete issues, comments or attachments; it isn't supported. Deleting an `issue.md` or comment with unsent edits just discards those edits.
- Don't edit the read-only section; it is ignored. The one exception is changing `updated_on` to overwrite after a conflict, and only when the user explicitly asks for it.
- Don't edit `issue.md` from a stale copy. Read it right before editing so `updated_on` is current.
