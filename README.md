# redmount

Mounts a single Redmine project as a drive (Windows) or a directory (Linux/macOS) so you can read and write issues and wiki pages as files. Built on WinFsp on Windows and FUSE elsewhere.

## Requirements
- Windows: [WinFsp](https://winfsp.dev/) 2.1 or later; to build, Rust (MSVC toolchain) and Visual Studio Build Tools ("Desktop development with C++")
- Linux: FUSE (`fusermount3` or `fusermount`, e.g. the `fuse3` package); to build, just Rust
- macOS: [macFUSE](https://macfuse.github.io/); to build, Rust and `pkgconf`
- Redmine: enable the REST API and look up your API key on the "My account" page

Prebuilt binaries for all three are produced by the GitHub Actions `build` workflow (artifacts).

## Usage
```powershell
$env:REDMINE_API_KEY = "xxxxxxxx"
cargo run --release -- --url https://redmine.example.com --project myproj --mount R:
```
```sh
export REDMINE_API_KEY=xxxxxxxx
mkdir -p ~/redmine
cargo run --release -- --url https://redmine.example.com --project myproj --mount ~/redmine
```
Press Enter to unmount. If the process died, unmount with `fusermount -u ~/redmine` (Linux) or `umount ~/redmine` (macOS).

`--filter "assigned_to_id=me&fixed_version_id=3"` limits the folder listings to issues matching a Redmine `/issues.json` query string (`tracker_id`, `status_id` and `query_id` are not usable). Issues outside the filter are still reachable by typing their path, and one that stops matching leaves the listing within 10 minutes.

The examples below use Windows paths; on Linux/macOS they are `~/redmine/Bug/123 Login fails/issue.md` and so on.

## Layout
```
R:\Bug\123 Login fails\issue.md          Fields (front matter) + description
R:\Bug\123 Login fails\comment_456.md    One file per comment
R:\Bug\123 Login fails\attachments\      Attachments
R:\Bug\_closed\98 Old crash\             Closed issues (the tracker folder lists open ones only)
R:\_wiki\Wiki\page.md                    Wiki page (front matter + body, when wiki module is enabled)
R:\_wiki\Wiki\attachments\               Wiki page attachments
```
Folders are resolved by their leading ID, so `R:\Bug\123` works too, whether or not the issue is listed there.

Listings are refreshed every 30 seconds by fetching only the issues updated since the last fetch. Deleted issues and issues moved to another project disappear from listings within 10 minutes.

## Writing
A file is sent to Redmine when it is saved and closed. Editors that save by writing a temp file and renaming it over the target send on the rename. Sending is synchronous, so the save does not finish until Redmine responds. A program that keeps the file open sends nothing until it closes it.

| Action | Result |
|---|---|
| Edit and save `issue.md` | Updates the issue (only changed fields are sent) |
| Edit and save `comment_N.md` | Edits the comment (Redmine 5.0+) |
| Create and save `comment.md` in an issue folder | Adds a comment; the file is replaced by `comment_N.md` |
| Copy a file into `attachments\` | Attaches it |
| Create a new folder in a tracker folder, fill in the `issue.md` template inside, and save | Creates an issue (the folder name is used if subject is empty) |
| Edit and save `page.md` in `_wiki\<Title>\` | Updates the wiki page (`parent`, `comments`, body) |
| Create a new folder in `_wiki\`, fill in `page.md` inside, and save | Creates a wiki page named after the folder |
| Copy a file into `_wiki\<Title>\attachments\` | Attaches it to the wiki page |

- If sending fails (validation error, conflict, etc.), the reason is written to `<file name>.error.txt`. Your edits stay on the drive, so you can fix them and save again.
- Deleting an unsent `issue.md`, `page.md`, or comment discards those edits.
- Other files (editor temp files, `.md` files with other names, etc.) are kept in memory only and never sent to Redmine. They disappear on unmount.
- Issues, comments, wiki pages, and attachments cannot be deleted.

## Agent Skill
[`skills/redmount/`](skills/redmount/SKILL.md) is an [Agent Skill](https://agentskills.io/) that teaches AI agents the rules above. This repository is also a plugin and a plugin marketplace that ship it:

- Claude Code: `/plugin marketplace add kaitoy/redmount`, then `/plugin install redmount@redmount`
- GitHub Copilot CLI: `copilot plugin install kaitoy/redmount`
- VS Code: run **Chat: Install Plugin from Source** and enter `https://github.com/kaitoy/redmount`
- Other agents: copy `skills/redmount/` to the agent's skills directory

Then tell the agent where the drive is mounted.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). The Windows build statically links [winfsp-rs](https://github.com/SnowflakePowered/winfsp-rs) (GPL-3.0).
