# Contributing

## Build and test
See [README.md](README.md#requirements) for the per-OS build requirements.
```sh
cargo build
cargo test
git config core.hooksPath .githooks   # once per clone: pre-commit runs cargo fmt --check + clippy -D warnings
```

## Local Redmine
`compose.yaml` runs a throwaway Redmine (SQLite) on http://localhost:8080:
```sh
docker compose up -d
```
Every start runs `dev/redmine-init.rb`, which (idempotently):
- loads the default data (trackers Bug/Feature/Support, statuses, priorities)
- enables the REST API
- creates the project `demo`
- sets the `admin` user's API key to `redmount0dev0api0key000000000000000000000`

The web UI login is `admin` / `admin`. The first start takes a little while (DB migration); `docker compose logs -f` shows when Puma is listening.

## Mount it
Windows:
```powershell
$env:REDMINE_API_KEY = "redmount0dev0api0key000000000000000000000"
cargo run -- --url http://localhost:8080 --project demo --mount R:
```
Linux/macOS:
```sh
export REDMINE_API_KEY=redmount0dev0api0key000000000000000000000
mkdir -p ~/redmine
cargo run -- --url http://localhost:8080 --project demo --mount ~/redmine
```
`R:\Bug\`, `R:\Feature\` and `R:\Support\` appear; create an issue by making a folder under one of them and saving an `issue.md` in it (see [README.md](README.md#writing)). Press Enter to unmount.

## Reset
```sh
docker compose down -v   # removes the container and all Redmine data
```
