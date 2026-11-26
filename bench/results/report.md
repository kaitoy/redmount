# redmount vs Redmine MCP Server Benchmark Report

- **Generated**: 2026-11-26 13:33 UTC
- **Model**: `sonnet` (effort: `medium`)
- **Modes**: `redmount`, `mcp-onozaty`
- **Trials per scenario**: 3
- **Dataset**: Local Docker Redmine 6 (`demo` project: 40 issues — 30 Open / 10 Closed, ~17 comments, 4 wiki pages)

## Per-Scenario Comparison (Median across trials)

| Scenario | Mode | Pass | Wall Time | API Time | Turns | Tools | Input+Cache (Total) | Cache Create | Output | Cost (USD) |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **S1** Filtered Search | `redmount` | 3/3 | 6.22s | 3.75s | 3.0 | 2.0 | 65,906 | 2,674 | 377 | $0.0206 |
| **S1** Filtered Search | `mcp-onozaty` | 3/3 | 10.21s | 7.03s | 5.0 | 4.0 | 90,949 | 4,998 | 587 | $0.0345 |
| **S2** Full-text Cross Search | `redmount` | 3/3 | 8.34s | 5.93s | 3.0 | 2.0 | 64,011 | 213 | 389 | $0.0113 |
| **S2** Full-text Cross Search | `mcp-onozaty` | 3/3 | 8.48s | 5.81s | 4.0 | 3.0 | 84,208 | 1,301 | 571 | $0.0194 |
| **S3** Single Issue Update | `redmount` | 3/3 | 14.96s | 12.10s | 10.0 | 9.0 | 157,166 | 4,360 | 1,189 | $0.0380 |
| **S3** Single Issue Update | `mcp-onozaty` | 3/3 | 8.52s | 5.80s | 6.0 | 5.0 | 88,747 | 3,522 | 671 | $0.0293 |
| **S4** Read Comments & Reply | `redmount` | 3/3 | 13.71s | 9.72s | 11.0 | 10.0 | 154,001 | 1,909 | 828 | $0.0324 |
| **S4** Read Comments & Reply | `mcp-onozaty` | 3/3 | 8.23s | 5.66s | 4.0 | 3.0 | 85,614 | 2,347 | 451 | $0.0221 |
| **S5** Create New Issue | `redmount` | 3/3 | 7.46s | 5.02s | 4.0 | 3.0 | 64,527 | 801 | 557 | $0.0154 |
| **S5** Create New Issue | `mcp-onozaty` | 3/3 | 12.46s | 9.88s | 10.0 | 9.0 | 165,599 | 5,494 | 1,112 | $0.0495 |
| **S6** Batch Update (3 Issues) | `redmount` | 3/3 | 19.48s | 15.60s | 14.0 | 13.0 | 167,014 | 6,145 | 1,932 | $0.0601 |
| **S6** Batch Update (3 Issues) | `mcp-onozaty` | 3/3 | 11.59s | 8.81s | 8.0 | 7.0 | 121,089 | 16,891 | 1,106 | $0.0889 |
| **S7** Wiki Read & Append | `redmount` | 3/3 | 7.41s | 6.00s | 6.0 | 5.0 | 86,794 | 1,242 | 599 | $0.0198 |
| **S7** Wiki Read & Append | `mcp-onozaty` | 3/3 | 12.78s | 9.64s | 7.0 | 6.0 | 151,902 | 2,231 | 974 | $0.0371 |

## Aggregate Summary (Sum of Scenario Medians)

| Mode | Pass Rate | Total Wall Time | Total API Time | Total Turns | Total Tool Calls | Total Input+Cache | Total Cache Create | Total Output | Total Cost (USD) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `redmount` | 100.0% (21/21) | 77.58s | 58.12s | 51.0 | 44.0 | 759,419 | 17,344 | 5,871 | $0.1977 |
| `mcp-onozaty` | 100.0% (21/21) | 72.27s | 52.64s | 44.0 | 37.0 | 788,108 | 36,784 | 5,472 | $0.2809 |

**Ratio (`redmount` / `mcp-onozaty`)**: Wall Time `1.07x`, Turns `1.16x`, Tools `1.19x`, Total Context `0.96x`, Output `1.07x`, Cost `0.70x`.

## Tool Usage Breakdown (All Trials Combined)

### `redmount`
- `Read`: 40
- `Glob`: 28
- `Edit`: 17
- `Write`: 15
- `Bash`: 12
- `Grep`: 10
- `PowerShell`: 4

### `mcp-onozaty`
- `ToolSearch`: 30
- `mcp__redmine__updateIssue`: 15
- `mcp__redmine__getIssue`: 10
- `mcp__redmine__getTrackers`: 9
- `mcp__redmine__getProjects`: 9
- `mcp__redmine__getIssuePriorities`: 6
- `mcp__redmine__getIssues`: 6
- `mcp__redmine__getIssueStatuses`: 6
- `mcp__redmine__getUsers`: 6
- `mcp__redmine__search`: 3
- `mcp__redmine__getVersionsByProject`: 3
- `mcp__redmine__createIssue`: 3
- `mcp__redmine__getWikiPage`: 3
- `mcp__redmine__updateWikiPage`: 3
