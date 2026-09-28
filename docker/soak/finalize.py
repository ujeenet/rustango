#!/usr/bin/env python3
"""Finish a soak run on the host: log scan, Playwright merge, one report.

    python3 docker/soak/finalize.py --out /tmp/soak-report

Reads the driver's `report.json` and `secrets.json` from the
`soak-results` volume, then adds what only the host can see:

  * **#1610 log scan** — every container log is searched for every
    credential, token and cookie the driver used. One hit fails.
  * **ERROR lines** — the fleet logs `ERROR` only for failures nobody
    injected; each one is listed.
  * **bootstrap exits** — every `bootstrap-*` must have exited 0.
  * **Playwright** — `playwright.json` from the browser agent, if present,
    is merged so one report lists every check.

Writes `<out>.json` and `<out>.txt`; exits non-zero on any FAIL.
Standard library only.
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import subprocess
import sys

PROJECT = os.environ.get("SOAK_PROJECT", "rustango-soak")
HERE = os.path.dirname(os.path.abspath(__file__))

# Every fix shipped in 0.58.0, by the issue tag its checks carry. A tag
# with no check row at all is printed as MISSING: nothing may be absent.
INVENTORY = {
    "#1673": "trusted client IP, dual-stack IP rules, streamed body limit",
    "#1675": "Model shortcuts honour scopes; audited Pool writes",
    "#1609": "login rate limits",
    "#1732": "bounded hashing queue",
    "session extractors": "SessionUser/SessionOperator/CurrentMember on any backend",
    "#1338": "password change ends same-second sessions",
    "#1709": "hashing off the async runtime",
    "#1636": "storefront escapes product text",
    "#1668": "idempotency replay scoped to caller and route",
    "#1671": "ViewSet create keeps a natural PK",
    "#1693": "CSRF refuses an empty token",
    "#1711": "admin sets one CSRF cookie",
    "#1669": "urlize escapes; template views check CSRF",
    "#1645": "ensure-helper tenant FKs stay in the tenant schema",
    "#1667": "admin inline formsets stay under their parent",
    "#1670": "webhook delivery checks its target",
    "#1661": "non_exhaustive schema/IR structs and enums",
    "#1713": "tenant admin writes are CSRF-protected",
    "#1710": "operator console is CSRF-protected",
    "#1702": "new tenant project loads its settings",
    "#1700": "allowed_hosts and HTTPS redirect cover the tenancy server",
    "#1699": "security headers on tenant login, admin and console",
    "#1695": "login forms check Origin",
    "#1663": "one cookie reader, percent codecs, intcomma, escapers",
    "#1193": "one error envelope",
    "#1190": "JWT auth is a value; revocation store",
    "#1626": "callback in an atomic migration",
    "#1610": "span redaction with the access log off",
    "#1607": "tenant login CSRF",
    "#1529": "CSRF Origin check on by default",
    "#1538": "JWT with no exp refused",
    "#1672": "single-use refresh, TOTP replay guard, lockout window (PR #1754)",
    "#1674": "page cache keyed on tenant; long DB cache keys (PR #1755)",
    "#1666": "bounded update/delete; nested atomic savepoints (PR #1759)",
    "#1760": "SSO links by subject; email linking opt-in (PR #1760)",
    "#1746": "ViewSet and template views apply global scopes (PR #1751)",
}


def sh(args, check=False):
    return subprocess.run(args, capture_output=True, text=True, check=check)


def read_volume(path):
    """A file from the `soak-results` volume, or None."""
    r = sh(["docker", "run", "--rm", "--entrypoint", "cat", "-v",
            f"{PROJECT}_soak-results:/results", "rustango-soak/driver:latest", path])
    return r.stdout if r.returncode == 0 else None


def add(checks, name, issue, verdict, detail="", instance=""):
    checks.append({"name": name, "issue": issue, "verdict": verdict,
                   "detail": detail, "instance": instance, "source": "finalize"})


def containers():
    """[(service, container, state, status)] for the project, via labels,
    so no compose file (or its required secrets) is needed."""
    r = sh(["docker", "ps", "-a", "--filter", f"label=com.docker.compose.project={PROJECT}",
            "--format", '{{.Label "com.docker.compose.service"}}\t{{.Names}}\t{{.State}}\t{{.Status}}'])
    rows = [tuple(ln.split("\t")) for ln in r.stdout.splitlines() if ln.count("\t") == 3]
    # Only this project's own containers; another stack may reuse the label.
    return [row for row in rows if row[1].startswith(f"{PROJECT}-")]


def service_logs():
    """{service: log text} for every container in the project."""
    out = {}
    for svc, name, _, _ in containers():
        r = sh(["docker", "logs", name])
        out[svc] = out.get(svc, "") + r.stdout + r.stderr
    return out


def scan_logs(checks, logs, secrets, redaction):
    if not any(logs.values()) or not secrets:
        # A scan over nothing would pass; that is not a pass.
        add(checks, "no credential, token or cookie in any container log", "#1610", "FAIL",
            f"nothing to scan: {len(logs)} logs, {len(secrets)} secrets")
        return
    hits = []
    for svc, text in logs.items():
        for s in secrets:
            if s in text:
                hits.append(f"{svc}: {s[:6]}… ({len(s)} chars)")
    add(checks, "no credential, token or cookie in any container log", "#1610",
        "FAIL" if hits else "PASS",
        f"found: {hits[:10]}" if hits else f"{len(secrets)} values over {len(logs)} logs")

    marker = redaction.get("marker")
    inst = redaction.get("instance", "saas-pg-edge")
    svc = "web-" + inst
    text = logs.get(svc, "")
    if not marker:
        add(checks, "span redaction with the access log off", "#1610", "NOT-COVERED",
            "the driver sent no redaction probe", inst)
        return
    lines = [ln for ln in text.splitlines() if "invite_token" in ln]
    if marker in text:
        add(checks, "span redaction with the access log off", "#1610", "FAIL",
            f"the invite token is in clear text: {lines[:1]}", inst)
    elif not lines:
        add(checks, "span redaction with the access log off", "#1610", "NOT-COVERED",
            "no log line carries the query at all, so redaction is not observable", inst)
    else:
        add(checks, "span redaction with the access log off", "#1610", "PASS",
            lines[0][-200:], inst)


def scan_errors(checks, logs):
    bad = []
    for svc, text in logs.items():
        for ln in text.splitlines():
            if " ERROR " in ln or ln.startswith("ERROR"):
                bad.append(f"{svc}: {ln[-220:]}")
    add(checks, "no unexpected ERROR lines in the logs", "—", "FAIL" if bad else "PASS",
        "\n           ".join(bad[:15]) + (f"\n           … {len(bad)} total" if len(bad) > 15
                                          else "") if bad else "")
    return bad


def bootstrap_exits(checks):
    bad, seen = [], 0
    for svc, _, state, status in containers():
        if svc.startswith("bootstrap-"):
            seen += 1
            if state != "exited" or not status.startswith("Exited (0)"):
                bad.append(f"{svc}: {status}")
    add(checks, "every bootstrap migrate finished", "#1626",
        "FAIL" if bad or not seen else "PASS",
        f"{bad}" if bad else f"{seen} bootstrap containers exited 0 (context for #1626)")


def find_playwright(explicit):
    cands = [explicit] if explicit else []
    cands += [os.path.join(HERE, "soak-results", "playwright.json")]
    # The browser agent's worktree sits next to this one; newest first.
    sib = glob.glob(os.path.join(HERE, "..", "..", "..", "rustango-soak*", "**",
                                 "soak-results", "playwright.json"), recursive=True)
    cands += sorted(sib, key=os.path.getmtime, reverse=True)
    for c in cands:
        if c and os.path.exists(c):
            with open(c) as fh:
                return json.load(fh), c
    raw = read_volume("/results/playwright.json")
    if raw:
        return json.loads(raw), "volume:/results/playwright.json"
    return None, None


def normalize_playwright(data):
    rows = data.get("checks", data) if isinstance(data, dict) else data
    out = []
    for r in rows if isinstance(rows, list) else []:
        v = str(r.get("verdict") or r.get("status") or r.get("result") or "").upper()
        v = {"PASSED": "PASS", "FAILED": "FAIL", "SKIPPED": "NOT-COVERED",
             "OK": "PASS"}.get(v, v)
        out.append({"name": r.get("name") or r.get("title", "?"),
                    "issue": r.get("issue", "—"), "verdict": v or "NOT-COVERED",
                    "detail": r.get("detail", ""), "instance": r.get("instance", ""),
                    "source": "playwright"})
    return out


def table(checks):
    lines = []
    by_issue = {}
    for c in checks:
        by_issue.setdefault(c["issue"], []).append(c)
    for issue in list(INVENTORY) + sorted(set(by_issue) - set(INVENTORY)):
        rows = by_issue.get(issue, [])
        title = INVENTORY.get(issue, "")
        if not rows:
            lines.append(f"{issue:>18}  MISSING — no check at all  ({title})")
            continue
        lines.append(f"{issue:>18}  {title}")
        names = {}
        for c in rows:
            names.setdefault((c["name"], c.get("source", "driver")), []).append(c)
        for (name, src), cs in names.items():
            per = ", ".join(f"{c['instance'] or 'fleet'}={c['verdict']}" for c in cs)
            lines.append(f"{'':>20}{name} [{src}]: {per}")
    return lines


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True, help="path prefix for .json and .txt")
    ap.add_argument("--playwright", default=os.environ.get("SOAK_PLAYWRIGHT_JSON"))
    ap.add_argument("--driver-log", help="the driver's printed output, kept in the .txt")
    args = ap.parse_args()

    raw = read_volume("/results/report.json")
    if not raw:
        print("no report.json in the soak-results volume — run the driver first")
        return 2
    report = json.loads(raw)
    secrets = json.loads(read_volume("/results/secrets.json") or "[]")
    checks = [dict(c, source="driver") for c in report["checks"]
              if not (c["issue"] == "#1610" and c["verdict"] == "NOT-COVERED")]

    logs = service_logs()
    scan_logs(checks, logs, secrets, report.get("redaction") or {})
    errors = scan_errors(checks, logs)
    bootstrap_exits(checks)
    pw, pw_path = find_playwright(args.playwright)
    if pw is not None:
        checks += normalize_playwright(pw)
    else:
        add(checks, "Playwright report merged", "—", "NOT-COVERED",
            "no playwright.json found (volume, docker/soak/soak-results, sibling worktree)")

    count = {v: sum(1 for c in checks if c["verdict"] == v)
             for v in ("PASS", "FAIL", "NOT-COVERED", "KNOWN-GAP")}
    missing = [i for i in INVENTORY if not any(c["issue"] == i for c in checks)]
    text = [
        "=" * 70,
        f"soak run {report.get('run_id')}: {count['PASS']} PASS, {count['FAIL']} FAIL, "
        f"{count['NOT-COVERED']} NOT-COVERED, {count['KNOWN-GAP']} KNOWN-GAP, "
        f"{len(missing)} MISSING",
        f"requests: {report.get('requests', {})}",
        f"ERROR lines in logs: {len(errors)}",
        f"playwright: {pw_path or 'not merged'}",
        "=" * 70, "", "failures:",
    ]
    text += [f"  {c['issue']:>8}  {c['name']} [{c['instance']}] ({c.get('source')})\n"
             f"            {c['detail']}" for c in checks if c["verdict"] == "FAIL"] or ["  none"]
    text += ["", "coverage — fix -> checks -> verdict per instance:", ""] + table(checks)
    out = {"summary": count, "missing": missing, "playwright": pw_path,
           "error_lines": errors, "checks": checks, "driver": report}
    with open(args.out + ".json", "w") as fh:
        json.dump(out, fh, indent=2)
    head = ""
    if args.driver_log and os.path.exists(args.driver_log):
        with open(args.driver_log) as fh:
            head = fh.read() + "\n"
    with open(args.out + ".txt", "w") as fh:
        fh.write(head + "\n".join(text) + "\n")
    print("\n".join(text))
    return 1 if count["FAIL"] or missing else 0


if __name__ == "__main__":
    sys.exit(main())
