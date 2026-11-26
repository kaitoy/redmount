#!/usr/bin/env python3
"""Benchmark harness comparing redmount vs Redmine MCP server via Claude Code CLI."""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from collections import Counter
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path

from scenarios import SCENARIOS, RedmineClient, Scenario

REPO_ROOT = Path(__file__).resolve().parent.parent
BENCH_DIR = Path(__file__).resolve().parent
SEED_RB = BENCH_DIR / "seed.rb"
SKILL_MD = REPO_ROOT / "skills" / "redmount" / "SKILL.md"
WORKDIR = BENCH_DIR / ".workdir"
RESULTS_DIR = BENCH_DIR / "results"
RAW_DIR = RESULTS_DIR / "raw"

DEFAULT_URL = "http://localhost:8080"
DEFAULT_API_KEY = "redmount0dev0api0key000000000000000000000"
DEFAULT_PROJECT = "demo"


@dataclass
class RunResult:
    mode: str
    scenario_id: str
    scenario_name: str
    trial: int
    model: str
    effort: str
    passed: bool
    verify_detail: str
    is_error: bool
    exit_code: int
    wall_time_ms: int
    duration_ms: int
    duration_api_ms: int
    num_turns: int
    tool_calls_total: int
    tool_calls_by_name: dict[str, int] = field(default_factory=dict)
    input_tokens: int = 0
    cache_creation_tokens: int = 0
    cache_read_tokens: int = 0
    output_tokens: int = 0
    total_cost_usd: float = 0.0
    raw_log_path: str = ""
    answer_preview: str = ""

    @property
    def total_input_context(self) -> int:
        return self.input_tokens + self.cache_creation_tokens + self.cache_read_tokens


def wait_for_redmine(base_url: str, api_key: str, timeout_s: int = 120) -> None:
    print(f"[setup] Waiting for Redmine at {base_url} ...", flush=True)
    deadline = time.monotonic() + timeout_s
    url = f"{base_url.rstrip('/')}/projects/{DEFAULT_PROJECT}.json"
    last_err = ""
    while time.monotonic() < deadline:
        try:
            req = urllib.request.Request(url, headers={"X-Redmine-API-Key": api_key})
            with urllib.request.urlopen(req, timeout=5) as resp:
                if resp.status == 200:
                    print("[setup] Redmine is ready.", flush=True)
                    return
        except Exception as e:
            last_err = str(e)
        time.sleep(2)
    raise RuntimeError(f"Redmine did not become ready within {timeout_s}s: {last_err}")


def ensure_docker_redmine(base_url: str, api_key: str) -> None:
    subprocess.run(
        ["docker", "compose", "up", "-d"],
        cwd=REPO_ROOT,
        check=True,
    )
    wait_for_redmine(base_url, api_key)


