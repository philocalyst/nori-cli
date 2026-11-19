"""Smoke and time one agent through ACP: initialize, session/new, [model], one prompt.

usage:
  python3 -I agent_smoke.py --nori target/debug/nori --agent claude --model haiku --runs 5
  python3 -I agent_smoke.py --nori target/debug/nori --agent codex --model gpt-6-luna --runs 3 --json
  python3 -I agent_smoke.py --nori target/debug/nori --agent antigravity --via facade --no-prompt

Starts a fresh agent per run (`nori agent serve AGENT [--option K=V...]`, or with `--via facade`
`nori exec --acp --agent AGENT`), speaks newline-delimited JSON-RPC 2.0 over its stdio, and records
per run, in milliseconds:
  initialize_ms     initialize request -> response
  session_new_ms    session/new request -> response
  set_model_ms      session/set_config_option(model) -> response        (with --model)
  first_chunk_ms    session/prompt -> first agent_message_chunk
  prompt_done_ms    session/prompt -> its response
plus the stop reason, the reply text, and the `_meta.nori` usage the agent sent (see
docs/usage.md): whether any arrived, whether it is valid against the contract, the problems found,
and the last report verbatim. A run that fails records `error` and no timings after the failure.

`--json` prints exactly one JSON object on stdout (schema 1: `agent`, `model`, `via`, `runs`,
`summary`), nothing else. Without it the same data is printed as a table. Exit status is 0 when
every run completed and (unless `--no-prompt`) its usage meta was valid, 1 otherwise.

Uses whatever accounts the agent is logged in to; credentials are never read here. Stdlib only.
"""

import argparse
import json
import os
import queue
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time

SCHEMA = 1
TIMINGS = ["initialize_ms", "session_new_ms", "set_model_ms", "first_chunk_ms", "prompt_done_ms"]
TOKEN_KEYS = ["input", "output", "cacheRead", "cacheWrite", "reasoning"]
RFC3339 = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$")


def is_number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def validate_usage(meta):
    """Problems with a `_meta` object against the `_meta.nori` usage contract (empty: valid)."""
    nori = meta.get("nori") if isinstance(meta, dict) else None
    if not isinstance(nori, dict):
        return ["_meta.nori is missing"]
    problems = []
    if "model" in nori and not (isinstance(nori["model"], str) and nori["model"]):
        problems.append("model must be a non-empty string")
    tokens = nori.get("tokens")
    if tokens is not None:
        if not isinstance(tokens, dict):
            problems.append("tokens must be an object")
        else:
            for key in TOKEN_KEYS:
                value = tokens.get(key)
                if not (isinstance(value, int) and not isinstance(value, bool) and value >= 0):
                    problems.append(f"tokens.{key} must be a non-negative integer")
    if nori.get("tokensScope", "delta") not in ("delta", "session"):
        problems.append("tokensScope must be 'session' when present")
    limits = nori.get("rateLimits")
    if limits is not None:
        if not isinstance(limits, list):
            problems.append("rateLimits must be an array")
        else:
            for index, window in enumerate(limits):
                where = f"rateLimits[{index}]"
                if not isinstance(window, dict):
                    problems.append(f"{where} must be an object")
                    continue
                for key in ("id", "label"):
                    if not (isinstance(window.get(key), str) and window[key]):
                        problems.append(f"{where}.{key} must be a non-empty string")
                used = window.get("usedPercent")
                if used is not None and not (is_number(used) and 0 <= used <= 100):
                    problems.append(f"{where}.usedPercent must be a number in 0..100")
                resets = window.get("resetsAt")
                if resets is not None and not (isinstance(resets, str) and RFC3339.match(resets)):
                    problems.append(f"{where}.resetsAt must be RFC 3339")
                minutes = window.get("windowMinutes")
                if minutes is not None and not (isinstance(minutes, int) and not isinstance(minutes, bool) and minutes > 0):
                    problems.append(f"{where}.windowMinutes must be a positive integer")
    if not any(key in nori for key in ("model", "tokens", "rateLimits")):
        problems.append("_meta.nori carries nothing")
    return problems


