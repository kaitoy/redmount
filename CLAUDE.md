# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

redmount mounts one Redmine project as a Windows drive via WinFsp, or as a directory via FUSE on Linux (fuser, pure-Rust mount via fusermount) and macOS (macFUSE) (Rust, edition 2024). See README.md for the user-facing file layout and write semantics.

## Commands

```powershell
cargo build                     # Windows: needs WinFsp 2.1+ and VS Build Tools (C++); macOS: macFUSE + pkgconf; Linux: nothing extra
cargo test                      # unit tests (markdown round-trip, fs name/time helpers)
cargo test roundtrip_multibyte  # single test by name
git config core.hooksPath .githooks  # once per clone: pre-commit runs cargo fmt --check + clippy -D warnings
$env:REDMINE_API_KEY = "..."; cargo run --release -- --url https://redmine.example.com --project myproj --mount R:
```

On Windows `build.rs` delay-loads `winfsp-x64.dll`; `win.rs` prepends `%ProgramFiles(x86)%\WinFsp\bin` to `PATH` before `winfsp_init`. Linux check from Windows: `docker run --rm -v "${PWD}:/src" -w /src -e CARGO_TARGET_DIR=/tmp/t rust:1 cargo test`. CI (`.github/workflows/build.yml`) builds and uploads binaries for all three OSes.

## Architecture

A core plus one adapter per OS:

- `src/redmine.rs`: blocking `reqwest` REST client (`Api`) + serde models (`Issue`, `Journal`, `Attachment`, `WikiIndexEntry`, `WikiPage`, `Meta`). `Meta` (trackers, statuses, priorities, users, categories, versions, custom fields, `wiki_enabled`) is fetched once at mount and used to map names to IDs. Restart to pick up new members or versions.
- `src/markdown.rs`: `issue.md` / `comment_N.md` / `page.md` text <-> Redmine payloads. YAML front matter holds editable fields; everything after `RO_MARK` is read-only and ignored on parse. `Payload::diff` sends only changed fields (or non-empty ones when creating). `updated_on` (issues) and `version` (wiki pages) from the file are used for conflict detection.
- `src/fs.rs`: OS-neutral core (`RedmineFs`). Public ops take `\`-separated path strings (`open`, `close`, `create`, `cleanup`, `check_delete`, `rename`, `read`, `write`, `set_size`, `read_dir`, `stat`) and return `fs::E` / `fs::Stat` (times in FILETIME units).
- `src/win.rs` (`cfg(windows)`): WinFsp `FileSystemContext` adapter (`WinFs`; `Handle` holds the path), volume params, security descriptor.
- `src/fuse.rs` (`cfg(unix)`): `fuse_mt::FilesystemMT` adapter; converts `/` to `\` and `E` to errno. `release` = `cleanup` + `close`, `unlink`/`rmdir` = `check_delete` + `cleanup(delete)`, opens use direct I/O. macOS mounts with `noappledouble` so Finder's `._*` files don't clutter issue folders.

Adapters only translate types; behavior changes belong in `fs.rs`.

### fs.rs model

- **Paths resolve to `Node`s** (`resolve`): `\<tracker>\<id subject>\{issue.md, comment_<journal id>.md, attachments\<name>}` and (when `meta.wiki_enabled`) `\_wiki\<page title>\{page.md, attachments\<name>}`. Matching is case-insensitive; issue folders resolve by leading ID (`leading_id`), so the subject part of the name is cosmetic. Tracker folders list open issues; `\<tracker>\_closed\` (`Node::Closed`) lists closed ones, and `_closed\<issue>\...` resolves exactly like `<issue>\...`.
- **Server data is cached** in `State.issues` (30s `TTL`), `State.lists` keyed by (tracker, closed), `State.wiki_list` (30s `TTL`), and `State.wiki_pages` (30s `TTL`). An issue list is fetched in full once (and every `FULL`), then refreshed after `TTL` by a project-wide `updated_on>=cursor` delta (`List::merge`); `invalidate` / `invalidate_wiki` drops cached entries and marks listings for refresh after a write. Attachment bodies (`blobs`) are cached until unmount.
- **`State.local` is an in-memory overlay** of `Entry`s keyed by lowercased path: `#<id>\<rest>` under existing issues, `wiki:<title>\<rest>` under existing wiki pages, `\<tracker>\<folder>\...` and `\_wiki\<folder>\...` for pending (not-yet-created) folders. It holds new/scratch files, error files, and edit buffers for `issue.md`/comments/`page.md`. Overlay entries shadow or add to server listings in `list_dir`.
  - `open` snapshots `issue.md`/comments into a *transient* entry so a handle sees consistent text; transient + clean entries are dropped on last `close`.
  - `dirty` = written but not successfully sent.
- **Writes go to Redmine on `cleanup`** (handle close: WinFsp Cleanup / FUSE release) via `commit` -> `send`, returning a `Done` that decides what happens to the overlay (`Updated`, `Consumed` = local file becomes server data and disappears, `Created` = pending folder becomes a real issue and is recorded in `aliases`). On failure the buffer stays dirty and `<name>.error.txt` is written next to it.
- **`rename` also commits**: editors' atomic save (temp file -> `issue.md`) and moving a file onto a meaningful name trigger an immediate send.
- One global `Mutex<State>` is held across HTTP calls (marked `ponytail:`, intentional).

## Conventions

- The plugin (`plugin.json`, `.claude-plugin/marketplace.json`, shipping `skills/`) is versioned separately from `Cargo.toml`. When `skills/` changes, bump `version` in both manifests together; Claude Code users only get updates on a version bump.
- Deliberate shortcuts are marked with `// ponytail:` comments naming the limit and upgrade path; keep that style.
- Errors to the kernel: map with `err(STATUS_*)` / `nf()` / `io()`; user-visible send errors go into `.error.txt`, not just stderr.
