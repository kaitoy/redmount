# redmount

Mounts a single Redmine project as a drive (Windows) or a directory (Linux/macOS) so you can read and write issues as files. Built on WinFsp on Windows and FUSE elsewhere.

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

The examples below use Windows paths; on Linux/macOS they are `~/redmine/Bug/123 Login fails/issue.md` and so on.

## Layout
```
R:\Bug\123 Login fails\issue.md          Fields (front matter) + description
R:\Bug\123 Login fails\comment_456.md    One file per comment
R:\Bug\123 Login fails\attachments\      Attachments
R:\Bug\_closed\98 Old crash\             Closed issues (the tracker folder lists open ones only)
```
Folders are resolved by their leading ID, so `R:\Bug\123` works too, whether or not the issue is listed there.

Listings are refreshed every 30 seconds by fetching only the issues updated since the last fetch. Deleted issues and issues moved to another project disappear from listings within 10 minutes.

## Writing
A file is sent to Redmine when it is saved and closed.

| Action | Result |
|---|---|
| Edit and save `issue.md` | Updates the issue (only changed fields are sent) |
| Edit and save `comment_N.md` | Edits the comment (Redmine 5.0+) |
| Create and save a new `*.md` in an issue folder | Adds a comment; the file is replaced by `comment_N.md` |
| Copy a file into `attachments\` | Attaches it |
| Create a new folder in a tracker folder, fill in the `issue.md` template inside, and save | Creates an issue (the folder name is used if subject is empty) |

- If sending fails (validation error, conflict, etc.), the reason is written to `<file name>.error.txt`. Your edits stay on the drive, so you can fix them and save again.
- Deleting an unsent `issue.md` or comment discards those edits.
- Non-`.md` files (editor temp files, etc.) are kept in memory only and never sent to Redmine. They disappear on unmount.
- Issues, comments and attachments cannot be deleted.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). The Windows build statically links [winfsp-rs](https://github.com/SnowflakePowered/winfsp-rs) (GPL-3.0).