class Acp:
    def __init__(self, cmd, env, cwd):
        self.t0 = time.monotonic()
        self.proc = subprocess.Popen(
            cmd, cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )
        self.inbox = queue.Queue()
        self.stderr = []
        self.next_id = 1
        self.frames = []
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._read_stderr, daemon=True).start()

    def now(self):
        return (time.monotonic() - self.t0) * 1000.0

    def _read_stderr(self):
        for raw in self.proc.stderr:
            self.stderr.append(raw.decode(errors="replace").rstrip())

    def _read_stdout(self):
        for raw in self.proc.stdout:
            at = self.now()
            try:
                self.inbox.put((at, json.loads(raw)))
            except ValueError:
                self.inbox.put((at, {"unparsed": raw.decode(errors="replace")[:200]}))
        self.inbox.put((self.now(), None))

    def send(self, obj):
        self.proc.stdin.write((json.dumps(obj) + "\n").encode())
        self.proc.stdin.flush()

    def call(self, method, params, timeout):
        """Send a request and wait for its response. Returns (sent_ms, answered_ms, response, frames)."""
        rid = self.next_id
        self.next_id += 1
        sent = self.now()
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        deadline = time.monotonic() + timeout
        seen = []
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise TimeoutError(f"{method} timed out after {timeout:g}s")
            try:
                at, msg = self.inbox.get(timeout=left)
            except queue.Empty:
                raise TimeoutError(f"{method} timed out after {timeout:g}s")
            if msg is None:
                raise RuntimeError(f"agent exited during {method}: {' | '.join(self.stderr[-3:])[:300]}")
            if "method" in msg and "id" in msg:
                self.answer_request(msg)
                continue
            if msg.get("id") == rid and ("result" in msg or "error" in msg):
                if "error" in msg:
                    raise RuntimeError(f"{method} error: {json.dumps(msg['error'])[:300]}")
                return sent, at, msg["result"], seen
            seen.append((at, msg))

    def answer_request(self, msg):
        """An agent-to-client request: allow tool permission once, refuse anything else."""
        if msg["method"] == "session/request_permission":
            options = msg["params"].get("options", [])
            allow = next((o for o in options if str(o.get("kind", "")).startswith("allow")), None)
            outcome = {"outcome": "selected", "optionId": allow["optionId"]} if allow else {"outcome": "cancelled"}
            self.send({"jsonrpc": "2.0", "id": msg["id"], "result": {"outcome": outcome}})
        else:
            self.send({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "not supported by the probe"}})

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def run_once(args, home):
    cwd = tempfile.mkdtemp(prefix="agent-smoke-")
    env = dict(os.environ, NORI_HOME=home)
    if args.via == "serve":
        cmd = [args.nori, "agent", "serve", args.agent]
        for option in args.option:
            cmd.append(option)
    else:
        cmd = [args.nori, "exec", "--acp", "--agent", args.agent, "-C", cwd, "--dangerously-bypass-approvals-and-sandbox"]
    out = {"error": None}
    acp = Acp(cmd, env, cwd)
    try:
        t = args.timeout
        sent, got, result, _ = acp.call(
            "initialize",
            {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "agent-smoke", "version": "1"}},
            t,
        )
        out["initialize_ms"] = round(got - sent)
        info = result.get("agentInfo") or {}
        out["agent_info"] = {"name": info.get("name"), "version": info.get("version")}
        out["capabilities"] = result.get("agentCapabilities")
        sent, got, result, _ = acp.call("session/new", {"cwd": cwd, "mcpServers": []}, t)
        out["session_new_ms"] = round(got - sent)
        session = result["sessionId"]
        if args.model:
            sent, got, _, _ = acp.call(
                "session/set_config_option", {"sessionId": session, "configId": "model", "value": args.model}, t
            )
            out["set_model_ms"] = round(got - sent)
        if args.no_prompt:
            return out
        sent, got, result, seen = acp.call(
            "session/prompt", {"sessionId": session, "prompt": [{"type": "text", "text": args.prompt}]}, t
        )
        out["prompt_done_ms"] = round(got - sent)
        out["stop_reason"] = result.get("stopReason")
        text, reports, first = [], [], None
        for at, msg in seen:
            if msg.get("method") != "session/update":
                continue
            update = msg["params"]["update"]
            kind = update.get("sessionUpdate")
            if kind == "agent_message_chunk":
                if first is None:
                    first = at - sent
                text.append(update.get("content", {}).get("text", ""))
            elif kind in ("usage_update", "session_info_update") and (update.get("_meta") or {}).get("nori"):
                reports.append(update)
        out["first_chunk_ms"] = None if first is None else round(first)
        out["reply"] = "".join(text)
        problems = []
        if not reports:
            problems.append("no usage report arrived")
        for update in reports:
            problems.extend(validate_usage(update["_meta"]))
        out["usage"] = {
            "present": bool(reports),
            "valid": bool(reports) and not problems,
            "problems": problems,
            "reports": len(reports),
            "last": reports[-1] if reports else None,
        }
    except (TimeoutError, RuntimeError, OSError, KeyError) as exc:
        out["error"] = f"{type(exc).__name__}: {exc}"
        out["stderr_tail"] = acp.stderr[-5:]
    finally:
        acp.close()
        shutil.rmtree(cwd, ignore_errors=True)
    return out


