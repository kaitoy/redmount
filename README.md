# redmount

Mounts a single Redmine project as a Windows drive so you can read and write issues as files. Built on WinFsp.

## Requirements
- [WinFsp](https://winfsp.dev/) 2.1 or later
- Rust (MSVC toolchain) and Visual Studio Build Tools ("Desktop development with C++")
- Redmine: enable the REST API and look up your API key on the "My account" page

## Usage
```powershell
$env:REDMINE_API_KEY = "xxxxxxxx"
cargo run --release -- --url https://redmine.example.com --project myproj --mount R:
```
Press Enter to unmount.

## Layout
```
R:\Bug\123 Login fails\issue.md          Fields (front matter) + description
R:\Bug\123 Login fails\comment_456.md    One file per comment
R:\Bug\123 Login fails\attachments\      Attachments
```
Folders are resolved by their leading ID, so `R:\Bug\123` works too.

## Writing
A file is sent to Redmine when it is saved and closed.

| Action | Result |
|---|---|
| Edit and save `issue.md` | Updates the issue (only changed fields are sent) |
| Edit and save `comment_N.md` | Edits the comment (Redmine 5.0+) |
| Create and save a new `*.md` in an issue folder | Adds a comment; the file is replaced by `comment_N.md` |
| Copy a file into `attachments\` | Attaches it |
| Delete a file in `attachments\` | Deletes the attachment |
| Create a new folder in a tracker folder, fill in the `issue.md` template inside, and save | Creates an issue (the folder name is used if subject is empty) |

- If sending fails (validation error, conflict, etc.), the reason is written to `<file name>.error.txt`. Your edits stay on the drive, so you can fix them and save again.
- Deleting an unsent `issue.md` or comment discards those edits.
- Non-`.md` files (editor temp files, etc.) are kept in memory only and never sent to Redmine. They disappear on unmount.
- Issues and comments cannot be deleted.
