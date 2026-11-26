# redmount vs Redmine MCP Server Benchmark

Reproducible benchmark comparing **redmount** (filesystem mount + Agent Skill) against **Redmine MCP servers** (default: `@onozaty/redmine-mcp-server`) using `claude -p --output-format stream-json`.

## Prerequisites

- Docker Desktop (`docker compose`)
- Rust toolchain + WinFsp (Windows) / FUSE (Linux/macOS) with `cargo build --release`
- Node.js / `npx` (for `@onozaty/redmine-mcp-server`)
- Python 3.10+ (stdlib only, no `pip install` needed)
- Claude Code CLI (`claude`) authenticated

## Quick Start

```powershell
# 1. Build release binary
cargo build --release

# 2. Smoke test (2 scenarios, 1 trial)
python bench/run.py --scenarios S1,S3 --trials 1

# 3. Full benchmark (all 7 scenarios, 3 trials)
python bench/run.py --trials 3
```

## Options

| Flag | Default | Description |
|---|---|---|
| `--modes` | `redmount,mcp-onozaty` | Comma-separated modes to compare |
| `--scenarios` | all (`S1`–`S7`) | Comma-separated scenario IDs |
| `--trials` | `3` | Trials per (mode, scenario) |
| `--model` | `sonnet` | Claude model alias or name |
| `--effort` | `medium` | Reasoning effort (`low`, `medium`, `high`) |
| `--mount` | `R:` | Drive letter (Windows) or directory (Unix) for redmount |
| `--redmount-bin` | `target/release/redmount(.exe)` | Path to redmount binary |
| `--skip-seed` | `false` | Skip DB reset (debugging only) |

## Outputs

- `bench/results/report.md`: Human-readable comparison tables (latency, tokens, cost, turns, tool calls, pass rate)
- `bench/results/summary.json`: Structured metrics for further analysis
- `bench/results/raw/` (gitignored): Full `stream-json` logs per run