def percentile(values, p):
    values = sorted(values)
    return values[max(0, min(len(values) - 1, round(p / 100.0 * (len(values) - 1))))]


def summarize(runs, args):
    summary = {"runs": len(runs), "ok": sum(1 for r in runs if r["error"] is None)}
    summary["usage_valid"] = sum(1 for r in runs if (r.get("usage") or {}).get("valid"))
    for key in TIMINGS:
        values = [r[key] for r in runs if r.get(key) is not None]
        summary[key] = {"n": len(values), "median": statistics.median(values), "p90": percentile(values, 90)} if values else None
    return summary


def print_table(report):
    print(f"agent={report['agent']} model={report['model']} via={report['via']} runs={len(report['runs'])}")
    header = ["run", *[k.replace("_ms", "") for k in TIMINGS], "stop", "usage", "error"]
    rows = []
    for index, run in enumerate(report["runs"], 1):
        usage = run.get("usage")
        flag = "-" if usage is None else ("valid" if usage["valid"] else ("invalid" if usage["present"] else "none"))
        rows.append(
            [str(index)]
            + ["-" if run.get(k) is None else str(run[k]) for k in TIMINGS]
            + [str(run.get("stop_reason") or "-"), flag, run["error"] or "-"]
        )
    widths = [max(len(row[i]) for row in [header, *rows]) for i in range(len(header))]
    for row in [header, *rows]:
        print("  ".join(cell.ljust(widths[i]) for i, cell in enumerate(row)))
    summary = report["summary"]
    print(f"ok {summary['ok']}/{summary['runs']}, usage valid {summary['usage_valid']}/{summary['runs']}")
    for key in TIMINGS:
        stat = summary[key]
        if stat:
            print(f"{key:<16} median {stat['median']:>8.0f}  p90 {stat['p90']:>8.0f}  (n={stat['n']})")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--nori", required=True, help="path to the nori binary")
    ap.add_argument("--agent", required=True, help="agent slug: claude, codex, opencode, antigravity, ...")
    ap.add_argument("--model", help="model id, selected with session/set_config_option after session/new")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--json", action="store_true", help="print one JSON object and nothing else")
    ap.add_argument("--prompt", default="Reply with just OK")
    ap.add_argument("--no-prompt", action="store_true", help="stop after session/new (and the model)")
    ap.add_argument("--via", choices=["serve", "facade"], default="serve", help="`nori agent serve` or `nori exec --acp`")
    ap.add_argument("--option", action="append", default=[], metavar="K=V", help="agent option for `agent serve`")
    ap.add_argument("--home", help="NORI_HOME (default: a fresh temp dir per invocation)")
    ap.add_argument("--timeout", type=float, default=120, help="seconds per request")
    args = ap.parse_args()

    home = args.home or tempfile.mkdtemp(prefix="agent-smoke-home-")
    runs = [run_once(args, home) for _ in range(max(1, args.runs))]
    if not args.home:
        shutil.rmtree(home, ignore_errors=True)
    report = {
        "schema": SCHEMA,
        "agent": args.agent,
        "model": args.model,
        "via": args.via,
        "prompt": None if args.no_prompt else args.prompt,
        "runs": runs,
        "summary": summarize(runs, args),
    }
    if args.json:
        json.dump(report, sys.stdout, indent=1, sort_keys=True)
        sys.stdout.write("\n")
    else:
        print_table(report)
    healthy = all(r["error"] is None and (args.no_prompt or r["usage"]["valid"]) for r in runs)
    sys.exit(0 if healthy else 1)


main()