def seed_redmine() -> None:
    subprocess.run(
        ["docker", "compose", "cp", str(SEED_RB), "redmine:/tmp/bench-seed.rb"],
        cwd=REPO_ROOT,
        check=True,
        stdout=subprocess.DEVNULL,
    )
    res = subprocess.run(
        ["docker", "compose", "exec", "-T", "redmine", "rails", "runner", "/tmp/bench-seed.rb"],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    line = (res.stdout or "").strip().splitlines()
    if line:
        print(f"[seed] {line[-1]}", flush=True)


class RedmountProcess:
    """Manages a redmount subprocess and clean unmount."""

    def __init__(self, binary: Path, url: str, api_key: str, project: str, mount: str) -> None:
        self.binary = binary
        self.url = url
        self.api_key = api_key
        self.project = project
        self.mount = mount
        self.proc: subprocess.Popen[bytes] | None = None

    def _mount_probe(self) -> Path:
        if platform.system() == "Windows":
            m = self.mount.rstrip("\\")
            return Path(f"{m}\\Bug")
        return Path(self.mount) / "Bug"

    def start(self, timeout_s: int = 20) -> None:
        if not self.binary.exists():
            raise FileNotFoundError(
                f"redmount binary not found at {self.binary}. Run `cargo build --release` first."
            )
        if platform.system() != "Windows":
            Path(self.mount).mkdir(parents=True, exist_ok=True)
        env = os.environ.copy()
        env["REDMINE_API_KEY"] = self.api_key
        self.proc = subprocess.Popen(
            [
                str(self.binary),
                "--url", self.url,
                "--project", self.project,
                "--mount", self.mount,
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            env=env,
        )
        probe = self._mount_probe()
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                out = (self.proc.stdout.read() if self.proc.stdout else b"").decode("utf-8", errors="replace")
                raise RuntimeError(f"redmount exited prematurely (code {self.proc.returncode}):\n{out}")
            if probe.exists():
                return
            time.sleep(0.25)
        self.stop()
        raise RuntimeError(f"redmount mount point {probe} did not appear within {timeout_s}s")

    def stop(self) -> None:
        if self.proc is None:
            return
        try:
            if self.proc.poll() is None:
                if self.proc.stdin:
                    try:
                        self.proc.stdin.write(b"\n")
                        self.proc.stdin.flush()
                        self.proc.stdin.close()
                    except OSError:
                        pass
                try:
                    self.proc.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    self.proc.terminate()
                    try:
                        self.proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        self.proc.kill()
                        self.proc.wait(timeout=5)
        finally:
            self.proc = None
            probe = self._mount_probe()
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and probe.exists():
                time.sleep(0.2)


def build_mcp_config(mode: str, url: str, api_key: str, dest: Path) -> Path:
    if mode == "redmount":
        cfg = {"mcpServers": {}}
    elif mode == "mcp-onozaty":
        if platform.system() == "Windows":
            server = {
                "command": "cmd",
                "args": ["/c", "npx", "-y", "@onozaty/redmine-mcp-server"],
                "env": {
                    "REDMINE_URL": url,
                    "REDMINE_API_KEY": api_key,
                },
            }
        else:
            server = {
                "command": "npx",
                "args": ["-y", "@onozaty/redmine-mcp-server"],
                "env": {
                    "REDMINE_URL": url,
                    "REDMINE_API_KEY": api_key,
                },
            }
        cfg = {"mcpServers": {"redmine": server}}
    else:
        raise ValueError(f"Unknown mode: {mode}")

    dest.write_text(json.dumps(cfg, indent=2), encoding="utf-8")
    return dest


def parse_stream_json(raw_text: str) -> dict:
    tool_counts: Counter[str] = Counter()
    result_event: dict | None = None
    last_assistant_texts: list[str] = []

    for line in raw_text.splitlines():
        line = line.strip()
        if not line or not line.startswith("{"):
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue

        ev_type = ev.get("type")
        if ev_type == "assistant":
            msg = ev.get("message", {})
            for block in msg.get("content", []):
                if not isinstance(block, dict):
                    continue
                if block.get("type") == "tool_use":
                    name = block.get("name", "unknown")
                    tool_counts[name] += 1
                elif block.get("type") == "text":
                    last_assistant_texts.append(block.get("text", ""))
        elif ev_type == "result":
            result_event = ev

    usage = (result_event or {}).get("usage", {}) or {}
    answer = (result_event or {}).get("result")
    if not isinstance(answer, str):
        answer = "\n".join(last_assistant_texts)

    return {
        "answer": answer or "",
        "is_error": bool((result_event or {}).get("is_error", result_event is None)),
        "api_error": (result_event or {}).get("api_error", ""),
        "api_error_status": (result_event or {}).get("api_error_status", 0),
        "duration_ms": int((result_event or {}).get("duration_ms", 0) or 0),
        "duration_api_ms": int((result_event or {}).get("duration_api_ms", 0) or 0),
        "num_turns": int((result_event or {}).get("num_turns", 0) or 0),
        "total_cost_usd": float((result_event or {}).get("total_cost_usd", 0.0) or 0.0),
        "input_tokens": int(usage.get("input_tokens", 0) or 0),
        "cache_creation_tokens": int(usage.get("cache_creation_input_tokens", 0) or 0),
        "cache_read_tokens": int(usage.get("cache_read_input_tokens", 0) or 0),
        "output_tokens": int(usage.get("output_tokens", 0) or 0),
        "tool_calls_total": sum(tool_counts.values()),
        "tool_calls_by_name": dict(tool_counts),
    }


def run_single(
    *,
    mode: str,
    scenario: Scenario,
    trial: int,
    model: str,
    effort: str,
    url: str,
    api_key: str,
    mount: str,
    redmount_bin: Path,
    skip_seed: bool,
    timeout_s: int,
) -> RunResult:
    if not skip_seed:
        seed_redmine()

    # Fresh empty working directory so repository files / AGENTS.md never leak into context
    if WORKDIR.exists():
        shutil.rmtree(WORKDIR, ignore_errors=True)
    WORKDIR.mkdir(parents=True, exist_ok=True)
    RAW_DIR.mkdir(parents=True, exist_ok=True)

    mcp_cfg_path = WORKDIR / f"mcp-{mode}.json"
    build_mcp_config(mode, url, api_key, mcp_cfg_path)

    rm_proc: RedmountProcess | None = None
    if mode == "redmount":
        rm_proc = RedmountProcess(redmount_bin, url, api_key, DEFAULT_PROJECT, mount)
        rm_proc.start()

    try:
        claude_exe = shutil.which("claude")
        if not claude_exe:
            raise FileNotFoundError("`claude` CLI not found in PATH")

        cmd = [
            claude_exe,
            "-p",
            "--output-format", "stream-json",
            "--verbose",
            "--model", model,
            "--effort", effort,
            "--permission-mode", "bypassPermissions",
            "--no-session-persistence",
            "--setting-sources", "",
            "--disable-slash-commands",
            "--strict-mcp-config",
            "--mcp-config", str(mcp_cfg_path),
        ]

        if mode == "redmount":
            mount_path = f"{mount.rstrip(os.sep)}{os.sep}" if platform.system() == "Windows" else mount
            cmd.extend(["--add-dir", mount_path])
            skill_text = SKILL_MD.read_text(encoding="utf-8")
            cmd.extend(["--append-system-prompt", skill_text])
            prompt = (
                f"Redmine project `{DEFAULT_PROJECT}` is mounted via redmount at `{mount_path}`. "
                f"Use your file tools on `{mount_path}` to complete the task below. Be concise.\n\n"
                f"{scenario.task_prompt}"
            )
        else:
            prompt = (
                f"Use the configured Redmine MCP server tools to complete the task below on project `{DEFAULT_PROJECT}`. "
                f"Be concise.\n\n"
                f"{scenario.task_prompt}"
            )

        t0 = time.perf_counter()
        try:
            proc = subprocess.run(
                cmd,
                input=prompt.encode("utf-8"),
                cwd=WORKDIR,
                capture_output=True,
                timeout=timeout_s,
            )
            exit_code = proc.returncode
            stdout_text = proc.stdout.decode("utf-8", errors="replace")
            stderr_text = proc.stderr.decode("utf-8", errors="replace")
        except subprocess.TimeoutExpired as e:
            exit_code = 124
            stdout_text = (e.stdout or b"").decode("utf-8", errors="replace")
            stderr_text = f"TIMEOUT after {timeout_s}s\n" + (e.stderr or b"").decode("utf-8", errors="replace")
        wall_ms = int((time.perf_counter() - t0) * 1000)

        raw_file = RAW_DIR / f"{mode}_{scenario.id}_t{trial}.jsonl"
        raw_file.write_text(stdout_text, encoding="utf-8")
        if stderr_text.strip():
            (RAW_DIR / f"{mode}_{scenario.id}_t{trial}.stderr.txt").write_text(stderr_text, encoding="utf-8")

        parsed = parse_stream_json(stdout_text)
        if parsed["api_error"] == "usage_limit_reached" or parsed["api_error_status"] == 429:
            raise RuntimeError(f"Claude API rate limit hit (429): {parsed['answer']}")
        api = RedmineClient(url, api_key)
        try:
            passed, verify_detail = scenario.verify(api, parsed["answer"])
        except Exception as e:
            passed, verify_detail = False, f"verify exception: {e}"

        if exit_code != 0 or parsed["is_error"]:
            passed = False
            verify_detail = f"cli error (exit={exit_code}): {verify_detail}"

        return RunResult(
            mode=mode,
            scenario_id=scenario.id,
            scenario_name=scenario.name,
            trial=trial,
            model=model,
            effort=effort,
            passed=passed,
            verify_detail=verify_detail,
            is_error=parsed["is_error"] or exit_code != 0,
            exit_code=exit_code,
            wall_time_ms=wall_ms,
            duration_ms=parsed["duration_ms"],
            duration_api_ms=parsed["duration_api_ms"],
            num_turns=parsed["num_turns"],
            tool_calls_total=parsed["tool_calls_total"],
            tool_calls_by_name=parsed["tool_calls_by_name"],
            input_tokens=parsed["input_tokens"],
            cache_creation_tokens=parsed["cache_creation_tokens"],
            cache_read_tokens=parsed["cache_read_tokens"],
            output_tokens=parsed["output_tokens"],
            total_cost_usd=parsed["total_cost_usd"],
            raw_log_path=str(raw_file.relative_to(REPO_ROOT)),
            answer_preview=parsed["answer"][:300].replace("\n", " "),
        )
    finally:
        if rm_proc is not None:
            rm_proc.stop()


def med(vals: list[float | int]) -> float:
    return float(statistics.median(vals)) if vals else 0.0


def fmt_int(v: float) -> str:
    return f"{int(round(v)):,}"


def fmt_sec(ms: float) -> str:
    return f"{ms / 1000.0:.2f}s"


def fmt_usd(v: float) -> str:
    return f"${v:.4f}"


def generate_report(results: list[RunResult], modes: list[str], scenario_ids: list[str], args: argparse.Namespace) -> str:
    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    lines: list[str] = [
        "# redmount vs Redmine MCP Server Benchmark Report",
        "",
        f"- **Generated**: {now}",
        f"- **Model**: `{args.model}` (effort: `{args.effort}`)",
        f"- **Modes**: {', '.join(f'`{m}`' for m in modes)}",
        f"- **Trials per scenario**: {args.trials}",
        f"- **Dataset**: Local Docker Redmine 6 (`demo` project: 40 issues — 30 Open / 10 Closed, ~17 comments, 4 wiki pages)",
        "",
        "## Per-Scenario Comparison (Median across trials)",
        "",
        "| Scenario | Mode | Pass | Wall Time | API Time | Turns | Tools | Input+Cache (Total) | Cache Create | Output | Cost (USD) |",
        "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]

    by_key: dict[tuple[str, str], list[RunResult]] = {}
    for r in results:
        by_key.setdefault((r.scenario_id, r.mode), []).append(r)

    sc_map = {s.id: s for s in SCENARIOS}

    for sid in scenario_ids:
        sc = sc_map[sid]
        for mode in modes:
            runs = by_key.get((sid, mode), [])
            if not runs:
                continue
            pass_cnt = sum(1 for r in runs if r.passed)
            lines.append(
                f"| **{sid}** {sc.name} | `{mode}` | {pass_cnt}/{len(runs)} "
                f"| {fmt_sec(med([r.wall_time_ms for r in runs]))} "
                f"| {fmt_sec(med([r.duration_api_ms for r in runs]))} "
                f"| {med([r.num_turns for r in runs]):.1f} "
                f"| {med([r.tool_calls_total for r in runs]):.1f} "
                f"| {fmt_int(med([r.total_input_context for r in runs]))} "
                f"| {fmt_int(med([r.cache_creation_tokens for r in runs]))} "
                f"| {fmt_int(med([r.output_tokens for r in runs]))} "
                f"| {fmt_usd(med([r.total_cost_usd for r in runs]))} |"
            )

    lines.extend([
        "",
        "## Aggregate Summary (Sum of Scenario Medians)",
        "",
        "| Mode | Pass Rate | Total Wall Time | Total API Time | Total Turns | Total Tool Calls | Total Input+Cache | Total Cache Create | Total Output | Total Cost (USD) |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ])

    agg_by_mode: dict[str, dict[str, float]] = {}
    for mode in modes:
        mode_runs = [r for r in results if r.mode == mode]
        pass_rate = (sum(1 for r in mode_runs if r.passed) / len(mode_runs) * 100.0) if mode_runs else 0.0
        tot_wall = sum(med([r.wall_time_ms for r in by_key.get((sid, mode), [])]) for sid in scenario_ids)
        tot_api = sum(med([r.duration_api_ms for r in by_key.get((sid, mode), [])]) for sid in scenario_ids)
        tot_turns = sum(med([r.num_turns for r in by_key.get((sid, mode), [])]) for sid in scenario_ids)
        tot_tools = sum(med([r.tool_calls_total for r in by_key.get((sid, mode), [])]) for sid in scenario_ids)
        tot_ctx = sum(med([r.total_input_context for r in by_key.get((sid, mode), [])]) for sid in scenario_ids)
        tot_cc = sum(med([r.cache_creation_tokens for r in by_key.get((sid, mode), [])]) for sid in scenario_ids)
        tot_out = sum(med([r.output_tokens for r in by_key.get((sid, mode), [])]) for sid in scenario_ids)
        tot_cost = sum(med([r.total_cost_usd for r in by_key.get((sid, mode), [])]) for sid in scenario_ids)
        agg_by_mode[mode] = {
            "wall": tot_wall, "api": tot_api, "turns": tot_turns, "tools": tot_tools,
            "ctx": tot_ctx, "cc": tot_cc, "out": tot_out, "cost": tot_cost,
        }
        lines.append(
            f"| `{mode}` | {pass_rate:.1f}% ({sum(1 for r in mode_runs if r.passed)}/{len(mode_runs)}) "
            f"| {fmt_sec(tot_wall)} | {fmt_sec(tot_api)} | {tot_turns:.1f} | {tot_tools:.1f} "
            f"| {fmt_int(tot_ctx)} | {fmt_int(tot_cc)} | {fmt_int(tot_out)} | {fmt_usd(tot_cost)} |"
        )

    if len(modes) == 2 and all(m in agg_by_mode for m in modes):
        m0, m1 = modes[0], modes[1]
        a0, a1 = agg_by_mode[m0], agg_by_mode[m1]
        def ratio(k: str) -> str:
            return f"{(a0[k] / a1[k]):.2f}x" if a1[k] > 0 else "N/A"
        lines.extend([
            "",
            f"**Ratio (`{m0}` / `{m1}`)**: "
            f"Wall Time `{ratio('wall')}`, Turns `{ratio('turns')}`, Tools `{ratio('tools')}`, "
            f"Total Context `{ratio('ctx')}`, Output `{ratio('out')}`, Cost `{ratio('cost')}`.",
        ])

    lines.extend(["", "## Tool Usage Breakdown (All Trials Combined)", ""])
    for mode in modes:
        c: Counter[str] = Counter()
        for r in results:
            if r.mode == mode:
                c.update(r.tool_calls_by_name)
        lines.append(f"### `{mode}`")
        if not c:
            lines.append("- (no tool calls recorded)")
        else:
            for name, count in c.most_common():
                lines.append(f"- `{name}`: {count}")
        lines.append("")

    failures = [r for r in results if not r.passed]
    if failures:
        lines.extend(["## Failed Trials", ""])
        for r in failures:
            lines.append(
                f"- `{r.mode}` / **{r.scenario_id}** (trial {r.trial}): {r.verify_detail} "
                f"(answer: `{r.answer_preview}`)"
            )
        lines.append("")

    return "\n".join(lines)


def main() -> int:
    default_bin = REPO_ROOT / "target" / "release" / ("redmount.exe" if platform.system() == "Windows" else "redmount")
    default_mount = "R:" if platform.system() == "Windows" else str(Path(tempfile.gettempdir()) / "redmount-bench")

    p = argparse.ArgumentParser(description="Benchmark redmount vs Redmine MCP server.")
    p.add_argument("--modes", default="redmount,mcp-onozaty", help="Comma-separated modes")
    p.add_argument("--scenarios", default=",".join(s.id for s in SCENARIOS), help="Comma-separated scenario IDs")
    p.add_argument("--trials", type=int, default=3, help="Number of trials per (mode, scenario)")
    p.add_argument("--model", default="sonnet", help="Claude model")
    p.add_argument("--effort", default="medium", help="Effort level (low, medium, high)")
    p.add_argument("--url", default=DEFAULT_URL, help="Redmine base URL")
    p.add_argument("--api-key", default=DEFAULT_API_KEY, help="Redmine API key")
    p.add_argument("--mount", default=default_mount, help="Mount drive or directory for redmount")
    p.add_argument("--redmount-bin", type=Path, default=default_bin, help="Path to redmount binary")
    p.add_argument("--timeout", type=int, default=180, help="Per-run timeout in seconds")
    p.add_argument("--skip-docker", action="store_true", help="Do not run docker compose up")
    p.add_argument("--skip-seed", action="store_true", help="Do not reset/seed DB between runs")
    args = p.parse_args()

    modes = [m.strip() for m in args.modes.split(",") if m.strip()]
    sc_ids = [s.strip() for s in args.scenarios.split(",") if s.strip()]
    sc_map = {s.id: s for s in SCENARIOS}
    scenarios = [sc_map[sid] for sid in sc_ids]

    if not args.skip_docker:
        ensure_docker_redmine(args.url, args.api_key)

    results: list[RunResult] = []
    total_runs = len(modes) * len(scenarios) * args.trials
    idx = 0

    for trial in range(1, args.trials + 1):
        for sc in scenarios:
            for mode in modes:
                idx += 1
                print(
                    f"\n[{idx}/{total_runs}] mode={mode} scenario={sc.id} ({sc.name}) trial={trial}",
                    flush=True,
                )
                r = run_single(
                    mode=mode,
                    scenario=sc,
                    trial=trial,
                    model=args.model,
                    effort=args.effort,
                    url=args.url,
                    api_key=args.api_key,
                    mount=args.mount,
                    redmount_bin=args.redmount_bin,
                    skip_seed=args.skip_seed,
                    timeout_s=args.timeout,
                )
                results.append(r)
                status = "PASS" if r.passed else "FAIL"
                print(
                    f"  -> {status} | wall={fmt_sec(r.wall_time_ms)} api={fmt_sec(r.duration_api_ms)} "
                    f"turns={r.num_turns} tools={r.tool_calls_total} "
                    f"ctx={fmt_int(r.total_input_context)} out={fmt_int(r.output_tokens)} "
                    f"cost={fmt_usd(r.total_cost_usd)} | {r.verify_detail}",
                    flush=True,
                )

    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    summary_path = RESULTS_DIR / "summary.json"
    report_path = RESULTS_DIR / "report.md"

    summary_payload = {
        "meta": {
            "generated_at": datetime.now(timezone.utc).isoformat(),
            "model": args.model,
            "effort": args.effort,
            "modes": modes,
            "scenarios": sc_ids,
            "trials": args.trials,
        },
        "results": [asdict(r) for r in results],
    }
    summary_path.write_text(json.dumps(summary_payload, indent=2, ensure_ascii=False), encoding="utf-8")

    report_md = generate_report(results, modes, sc_ids, args)
    report_path.write_text(report_md, encoding="utf-8")

    print(f"\nSaved summary: {summary_path}")
    print(f"Saved report:  {report_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
