#!/usr/bin/env python3
"""Browser checks for the 0.58.0 fixes on the admin, tenant admin and console.

Every check loads the real page in Chromium, then tampers with what the
browser sends (drops or empties the CSRF token, rewrites Origin, replays a
TOTP code, posts another parent's child id) and asserts the server refuses
it. The untampered submit of the same form must succeed.

Verdicts and report shape match driver.py: PASS, FAIL, NOT-COVERED, with
`name, issue, verdict, detail, instance`, written to /results/playwright.json.
Failure screenshots go to /results/playwright/.

`PW_SABOTAGE=1` skips every tamper (and rewrites responses where the
protection is a header or a status), so each check sees what an
unprotected server would do. Every check must then FAIL: that is the proof
a check can fail.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import re
import secrets
import struct
import sys
import time
import traceback
from concurrent.futures import ProcessPoolExecutor
from contextlib import contextmanager
from urllib.parse import urlparse

from argon2 import PasswordHasher
from playwright.sync_api import sync_playwright

APEX = os.environ.get("SOAK_APEX", "soak.localhost")
RESULTS = os.environ.get("SOAK_RESULTS", "/results")
SHOTS = os.path.join(RESULTS, "playwright")
SABOTAGE = os.environ.get("PW_SABOTAGE") == "1"
RUN = os.environ.get("SOAK_RUN_ID") or f"{time.time_ns():x}"[-8:]
TENANT = os.environ.get("PW_TENANT", "t01")
# The IdP's authorize URL is published for a host browser; ours is on the
# compose network, so that host:port is mapped to the service.
IDP_PUBLIC = os.environ.get("SOAK_IDP_PUBLIC", "localhost:19000")
IDP_INTERNAL = os.environ.get("SOAK_IDP_INTERNAL", "idp:9000")
IDP_ISSUER = os.environ.get("SOAK_IDP_ISSUER", "http://idp:9000")
SINGLE_HOST = os.environ.get("PW_SINGLE_HOST", "single.localhost")
UNLISTED_HOST = "pw-unlisted.invalid"
EVIL = "http://evil.example"
PAYLOAD = "pw\"'><script>alert(1663)</script><img src=x onerror=alert(1669)>"

ADMIN_PW = os.environ.get("SOAK_ADMIN_PASSWORD", "soak-admin-pw")
OPERATOR_PW = os.environ.get("SOAK_OPERATOR_PASSWORD", "soak-operator-pw")

DEFAULT_INSTANCES = (
    "single-pg=single:web-single-pg:8080,single-my=single:web-single-my:8080,"
    "single-sq=single:web-single-sq:8080,saas-pg=saas:web-saas-pg:8080,"
    "saas-my=saas:web-saas-my:8080,saas-sq=saas:web-saas-sq:8080"
)

HASHER = PasswordHasher()

# Tenant users on PW_TENANT. The tenant admin cannot create users (its
# create form drops the read-only `password_hash`), so bootstrap does,
# with `app create-user <tenant> pw-<k> --password soak-pw-<k>-pw`.
TENANT_USERS = ("csrf", "lock", "chg", "sso-user", "sso-staff", "sso-super", "shared")
TENANT_SUPERUSERS = ("csrf", "sso-super")


def tuser(k):
    return f"pw-{k}", f"soak-pw-{k}-pw"


def instances():
    out = {}
    for item in os.environ.get("PW_INSTANCES", DEFAULT_INSTANCES).split(","):
        name, rest = item.split("=", 1)
        kind, target = rest.split(":", 1)
        out[name.strip()] = (kind.strip(), target.strip())
    only = os.environ.get("PW_ONLY")
    if only:
        keep = {s.strip() for s in only.split(",")}
        out = {k: v for k, v in out.items() if k in keep}
    return out


# ----------------------------------------------------------------- report


class Rec:
    """One instance's checks. A FAIL saves a screenshot of the page."""

    def __init__(self, instance):
        self.instance = instance
        self.checks = []
        # Positive controls (the untampered submit works): sabotage leaves them passing.
        self.controls = set()

    def control(self, name, issue, ok, detail="", page=None):
        self.controls.add(name)
        self.verdict(name, issue, ok, detail, page)

    def add(self, name, issue, verdict, detail="", page=None):
        if verdict == "FAIL" and page is not None:
            try:
                slug = re.sub(r"[^a-z0-9]+", "-", f"{self.instance} {issue} {name}".lower())
                path = os.path.join(SHOTS, f"{slug[:120]}.png")
                page.screenshot(path=path, full_page=True)
                detail = f"{detail} (screenshot {os.path.basename(path)})"
            except Exception:
                pass
        self.checks.append({"name": name, "issue": issue, "verdict": verdict,
                            "detail": detail, "instance": self.instance})
        mark = {"PASS": "ok  ", "FAIL": "FAIL", "NOT-COVERED": "----"}[verdict]
        print(f"  {mark} {issue:>6}  {name} [{self.instance}]"
              + (f"\n           {detail}" if detail else ""), flush=True)

    def verdict(self, name, issue, ok, detail="", page=None):
        self.add(name, issue, "PASS" if ok else "FAIL", detail, page)


class Recorded(Exception):
    """The check already recorded its verdict; stop it."""


@contextmanager
def guarded(rec, name, issue, page=None):
    """A check that raises is a FAIL with the error, never a silent pass."""
    try:
        yield
    except Recorded:
        pass
    except Exception as e:  # noqa: BLE001
        tb = traceback.format_exc(limit=3).strip().splitlines()[-1]
        rec.add(name, issue, "FAIL", f"harness error: {e!s:.300} ({tb[:200]})", page)


# ------------------------------------------------------------------ utils


def totp(secret_b32, at=None, step=30):
    """RFC 6238, SHA1, 6 digits: what `rustango::totp` verifies."""
    key = base64.b32decode(secret_b32.upper() + "=" * (-len(secret_b32) % 8))
    counter = int((at or time.time()) // step)
    mac = hmac.new(key, struct.pack(">Q", counter), hashlib.sha1).digest()
    off = mac[-1] & 0x0F
    code = (struct.unpack(">I", mac[off:off + 4])[0] & 0x7FFFFFFF) % 1_000_000
    return f"{code:06d}", counter


def wait_next_step():
    time.sleep(30 - (time.time() % 30) + 1)


def set_cookies(resp, name):
    """Every `Set-Cookie` for `name` on one response."""
    out = []
    for h in resp.headers_array():
        if h["name"].lower() == "set-cookie":
            for line in h["value"].split("\n"):
                if line.split("=", 1)[0].strip() == name:
                    out.append(line)
    return out


def xff(n):
    """A distinct client IP per limit check (the fleet trusts the bridge)."""
    return f"198.51.{(int(RUN, 16) >> 4) % 200}.{n}"


class Site:
    """One surface: a base URL, its login path and its session cookie."""

    def __init__(self, base, login, admin, session_cookie, scope):
        self.base = base
        self.login = login
        self.admin = admin
        self.session_cookie = session_cookie
        self.scope = scope

    def url(self, path):
        return self.base + path


def launch(pw, target):
    rules = (f"MAP {IDP_PUBLIC} {IDP_INTERNAL}, MAP *.{APEX} {target}, MAP {APEX} {target}, "
             f"MAP {SINGLE_HOST} {target}, MAP {UNLISTED_HOST} {target}")
    return pw.chromium.launch(args=[f"--host-resolver-rules={rules}"])


def new_ctx(browser, state=None, ip=None):
    kw = {}
    if state:
        kw["storage_state"] = state
    if ip:
        kw["extra_http_headers"] = {"X-Forwarded-For": ip}
    ctx = browser.new_context(**kw)
    ctx.set_default_timeout(20000)
    return ctx


def page_with_dialogs(ctx):
    """A page that accepts `confirm()` and records any other dialog as XSS."""
    page = ctx.new_page()
    page.xss = []

    def on_dialog(d):
        if d.type != "confirm":
            page.xss.append(f"{d.type}: {d.message}")
        d.accept()

    page.on("dialog", on_dialog)
    return page


def body_of(resp):
    try:
        return resp.text()
    except Exception:
        return ""


# ----------------------------------------------------------- form tamper

TAMPERS = {
    # tamper -> (issue, what it does)
    "drop": ("no _csrf field", None),
    "empty": ("empty _csrf field and empty cookie", "#1693"),
    "origin": ("foreign Origin", "#1529"),
}


def submit(page, form, tamper=None, button=None, fill=None, timeout=20000):
    """Submit `form` as a user would, tampered as asked. Returns the POST response.

    `form` is a locator for one `<form>`; `fill(form)` types the inputs.
    """
    if fill:
        fill(form)
    action = form.evaluate("f => f.action")
    ctx = page.context
    routed = None
    if tamper and not SABOTAGE:
        if tamper == "drop":
            form.evaluate("f => f.querySelectorAll('input[name=_csrf]').forEach(i => i.remove())")
        elif tamper == "empty":
            form.evaluate("f => f.querySelectorAll('input[name=_csrf]').forEach(i => i.value = '')")
            u = urlparse(action)
            ctx.add_cookies([{"name": "rustango_csrf", "value": "", "url": f"{u.scheme}://{u.netloc}/",
                              "secure": True, "sameSite": "Lax"}])

        # Forms sent by `fetch` carry the token in `X-CSRF-Token`.
        def rewrite(route):
            if route.request.method != "POST":
                return route.continue_()
            h = dict(route.request.headers)
            if tamper == "origin":
                h["origin"] = EVIL
            elif tamper == "drop":
                h.pop("x-csrf-token", None)
            elif tamper == "empty" and "x-csrf-token" in h:
                h["x-csrf-token"] = ""
            route.continue_(headers=h)
        routed = action.split("?")[0]
        page.route(routed, rewrite)
    try:
        with page.expect_response(
                lambda r: r.request.method == "POST" and r.url.split("?")[0] == action.split("?")[0],
                timeout=timeout) as ri:
            if button:
                form.locator(button).first.click()
            else:
                btn = form.locator("button[type=submit], input[type=submit], button:not([type])")
                if btn.count():
                    btn.first.click()
                else:
                    form.evaluate("f => f.requestSubmit()")
        resp = ri.value
        try:
            page.wait_for_load_state("load", timeout=10000)
        except Exception:
            pass
        return resp
    finally:
        if routed:
            page.unroute(routed)


def pin(page, sel):
    """The first form matching `sel`, by a mark that survives changing its action."""
    loc = page.locator(sel).first
    if not loc.count():
        return None
    loc.evaluate("f => f.setAttribute('data-pw-form', '1')")
    return page.locator("form[data-pw-form='1']").first


def csrf_matrix(rec, browser, state, surface, label, issue, page_url, form_sel, *,
                fill=None, button=None, effect=None, refused=None, ok=None,
                untampered=True, ip=None):
    """Tampered submits of one real form are refused; the untampered one works.

    `effect(page, tamper)` says whether the tampered write landed (True =
    landed). `refused(resp, page)` overrides the 403 test for forms that
    answer a refusal differently; `ok(resp, page)` judges the untampered one.
    """
    refused = refused or (lambda r, p: r.status == 403)
    ok = ok or (lambda r, p: r.status < 400)
    for tamper, (what, tissue) in TAMPERS.items():
        name = f"{surface} {label}: {what} refused"
        iss = tissue or issue
        ctx = new_ctx(browser, state, ip)
        page = page_with_dialogs(ctx)
        with guarded(rec, name, iss, page):
            page.goto(page_url)
            form = pin(page, form_sel)
            if not form:
                rec.add(name, iss, "FAIL", f"no form {form_sel} on {page_url}", page)
                continue
            resp = submit(page, form, tamper, button, lambda f: fill(f, tamper) if fill else None)
            is_refused = refused(resp, page)
            landed = effect(page, tamper) if (effect and is_refused) else False
            good = is_refused and not landed
            rec.verdict(name, iss, good,
                        f"{resp.status}" + ("; the write landed anyway" if landed else "")
                        + ("" if is_refused else f": accepted ({body_of(resp)[:120]!r})"), page)
        ctx.close()
    if not untampered:
        return
    name = f"{surface} {label}: untampered submit accepted"
    ctx = new_ctx(browser, state, ip)
    page = page_with_dialogs(ctx)
    with guarded(rec, name, issue, page):
        page.goto(page_url)
        form = pin(page, form_sel)
        resp = submit(page, form, None, button, lambda f: fill(f, None) if fill else None)
        good = ok(resp, page)
        rec.control(name, issue, good, f"{resp.status} -> {page.url}"
                    + ("" if good else f" {body_of(resp)[:160]!r}"), page)
    ctx.close()


# ------------------------------------------------------------------ login


def login(browser, site, user, pw, *, totp_code=None, ip=None, expect_ok=True):
    """Sign in through the real form. Returns (ctx, page, response)."""
    ctx = new_ctx(browser, None, ip)
    page = page_with_dialogs(ctx)
    page.goto(site.url(site.login))
    page.fill("input[name=username]", user)
    page.fill("input[name=password]", pw)
    if totp_code is not None and page.locator("input[name=totp_code]").count():
        page.fill("input[name=totp_code]", totp_code)
    form = page.locator("form:has(input[name=password])").first
    resp = submit(page, form)
    if expect_ok and not has_session(ctx, site):
        raise RuntimeError(f"login as {user} failed: {resp.status} {page.url} "
                           f"{body_of(resp)[:160]!r}")
    return ctx, page, resp


def login_either(browser, site, user, pws):
    """Sign in with the first password that works (a run may have died mid-change)."""
    last = None
    for pw in pws:
        try:
            ctx, page, resp = login(browser, site, user, pw)
            return ctx, page, pw
        except RuntimeError as e:
            last = e
    raise last


def change_password(page, site, url, fields, old, new):
    """Change the signed-in user's password through the real form."""
    page.goto(site.url(url))
    form = page.locator(f"form:has(input[name={fields[1]}])").first
    return submit(page, form, fill=lambda f: (f.locator(f"[name={fields[0]}]").fill(old),
                                              f.locator(f"[name={fields[1]}]").fill(new),
                                              f.locator(f"[name={fields[2]}]").fill(new)))


def has_session(ctx, site):
    return any(c["name"] == site.session_cookie and c["value"] for c in ctx.cookies())


def state_of(ctx):
    return ctx.storage_state()


def attempt_login(browser, site, user, pw, ip, totp_code=None):
    """One sign-in attempt in a fresh context: (status, retry_after, signed_in, page, ctx)."""
    ctx = new_ctx(browser, None, ip)
    page = page_with_dialogs(ctx)
    page.goto(site.url(site.login))
    page.fill("input[name=username]", user)
    page.fill("input[name=password]", pw)
    if totp_code is not None and page.locator("input[name=totp_code]").count():
        page.fill("input[name=totp_code]", totp_code)
    resp = submit(page, page.locator("form:has(input[name=password])").first)
    if SABOTAGE and resp.status == 429:
        # What an unthrottled login would answer.
        return 200, None, False, page, ctx
    return resp.status, resp.headers.get("retry-after"), has_session(ctx, site), page, ctx


def signed_in_view(browser, state, site, path=None):
    """Is this session still admitted? Loads the admin index in a new context."""
    ctx = new_ctx(browser, state)
    page = ctx.new_page()
    resp = page.goto(site.url(path or site.admin))
    admitted = resp.status == 200 and urlparse(page.url).path.rstrip("/") != site.login.rstrip("/")
    admitted = admitted and not page.locator("form:has(input[name=password])").count()
    ctx.close()
    return admitted, f"{resp.status} {urlparse(page.url).path}"


# ------------------------------------------------------ generic checks


def check_headers(rec, browser, state, surface, urls):
    """#1699 — the pages the browser gets carry the security headers."""
    want = {"x-frame-options": None, "x-content-type-options": "nosniff", "referrer-policy": None}
    ctx = new_ctx(browser, state)
    page = ctx.new_page()
    if SABOTAGE:
        def strip(route):
            r = route.fetch()
            h = {k: v for k, v in r.headers.items() if k.lower() not in want}
            route.fulfill(response=r, headers=h)
        page.route("**/*", strip)
    for label, url in urls:
        name = f"{surface} {label} carries security headers"
        with guarded(rec, name, "#1699", page):
            resp = page.goto(url)
            h = resp.all_headers()
            missing = [k for k, v in want.items()
                       if k not in h or (v and h[k].lower() != v)]
            rec.verdict(name, "#1699", not missing,
                        f"{resp.status} {urlparse(page.url).path}; missing {missing}" if missing else
                        f"XFO {h.get('x-frame-options')}, nosniff, Referrer-Policy "
                        f"{h.get('referrer-policy')}", page)
        # The browser must also refuse to frame it.
        fname = f"{surface} {label} cannot be framed"
        with guarded(rec, fname, "#1699", page):
            page.goto(url)
            page.set_content(f'<iframe id="f" src="{url}" width="400" height="300"></iframe>')
            page.wait_for_timeout(1500)
            kids = [f for f in page.frames if f != page.main_frame]
            framed = bool(kids) and not kids[0].url.startswith("chrome-error") and \
                kids[0].locator("body *").count() > 0
            rec.verdict(fname, "#1699", not framed,
                        "the page rendered inside an iframe" if framed else "frame blocked", page)
    ctx.close()


def check_one_csrf_cookie(rec, browser, state, surface, url, form_sel=None):
    """#1711 — a first visit sets exactly one CSRF cookie, and the form token matches it."""
    name = f"{surface} first visit sets one CSRF cookie that matches the form"
    ctx = new_ctx(browser, state)
    if state:
        ctx.clear_cookies(name="rustango_csrf")
    page = ctx.new_page()
    if SABOTAGE:
        def dup(route):
            r = route.fetch()
            h = dict(r.headers)
            h["set-cookie"] = (h.get("set-cookie", "") + "\nrustango_csrf=sabotage; Path=/; Secure").strip()
            route.fulfill(response=r, headers=h)
        page.route("**/*", dup)
    with guarded(rec, name, "#1711", page):
        responses = []
        page.on("response", lambda r: responses.append(r))
        page.goto(url)
        cookies = [c for r in responses for c in set_cookies(r, "rustango_csrf")]
        stored = [c["value"] for c in ctx.cookies() if c["name"] == "rustango_csrf"]
        sel = f"{form_sel} input[name=_csrf]" if form_sel else "form[method=post] input[name=_csrf]"
        tokens = page.locator(sel).evaluate_all("els => els.map(e => e.value)")
        good = len(cookies) == 1 and stored and tokens and all(t == stored[0] for t in tokens)
        rec.verdict(name, "#1711", bool(good),
                    f"{len(cookies)} Set-Cookie rustango_csrf; form token "
                    f"{'matches' if good else 'does not match'} the stored cookie"
                    + ("" if good else f" (cookies={cookies!r:.200}, tokens={tokens[:2]})"), page)
    ctx.close()


def check_allowed_hosts(rec, browser, surface, listed_url, paths):
    """#1700 — an unlisted Host gets 400 on every surface; a listed one does not."""
    ctx = new_ctx(browser)
    page = ctx.new_page()
    try:
        base_ok = page.goto(listed_url).status
    except Exception as e:  # noqa: BLE001
        base_ok = f"error {e}"
    for label, path in paths:
        name = f"{surface} {label}: unlisted Host refused"
        with guarded(rec, name, "#1700", page):
            if SABOTAGE:
                resp = page.goto(listed_url)
            else:
                resp = page.goto(f"http://{UNLISTED_HOST}{path}")
            rec.verdict(name, "#1700", resp.status == 400 and base_ok not in (400,),
                        f"unlisted Host -> {resp.status}; listed -> {base_ok}", page)
    ctx.close()


def check_escaping(rec, page, surface, url, marker):
    """#1663 — a stored `"'><script>` value renders as text; no script runs."""
    name = f"{surface} stored markup renders as text"
    with guarded(rec, name, "#1663", page):
        page.xss.clear()
        if SABOTAGE:
            def unescape(route):
                r = route.fetch()
                body = r.text().replace("&lt;", "<").replace("&gt;", ">").replace(
                    "&quot;", '"').replace("&#x27;", "'").replace("&#39;", "'")
                route.fulfill(response=r, body=body)
            page.route(url, unescape)
        resp = page.goto(url)
        page.wait_for_timeout(800)
        raw = body_of(resp)
        shown = page.get_by_text(marker, exact=False).count() > 0
        injected = page.locator("script:text('alert(1663)')").count() + page.locator(
            "img[src=x]").count()
        quote_escaped = "&#x27;" in raw or "&#39;" in raw
        if SABOTAGE:
            page.unroute(url)
        good = shown and not injected and not page.xss and quote_escaped
        rec.verdict(name, "#1663", good,
                    f"text shown={shown}, injected elements={injected}, dialogs={page.xss}, "
                    f"' escaped={quote_escaped}", page)


# --------------------------------------------------------- admin helpers


def admin_create(page, site, table, values, *, checkbox=(), button="_continue"):
    """Create a row through the admin's create form; returns (resp, new pk or None)."""
    page.goto(site.url(f"{site.admin}/{table}/new"))
    form = page.locator(f"form[action$='/{table}']").first
    for k, v in values.items():
        el = form.locator(f"[name='{k}']").first
        tag = el.evaluate("e => e.tagName.toLowerCase()")
        if tag == "select":
            el.select_option(str(v))
        else:
            el.fill(str(v))
    for k in checkbox:
        form.locator(f"input[type=checkbox][name='{k}']").check()
    fill_datetimes(form)
    resp = submit(page, form, button=f"button[name={button}]")
    m = re.search(rf"/{table}/([^/?#]+)(?:/edit)?/?$", urlparse(page.url).path)
    pk = m.group(1) if m and m.group(1) != "new" else None
    return resp, pk


def admin_row_exists(page, site, table, needle):
    page.goto(site.url(f"{site.admin}/{table}?q={needle}"))
    return page.locator("table tbody tr", has_text=needle).count() > 0


def fill_datetimes(form):
    """Fill required inputs the test does not care about, so the browser submits."""
    for el in form.locator("input[type=datetime-local][required]").all():
        if not el.input_value():
            el.fill("2026-01-01T00:00")
    for el in form.locator("input[type=number][required]").all():
        if not el.input_value():
            el.fill("0")


def create_admin_user(page, site, username, pw):
    """A bare-admin superuser through the bare admin itself."""
    page.goto(site.url(f"{site.admin}/rustango_admin_users/new"))
    form = page.locator("form[action$='/rustango_admin_users']").first
    form.locator("[name=username]").fill(username)
    form.locator("[name=password_hash]").fill(HASHER.hash(pw))
    for cb in ("is_superuser", "active"):
        if form.locator(f"input[name={cb}]").count():
            form.locator(f"input[name={cb}]").check()
    fill_datetimes(form)
    resp = submit(page, form, button="button[name=_continue]")
    m = re.search(r"/rustango_admin_users/(\d+)", page.url)
    if resp.status >= 400 or not m:
        raise RuntimeError(f"creating admin user {username}: {resp.status} {page.url} "
                           f"{body_of(resp)[:200]!r}")
    return int(m.group(1))


# ------------------------------------------------------ admin write set


def admin_write_matrix(rec, browser, state, site, surface, issue):
    """CSRF on every admin write: create, update, delete, bulk action, audit cleanup."""
    tag = f"pw-{RUN}"
    # A row per tampered write, so a write that lands is visible.
    def create_fill(tamper):
        return lambda f: f.locator("[name=name]").fill(f"{tag}-create-{tamper or 'ok'}")

    def created(page, tamper):
        return admin_row_exists(page, site, "commerce_staff", f"{tag}-create-{tamper}")

    csrf_matrix(rec, browser, state, surface, "create row", issue,
                site.url(f"{site.admin}/commerce_staff/new"), "form[action$='/commerce_staff']",
                fill=lambda f, t: create_fill(t)(f), button="button[name=_save]",
                effect=created, ok=lambda r, p: r.status in (302, 303) and
                admin_row_exists(p, site, "commerce_staff", f"{tag}-create-ok"))

    # Update and delete need a row that exists.
    ctx = new_ctx(browser, state)
    page = page_with_dialogs(ctx)
    _, pk = admin_create(page, site, "commerce_staff", {"name": f"{tag}-target"})
    _, pk_del = admin_create(page, site, "commerce_staff", {"name": f"{tag}-delete-me"})
    ctx.close()
    if not pk or not pk_del:
        for label in ("update row", "delete row", "bulk action"):
            rec.add(f"{surface} {label}", issue, "FAIL", "could not create the target row")
        return

    def upd_fill(tamper):
        return lambda f: f.locator("[name=name]").fill(f"{tag}-renamed-{tamper or 'ok'}")

    csrf_matrix(rec, browser, state, surface, "update row", issue,
                site.url(f"{site.admin}/commerce_staff/{pk}/edit"),
                f"form[action$='/commerce_staff/{pk}']",
                fill=lambda f, t: upd_fill(t)(f), button="button[name=_save]",
                effect=lambda p, t: admin_row_exists(p, site, "commerce_staff", f"{tag}-renamed-{t}"))

    csrf_matrix(rec, browser, state, surface, "delete row", issue,
                site.url(f"{site.admin}/commerce_staff/{pk_del}"),
                f"form[action$='/commerce_staff/{pk_del}/delete']",
                effect=lambda p, t: not admin_row_exists(p, site, "commerce_staff", f"{tag}-delete-me"),
                ok=lambda r, p: r.status in (302, 303))

    # Bulk action. The list form posts to `/{table}/__action` without the
    # admin prefix, so it is sent to the prefixed route the admin serves.
    # No action is picked: untampered answers a redirect, and nothing runs.
    bulk = f"{site.admin}/commerce_staff/__action"
    csrf_matrix(rec, browser, state, surface, "bulk action route", issue,
                site.url(f"{site.admin}/commerce_staff"), "form.list-form",
                fill=lambda f, t: f.evaluate(f"f => f.action = '{site.url(bulk)}'"),
                ok=lambda r, p: r.status in (302, 303))

    # Audit cleanup, with a retention nothing is older than.
    audit = site.url(f"{site.admin}/__audit")

    def cleanup_fill(f):
        days = f.locator("[name=days]")
        if days.count():
            days.fill("36500")
        mode = f.locator("[name=mode]")
        if mode.count() and mode.first.get_attribute("type") == "radio":
            f.locator("input[name=mode][value=older_than]").check()

    csrf_matrix(rec, browser, state, surface, "audit cleanup", issue, audit,
                "form[action$='/cleanup']", fill=lambda f, t: cleanup_fill(f))


# -------------------------------------------------------- limits, TOTP


def check_login_limits(rec, browser, site, surface, known_user, known_pw, ip_user):
    """#1609/#1732 — lock after 5 failures, same answer for an unknown name; per-IP ceiling."""
    iss = "#1609"
    # Account lock: 5 wrong passwords, then the right one must still be refused.
    name = f"{surface} lock: right password refused after 5 failures (429 + Retry-After)"
    known = (None, None, "")
    with guarded(rec, name, iss):
        ip = xff(11 + hash(surface) % 40)
        status, _, signed, page, c = attempt_login(browser, site, known_user, known_pw, xff(10))
        c.close()
        if not signed and not SABOTAGE:
            rec.add(name, iss, "NOT-COVERED",
                    f"{known_user} cannot sign in before the test ({status}): still locked "
                    "by a run less than 15 minutes ago?")
            raise Recorded
        for _ in range(5):
            _, _, _, _, c = attempt_login(browser, site, known_user, "wrong-" + secrets.token_hex(4), ip)
            c.close()
        status, ra, signed, page, c = attempt_login(browser, site, known_user, known_pw, ip)
        rec.verdict(name, iss, status == 429 and ra and not signed,
                    f"{status}, Retry-After={ra}, signed in={signed}", page)
        known = (status, bool(ra), body_of_page(page))
        c.close()
    name = f"{surface} lock: unknown username answered like a known one"
    with guarded(rec, name, iss):
        ghost = f"pw-ghost-{RUN}-{surface[-2:]}"
        ip = xff(61 + hash(surface) % 40)
        for _ in range(5):
            _, _, _, _, c = attempt_login(browser, site, ghost, "wrong", ip)
            c.close()
        status, ra, signed, page, c = attempt_login(browser, site, ghost, "wrong", ip)
        same = known[0] is None or ((status, bool(ra)) == known[:2]
                                    and shape(body_of_page(page)) == shape(known[2]))
        rec.verdict(name, iss, status == 429 and ra and same,
                    f"unknown: {status} Retry-After={ra}; known: {known[0]} "
                    f"Retry-After={known[1]}; same body shape={same}", page)
        c.close()
    # Per-IP: a token bucket (20, refilling one per 3s), so the burst is
    # concurrent fetches from the login page. Distinct unknown names keep
    # the account lock out of it; then the real form, right password.
    name = f"{surface} per-IP limit: right password refused after a burst (429 + Retry-After)"
    with guarded(rec, name, iss):
        ip = xff(111 + hash(surface) % 40)
        # Own context, own connection pool: it keeps the bucket empty while
        # the real submit (second context, same client IP) lands.
        bctx = new_ctx(browser, None, ip)
        bp = bctx.new_page()
        bp.goto(site.url(site.login))
        bp.evaluate("""(prefix) => {
            window.__stop = false; window.__st = [];
            const f = document.querySelector('input[name=password]').form;
            const base = new FormData(f);
            let i = 0;
            const worker = async () => {
                while (!window.__stop && i < 300) {
                    const b = new URLSearchParams(base);
                    b.set('username', prefix + (i++)); b.set('password', 'wrong');
                    try {
                        const r = await fetch(f.action, {method: 'POST', body: b, redirect: 'manual'});
                        window.__st.push(r.status);
                    } catch (e) { window.__st.push(-1); }
                }
            };
            for (let k = 0; k < 6; k++) worker();
        }""", f"pw-ip-{RUN}-")
        bp.wait_for_function("() => window.__st.length >= 25", timeout=90000)
        c = new_ctx(browser, None, ip)
        page = page_with_dialogs(c)
        page.goto(site.url(site.login))
        page.fill("input[name=username]", ip_user[0])
        page.fill("input[name=password]", ip_user[1])
        resp = submit(page, page.locator("form:has(input[name=password])").first)
        burst = bp.evaluate("() => { window.__stop = true; return window.__st; }")
        bctx.close()
        status, ra = resp.status, resp.headers.get("retry-after")
        if SABOTAGE and status == 429:
            status, ra = 303, None
        counts = {s: burst.count(s) for s in set(burst)}
        rec.verdict(name, iss, status == 429 and ra is not None and not has_session(c, site),
                    f"burst statuses {counts}; then {ip_user[0]} with the right password -> "
                    f"{status}, Retry-After={ra}", page)
        c.close()
    rec.add(f"{surface} lock window is fixed, not sliding", "#1672", "NOT-COVERED",
            "the window is 1 hour; a browser run cannot wait it out (in-process test covers it)")


def body_of_page(page):
    try:
        return page.content()
    except Exception:
        return ""


def shape(html):
    """The response minus anything that varies per request."""
    return re.sub(r"[0-9a-f-]{8,}|\d+", "#", re.sub(r"<[^>]+>", "", html))[:400]


def check_password_change_ends_sessions(rec, browser, site, surface, user, pw, change_url,
                                        fields):
    """#1338 — a password change ends the user's other sessions, same second included.

    The password is changed back afterwards, so the fixture user survives.
    """
    name = f"{surface} password change ends the other session"
    alt = pw + "-2"
    with guarded(rec, name, "#1338"):
        # B first, so A's sign-in and B's change can land in one second.
        b_ctx, b_page, cur = login_either(browser, site, user, [pw, alt])
        b_page.goto(site.url(change_url))
        a_ctx, _, _ = login(browser, site, user, cur)
        t_a = time.time()
        new_pw = alt if cur == pw else pw
        # Sabotage: a wrong current password changes nothing, so A stays in.
        resp = change_password(b_page, site, change_url, fields,
                               "not-the-password" if SABOTAGE else cur, new_pw)
        same_second = int(time.time()) == int(t_a)
        a_state = state_of(a_ctx)
        # A's session being good without a change is what sabotage shows.
        admitted_after, where = signed_in_view(browser, a_state, site)
        rec.verdict(name, "#1338", not admitted_after,
                    f"change -> {resp.status}; session A afterwards: {where}"
                    f"; A signed in and changed within one second={same_second}", b_page)
        a_ctx.close()
        b_ctx.close()
        if not SABOTAGE and new_pw != pw:
            c, p, _ = login(browser, site, user, new_pw)
            change_password(p, site, change_url, fields, new_pw, pw)
            c.close()


def check_totp(rec, browser, site, surface, user, pw):
    """#1672 — enroll through the page, sign in with a code, a replayed code is refused."""
    iss = "#1672"
    ctx, page, _ = login(browser, site, user, pw)
    name = f"{surface} TOTP enroll through the page"
    secret = None
    with guarded(rec, name, iss, page):
        page.goto(site.url(f"{site.admin}/account/totp"))
        text = page.content()
        m = re.search(r"secret=([A-Z2-7]+)", text) or re.search(r"\b([A-Z2-7]{16,})\b", text)
        if not m:
            rec.add(name, iss, "FAIL", "no setup key on the enroll page", page)
            ctx.close()
            return
        secret = m.group(1)
        if time.time() % 30 > 25:
            wait_next_step()
        code, _ = totp(secret)
        form = page.locator("form:has(input[name=totp_code])").first
        resp = submit(page, form, fill=lambda f: f.locator("[name=totp_code]").fill(code))
        enabled = "now enabled" in page.content() or "enabled" in page.content().lower()
        rec.control(name, iss, resp.status == 200 and enabled, f"{resp.status}", page)
    ctx.close()
    if not secret:
        return
    # The confirming code was redeemed at enrollment: it cannot sign in.
    name = f"{surface} TOTP enrollment code cannot sign in again"
    with guarded(rec, name, iss):
        used, _ = totp(secret)
        if SABOTAGE:
            wait_next_step()
            used, _ = totp(secret)
        status, _, signed, p, c = attempt_login(browser, site, user, pw, xff(201), used)
        rec.verdict(name, iss, not signed, f"{status}, signed in={signed}", p)
        c.close()
    # A fresh code signs in; the same code in a fresh context is refused.
    wait_next_step()
    fresh, _ = totp(secret)
    name = f"{surface} TOTP sign-in with a fresh code"
    with guarded(rec, name, iss):
        status, _, signed, p, c = attempt_login(browser, site, user, pw, xff(202), fresh)
        rec.control(name, iss, signed, f"{status}, signed in={signed}", p)
        c.close()
    name = f"{surface} TOTP same code in a new context refused"
    with guarded(rec, name, iss):
        code = fresh
        if SABOTAGE:
            wait_next_step()
            code, _ = totp(secret)
        status, _, signed, p, c = attempt_login(browser, site, user, pw, xff(203), code)
        rec.verdict(name, iss, not signed, f"{status}, signed in={signed}", p)
        c.close()
    # Wrong codes with the right password count toward the lock.
    name = f"{surface} TOTP wrong codes lock the account"
    with guarded(rec, name, iss):
        ip = xff(204)
        for i in range(5):
            _, _, _, _, c = attempt_login(browser, site, user, pw, ip, "000000" if not SABOTAGE else None)
            c.close()
        wait_next_step()
        good, _ = totp(secret)
        status, ra, signed, p, c = attempt_login(browser, site, user, pw, ip, good)
        rec.verdict(name, iss, status == 429 and not signed,
                    f"right password + valid code after 5 wrong codes -> {status}, "
                    f"Retry-After={ra}, signed in={signed}", p)
        c.close()


# --------------------------------------------------------------- inline


def check_inline_parent(rec, browser, state, site, surface):
    """#1667 — an inline row posted with another parent's child id changes nothing."""
    iss = "#1667"
    name = f"{surface} inline row with another order's line id refused"
    ctx = new_ctx(browser, state)
    page = page_with_dialogs(ctx)
    with guarded(rec, name, iss, page):
        tag = f"pw{RUN}"
        _, cust = admin_create(page, site, "commerce_customer",
                               {"email": f"{tag}@pw.example.test", "full_name": "PW",
                                "loyalty_tier": "bronze"})
        _, prod = admin_create(page, site, "commerce_product",
                               {"sku": f"{tag}-sku", "name": "PW", "price_cents": 100})
        orders, lines = [], []
        for side in ("a", "b"):
            _, o = admin_create(page, site, "commerce_order",
                                {"reference": f"{tag}-{side}", "customer_id": cust,
                                 "status": "pending", "total_cents": 100})
            orders.append(o)
            _, ln = admin_create(page, site, "commerce_order_line",
                                 {"order_id": o, "product_id": prod, "quantity": 1,
                                  "unit_price_cents": 100})
            lines.append(ln)
        if not all(orders + lines + [cust, prod]):
            rec.add(name, iss, "FAIL", f"fixture rows not created: {orders} {lines}", page)
            ctx.close()
            return
        page.goto(site.url(f"{site.admin}/commerce_order/{orders[0]}/edit"))
        form = page.locator(f"form[action$='/commerce_order/{orders[0]}']").first
        pk_input = form.locator("input[type=hidden][name^='commerce_order_line-'][name$='-id']").first
        if not pk_input.count():
            rec.add(name, iss, "NOT-COVERED",
                    "order edit page renders no commerce_order_line inline (no inline registered)")
            ctx.close()
            return
        prefix = pk_input.get_attribute("name")[:-len("-id")]

        def fill(f):
            if not SABOTAGE:
                f.locator(f"input[name='{prefix}-id']").evaluate(f"e => e.value = '{lines[1]}'")
            f.locator(f"[name='{prefix}-quantity']").fill("77")
        resp = submit(page, form, button="button[name=_save]", fill=fill)
        # Sabotage posts order a's own line, so the probe must see that change.
        page.goto(site.url(f"{site.admin}/commerce_order_line/{lines[0 if SABOTAGE else 1]}"))
        body = page.content()
        moved_or_changed = "77" in re.sub(r"<[^>]+>", " ", body).split()
        rec.verdict(name, iss, not moved_or_changed,
                    f"POST -> {resp.status}; order b's line quantity 77 = {moved_or_changed}", page)
    ctx.close()


# ------------------------------------------------------------------ SSO


def sso_signin(browser, site, slug, sub, email=None, verified=True, ip=None):
    """Click the provider's button on the login page and assert an identity at the IdP."""
    ctx = new_ctx(browser, None, ip)
    page = page_with_dialogs(ctx)
    page.goto(site.url(site.login))
    btn = page.locator(f"a[href$='/sso/{slug}']").first
    if not btn.count():
        return None, page, ctx
    btn.click()
    page.wait_for_load_state("load")
    if "/authorize" not in page.url:
        # The app refused to start the flow (e.g. `?sso_error=config`).
        return False, page, ctx
    page.fill("input[name=sub]", sub)
    page.fill("input[name=email]", email or "")
    if not verified:
        page.locator("input[name=email_verified]").uncheck()
    page.locator("button:has-text('Sign in')").click()
    page.wait_for_load_state("load")
    return has_session(ctx, site), page, ctx


def create_tenant_provider(page, site, slug, label, allow_email_link):
    page.goto(site.url(f"{site.admin}/rustango_sso_providers/new"))
    form = page.locator("form[action$='/rustango_sso_providers']").first
    vals = {"slug": slug, "label": label, "issuer_url": IDP_ISSUER,
            "client_id": "pw-client", "client_secret": "pw-secret"}
    for k, v in vals.items():
        if form.locator(f"[name={k}]").count():
            form.locator(f"[name={k}]").fill(v)
    if form.locator("select[name=kind]").count():
        form.locator("select[name=kind]").select_option("oidc")
    elif form.locator("[name=kind]").count():
        form.locator("[name=kind]").fill("oidc")
    form.locator("input[name=enabled]").check()
    if allow_email_link:
        form.locator("input[name=allow_email_link]").check()
    fill_datetimes(form)
    resp = submit(page, form, button="button[name=_continue]")
    m = re.search(r"/rustango_sso_providers/(\d+)", page.url)
    return resp, (int(m.group(1)) if m else None)


def tenant_user_id(page, site, username):
    page.goto(site.url(f"{site.admin}/rustango_users?q={username}"))
    for a in page.locator("a[href*='/rustango_users/']").all():
        m = re.search(r"/rustango_users/(\d+)$", a.get_attribute("href") or "")
        if m and a.inner_text().strip() == username:
            return int(m.group(1))
    for a in page.locator("a[href*='/rustango_users/']").all():
        m = re.search(r"/rustango_users/(\d+)$", a.get_attribute("href") or "")
        if m:
            return int(m.group(1))
    return None


def set_user_email(page, site, uid, email):
    page.goto(site.url(f"{site.admin}/rustango_users/{uid}/edit"))
    form = page.locator(f"form[action$='/rustango_users/{uid}']").first
    if not form.locator("[name=email]").count():
        raise RuntimeError("rustango_users has no email field (build without sso)")
    return submit(page, form, button="button[name=_continue]",
                  fill=lambda f: f.locator("[name=email]").fill(email))


def delete_row(page, site, table, pk):
    page.goto(site.url(f"{site.admin}/{table}/{pk}"))
    form = page.locator(f"form[action$='/{table}/{pk}/delete']").first
    if form.count():
        submit(page, form)


def tenant_sso_checks(rec, browser, sup_state, site, surface):
    """#1760 — link by (provider, sub); email linking opt-in; never privileged; staff locked out."""
    iss = "#1760"
    ctx = new_ctx(browser, sup_state)
    page = page_with_dialogs(ctx)
    if page.goto(site.url(f"{site.admin}/rustango_sso_providers")).status != 200:
        rec.add(f"{surface} SSO", iss, "NOT-COVERED",
                "no rustango_sso_providers in the tenant admin (build without admin-sso)")
        ctx.close()
        return
    # A provider added through the admin UI is the superuser control.
    _, ui_id = create_tenant_provider(page, site, f"pw-ui-{RUN}", f"PW ui {RUN}", False)
    rec.control(f"{surface} superuser can add SsoProvider rows", iss, bool(ui_id), f"id {ui_id}", page)
    # The sign-in providers come from the soak probe: a secret typed into the
    # admin form is stored unencrypted in an encrypted column and never works.
    seeded = page.evaluate("() => fetch('/_soak/sso/seed', {method: 'POST'}).then(r => r.status)")
    if seeded != 200:
        rec.add(f"{surface} SSO providers", iss, "NOT-COVERED", f"/_soak/sso/seed -> {seeded}")
        ctx.close()
        return
    strict_label, link_label = "idp-strict", "idp-link"
    page.goto(site.url(f"{site.admin}/rustango_sso_providers?q=idp-link"))
    m = re.search(r"/rustango_sso_providers/(\d+)", page.locator(
        "table tbody tr", has_text="idp-link").first.inner_html())
    link_id = int(m.group(1)) if m else None
    mail = lambda k: f"{k}@{TENANT}.pw.example.test"  # noqa: E731
    ids = {}
    for k in ("sso-user", "sso-staff", "sso-super"):
        ids[k] = tenant_user_id(page, site, tuser(k)[0])
        set_user_email(page, site, ids[k], mail(k))
    # Staff: holds permissions, including add/change on both SSO tables.
    for code in ("rustango_sso_links.add", "rustango_sso_links.change", "rustango_sso_links.view",
                 "rustango_sso_providers.add", "rustango_sso_providers.change",
                 "rustango_sso_providers.view"):
        admin_create(page, site, "rustango_user_permissions",
                     {"user_id": ids["sso-staff"], "codename": code}, checkbox=("granted",))
    ctx.close()

    def sub(k):
        return f"{k}-sub-{RUN}"

    def sso(name, want_signed, label, subject, email, verified=True):
        with guarded(rec, name, iss):
            signed, p, c = sso_signin(browser, site, label, subject, email, verified)
            if signed is None:
                rec.add(name, iss, "FAIL", f"no '{label}' button on the login page", p)
            else:
                (rec.control if want_signed else rec.verdict)(
                            name, iss, signed == want_signed,
                            f"sub {subject!r}, email {email!r}: signed in={signed} "
                            f"at {urlparse(p.url).path}", p)
            c.close()

    # Sabotage runs each refusal with the input that must succeed.
    sso(f"{surface} SSO email match without opt-in refused", False,
        link_label if SABOTAGE else strict_label, sub("strict"), mail("sso-user"))
    sso(f"{surface} SSO opt-in links a normal user by email", True,
        link_label, sub("sso-user"), mail("sso-user"))
    sso(f"{surface} SSO later sign-in by subject alone works", True,
        link_label, sub("sso-user"), None, False)
    sso(f"{surface} SSO never links a superuser by email", False,
        link_label, sub("sso-user") if SABOTAGE else sub("sso-super"), mail("sso-super"))
    sso(f"{surface} SSO never links a staff account by email", False,
        link_label, sub("sso-user") if SABOTAGE else sub("sso-staff"), mail("sso-staff"))
    sso(f"{surface} SSO subject compares exactly (case differs)", False,
        link_label, sub("sso-user") if SABOTAGE else sub("sso-user").upper(), None, False)

    # Staff cannot add or change link/provider rows, even holding the perms.
    staff_ctx, _, _ = login(browser, site, *tuser("sso-staff"))
    staff = state_of(staff_ctx)
    staff_ctx.close()
    as_staff = sup_state if SABOTAGE else staff
    forms = [
        ("add", "rustango_sso_links", f"{site.admin}/rustango_sso_links/new",
         {"provider_source": "tenant", "provider_id": link_id or 1, "issuer": "oidc|x",
          "subject": f"staff-{RUN}", "subject_sha256": "0" * 64, "user_id": ids["sso-staff"]}),
        ("add", "rustango_sso_providers", f"{site.admin}/rustango_sso_providers/new",
         {"slug": f"pw-staff-{RUN}", "label": "x", "kind": "oidc", "client_id": "x", "client_secret": "x",
          "issuer_url": IDP_ISSUER}),
        ("change", "rustango_sso_providers", f"{site.admin}/rustango_sso_providers/{ui_id}/edit",
         {"label": f"PW ui {RUN} changed"}),
    ]
    for verb, table, path, vals in forms:
        name = f"{surface} staff cannot {verb} a {table} row"
        c = new_ctx(browser, as_staff)
        p = page_with_dialogs(c)
        with guarded(rec, name, iss, p):
            resp = p.goto(site.url(path))
            form = p.locator("form[method=post]:has(button[name=_save])")
            if resp.status == 403 or not form.count():
                rec.verdict(name, iss, resp.status == 403, f"{verb} form: {resp.status}", p)
            else:
                def fill(f):
                    for k, v in vals.items():
                        el = f.locator(f"[name={k}]")
                        if el.count():
                            if el.first.evaluate("e => e.tagName") == "SELECT":
                                el.first.select_option(str(v))
                            else:
                                el.first.fill(str(v))
                    fill_datetimes(f)
                r = submit(p, form.first, button="button[name=_save]", fill=fill)
                rec.verdict(name, iss, r.status == 403, f"form {resp.status}, POST -> {r.status}", p)
        c.close()
    ctx = new_ctx(browser, sup_state)
    page = page_with_dialogs(ctx)
    if ui_id:
        delete_row(page, site, "rustango_sso_providers", ui_id)
    ctx.close()


# ------------------------------------------------------------- surfaces


def section(rec, label, fn, *args):
    """Run one part of a suite; an error fails that part, not the rest."""
    try:
        fn(*args)
    except Exception as e:  # noqa: BLE001
        rec.add(f"{label} ran to the end", "—", "FAIL",
                f"aborted: {e!s:.300}\n{traceback.format_exc(limit=4)[-600:]}")


def saas_suite(rec, browser):
    tenant = Site(f"http://{TENANT}.{APEX}", "/__login", "/__admin", "rustango_tenant_session",
                  "tenant")
    console = Site(f"http://{APEX}", "/login", "", "rustango_op_session", "operator")
    ta, oc = "tenant admin", "console"

    # --- tenant admin ------------------------------------------------
    sup_ctx, sup_page, _ = login(browser, tenant, *tuser("sso-super"))
    sup = state_of(sup_ctx)
    missing = []
    for k in TENANT_USERS:
        u, pw = tuser(k)
        status, _, signed, _, c = attempt_login(browser, tenant, u, pw, xff(9))
        c.close()
        if not signed and status != 429:
            status, _, signed, _, c = attempt_login(browser, tenant, u, pw + "-2", xff(9))
            c.close()
        if not signed and status != 429:
            missing.append(u)
    if missing:
        rec.add(f"{ta} fixture users", "—", "FAIL",
                f"cannot sign in as {missing} on {TENANT}: bootstrap must create them")
    # Escaping fixture.
    admin_create(sup_page, tenant, "commerce_staff", {"name": PAYLOAD[:110]})
    sup_ctx.close()

    check_headers(rec, browser, None, ta, [("login page", tenant.url("/__login"))])
    check_headers(rec, browser, sup, ta, [("index", tenant.url("/__admin")),
                                          ("create form", tenant.url("/__admin/commerce_staff/new"))])
    check_one_csrf_cookie(rec, browser, None, f"{ta} login", tenant.url("/__login"))
    check_one_csrf_cookie(rec, browser, sup, f"{ta} create form",
                          tenant.url("/__admin/commerce_staff/new"),
                          "form[action$='/commerce_staff']")
    check_allowed_hosts(rec, browser, ta, tenant.url("/__login"),
                        [("login", "/__login"), ("admin", "/__admin"), ("app route", "/")])

    csrf_user, csrf_pw = tuser("csrf")
    csrf_matrix(rec, browser, None, ta, "login", "#1607", tenant.url("/__login"),
                "form:has(input[name=password])",
                fill=lambda f, t: (f.locator("[name=username]").fill(csrf_user),
                                   f.locator("[name=password]").fill(csrf_pw)),
                ok=lambda r, p: r.status in (302, 303) and has_session(p.context, tenant))
    section(rec, "admin_write_matrix", admin_write_matrix, rec, browser, sup, tenant, ta, "#1713")

    # Change password and logout, as a user of their own.
    fields = ("current_password", "new_password", "confirm_password")

    def own_user_writes():
        cctx, _, cur = login_either(browser, tenant, csrf_user, [csrf_pw, csrf_pw + "-2"])
        cst = state_of(cctx)
        cctx.close()
        new_pw = csrf_pw + "-2" if cur == csrf_pw else csrf_pw

        def pw_landed(p, t):
            # Probe with the old password: a miss with the new one would count
            # toward the lock, and a success here clears the counter.
            st, _, signed, _, c = attempt_login(browser, tenant, csrf_user, cur, xff(8))
            c.close()
            return not signed
        csrf_matrix(rec, browser, cst, ta, "change password", "#1713", tenant.url("/__change-password"),
                    "form:has(input[name=new_password])",
                    fill=lambda f, t: (f.locator("[name=current_password]").fill(cur),
                                       f.locator("[name=new_password]").fill(new_pw),
                                       f.locator("[name=confirm_password]").fill(new_pw)),
                    effect=pw_landed, ok=lambda r, p: r.status < 400 and
                    attempt_login(browser, tenant, csrf_user, new_pw, xff(8))[2])
        if new_pw != csrf_pw and not SABOTAGE:
            c, p, _ = login(browser, tenant, csrf_user, new_pw)
            change_password(p, tenant, "/__change-password", fields, new_pw, csrf_pw)
            c.close()
        cctx, _, _ = login_either(browser, tenant, csrf_user, [csrf_pw, csrf_pw + "-2"])
        cst = state_of(cctx)
        cctx.close()
        csrf_matrix(rec, browser, cst, ta, "logout", "#1713", tenant.url("/__admin"),
                    "form[action$='/__logout']",
                    effect=lambda p, t: not signed_in_view(p.context.browser, state_of(p.context),
                                                           tenant)[0],
                    ok=lambda r, p: r.status in (302, 303) and not has_session(p.context, tenant))

    section(rec, f"{ta} change password and logout", own_user_writes)

    ectx = new_ctx(browser, sup)
    epage = page_with_dialogs(ectx)
    check_escaping(rec, epage, f"{ta} list", tenant.url("/__admin/commerce_staff?q=pw"), "alert(1663)")
    ectx.close()
    section(rec, "check_inline_parent", check_inline_parent, rec, browser, sup, tenant, ta)
    check_password_change_ends_sessions(rec, browser, tenant, ta, *tuser("chg"),
                                        "/__change-password", fields)
    section(rec, "tenant_sso_checks", tenant_sso_checks, rec, browser, sup, tenant, ta)
    budget_pause()
    check_login_limits(rec, browser, tenant, ta, *tuser("lock"), tuser("csrf"))

    # --- operator console --------------------------------------------
    op_ctx, op_page, _ = login(browser, console, "soakops", OPERATOR_PW)
    ops = state_of(op_ctx)
    op_ctx.close()
    check_headers(rec, browser, None, oc, [("login page", console.url("/login"))])
    check_headers(rec, browser, ops, oc, [("index", console.url("/")),
                                          ("tenant edit", console.url(f"/orgs/{TENANT}/edit"))])
    check_one_csrf_cookie(rec, browser, None, f"{oc} login", console.url("/login"))
    check_one_csrf_cookie(rec, browser, ops, f"{oc} operators", console.url("/operators"),
                          "form[action$='/operators']")
    check_allowed_hosts(rec, browser, oc, console.url("/login"), [("login", "/login")])
    section(rec, "console_writes", console_writes, rec, browser, ops, console, oc)
    section(rec, "console_sso", console_sso, rec, browser, ops, console, tenant, oc)

    # Throwaway operators for the password-change and limit checks.
    pctx = new_ctx(browser, ops)
    ppage = page_with_dialogs(pctx)
    lock_op, chg_op = f"pwlock{RUN}", f"pwchg{RUN}"
    for u in (lock_op, chg_op):
        create_operator(ppage, console, u, f"{u}-password")
    pctx.close()
    check_password_change_ends_sessions(rec, browser, console, oc, chg_op, f"{chg_op}-password",
                                        "/change-password",
                                        ("current_password", "new_password", "confirm_password"))
    budget_pause()
    check_login_limits(rec, browser, console, oc, lock_op, f"{lock_op}-password",
                       ("soakops", OPERATOR_PW))


def create_operator(page, console, username, pw):
    page.goto(console.url("/operators"))
    form = page.locator("form[action$='/operators']").first
    return submit(page, form, button="button:not([name=generate])[type=submit]",
                  fill=lambda f: (f.locator("[name=username]").fill(username),
                                  f.locator("[name=password]").fill(pw),
                                  f.locator("[name=confirm_password]").fill(pw)))


def console_writes(rec, browser, ops, console, oc):
    """#1710 — every console POST form: tampered refused, untampered works."""
    iss = "#1710"
    csrf_matrix(rec, browser, None, oc, "login", iss, console.url("/login"),
                "form:has(input[name=password])",
                fill=lambda f, t: (f.locator("[name=username]").fill("soakops"),
                                   f.locator("[name=password]").fill(OPERATOR_PW)),
                ok=lambda r, p: r.status in (302, 303) and has_session(p.context, console))

    def op_exists(page, name):
        page.goto(console.url("/operators"))
        return page.get_by_text(name, exact=True).count() > 0

    new_op = lambda t: f"pwop{RUN}{t or 'ok'}"  # noqa: E731
    csrf_matrix(rec, browser, ops, oc, "create operator", iss, console.url("/operators"),
                "form[action$='/operators']", button="button:not([name=generate])[type=submit]",
                fill=lambda f, t: (f.locator("[name=username]").fill(new_op(t)),
                                   f.locator("[name=password]").fill("pw-operator-pass"),
                                   f.locator("[name=confirm_password]").fill("pw-operator-pass")),
                effect=lambda p, t: op_exists(p, new_op(t)),
                ok=lambda r, p: r.status < 400 and op_exists(p, new_op(None)))
    # Row forms on the operators page, against the operator created above.
    row = f"tr:has-text('{new_op(None)}')"
    ctx = new_ctx(browser, ops)
    page = ctx.new_page()
    page.goto(console.url("/operators"))
    target = page.locator(f"{row} form[action*='/reset-password']").first
    op_id = None
    if target.count():
        op_id = re.search(r"/operators/(\d+)/", target.get_attribute("action")).group(1)
    ctx.close()
    if op_id:
        csrf_matrix(rec, browser, ops, oc, "reset operator password", iss,
                    console.url(f"/operators#reset-{op_id}"),
                    f"form[action$='/operators/{op_id}/reset-password']",
                    button="button:not([name=generate])[type=submit]",
                    fill=lambda f, t: [el.fill("pw-reset-password-1") for el in
                                       f.locator("input[type=password]").all()])
        csrf_matrix(rec, browser, ops, oc, "deactivate operator", iss, console.url("/operators"),
                    f"form[action$='/operators/{op_id}/active']")
    else:
        rec.add(f"{oc} operator row forms", iss, "FAIL", "the new operator has no row forms")

    edit = console.url(f"/orgs/{TENANT}/edit")
    csrf_matrix(rec, browser, ops, oc, "edit tenant (unchanged values)", iss, edit,
                f"form[action$='/orgs/{TENANT}/edit']")
    csrf_matrix(rec, browser, ops, oc, "branding upload", iss, edit, "#branding-form",
                fill=lambda f, t: f.evaluate(
                    f"f => f.action = f.action.replace('/{TENANT}/', '/pw-nosuch-{RUN}/')"),
                ok=lambda r, p: r.status != 403 and r.status < 500)
    csrf_matrix(rec, browser, ops, oc, "impersonate", iss, edit,
                f"form[action$='/orgs/{TENANT}/impersonate']",
                ok=lambda r, p: r.status in (302, 303))
    # Destructive forms point at a slug that does not exist: a refusal
    # proves CSRF, and an accepted POST cannot destroy anything.
    nosuch = f"pw-nosuch-{RUN}"
    retarget = lambda f: f.evaluate(  # noqa: E731
        f"f => f.action = f.action.replace('/{TENANT}/', '/{nosuch}/')")
    csrf_matrix(rec, browser, ops, oc, "deactivate tenant (nonexistent slug)", iss, edit,
                f"form[action$='/orgs/{TENANT}/deactivate']", fill=lambda f, t: retarget(f),
                ok=lambda r, p: r.status in (302, 303))
    csrf_matrix(rec, browser, ops, oc, "purge tenant (nonexistent slug)", iss, edit,
                f"form[action$='/orgs/{TENANT}/purge']",
                fill=lambda f, t: (retarget(f), f.locator("[name=confirm]").fill("not-the-slug")),
                ok=lambda r, p: r.status in (302, 303))
    host = lambda t: f"pw-{RUN}-{t or 'ok'}.example.test"  # noqa: E731
    hosts = console.url(f"/orgs/{TENANT}/hosts")
    csrf_matrix(rec, browser, ops, oc, "add host", iss, hosts, "form[action$='/hosts/add']",
                fill=lambda f, t: f.locator("[name=hostname]").fill(host(t)),
                effect=lambda p, t: (p.goto(hosts), p.get_by_text(host(t)).count() > 0)[1])
    csrf_matrix(rec, browser, ops, oc, "toggle host", iss, hosts,
                f"tr:has-text('{host(None)}') form[action$='/hosts/toggle']")
    csrf_matrix(rec, browser, ops, oc, "remove host", iss, hosts,
                f"tr:has-text('{host(None)}') form[action$='/hosts/remove']",
                effect=lambda p, t: not (p.goto(hosts), p.get_by_text(host(None)).count() > 0)[1])
    orgs = console.url("/orgs")
    ctx = new_ctx(browser, ops)
    page = ctx.new_page()
    page.goto(orgs)
    extra = page.locator("form[method=post]").evaluate_all("fs => fs.map(f => f.action)")
    ctx.close()
    if any(a.endswith("/orgs/prewarm") for a in extra):
        csrf_matrix(rec, browser, ops, oc, "pre-warm pools", iss, orgs, "form[action$='/orgs/prewarm']")
    else:
        rec.add(f"{oc} pre-warm pools", iss, "NOT-COVERED", "no pre-warm form on /orgs")
    # Anything posting that this suite does not know is reported, not skipped.
    known = ("/logout", "/orgs/prewarm", "/orgs/migrate")
    unknown = [a for a in extra if not a.endswith(known)]
    if unknown:
        rec.add(f"{oc} forms on /orgs the suite does not drive", iss, "NOT-COVERED", ", ".join(unknown))
    # Logout last: its own user, so the rest keep their session.
    lctx, _, _ = login(browser, console, "soakops", OPERATOR_PW)
    lst = state_of(lctx)
    lctx.close()
    csrf_matrix(rec, browser, lst, oc, "logout", iss, console.url("/"), "form[action$='/logout']",
                effect=lambda p, t: not signed_in_view(p.context.browser, state_of(p.context),
                                                       console, "/")[0],
                ok=lambda r, p: r.status in (302, 303))


def console_sso(rec, browser, ops, console, tenant, oc):
    """#1760 — shared providers: CSRF, the email-link toggle keeps the id and its links."""
    iss = "#1760"
    ctx = new_ctx(browser, ops)
    page = page_with_dialogs(ctx)
    if page.goto(console.url("/sso-shared")).status == 404:
        rec.add(f"{oc} shared SSO providers", iss, "NOT-COVERED", "no /sso-shared (build without admin-sso)")
        ctx.close()
        return
    ctx.close()
    slug = lambda t: f"pw-shared-{RUN}-{t or 'ok'}"  # noqa: E731
    label = f"PW shared {RUN} {PAYLOAD[:40]}"

    def fill(f, t):
        f.locator("[name=slug]").fill(slug(t))
        f.locator("[name=label]").fill(label if t is None else f"PW shared {t}")
        for k, v in (("issuer_url", IDP_ISSUER), ("client_id", "pw-client"),
                     ("client_secret", "pw-secret")):
            if f.locator(f"[name={k}]").count():
                f.locator(f"[name={k}]").fill(v)
        if f.locator("select[name=kind]").count():
            f.locator("select[name=kind]").select_option("oidc")
        f.locator("input[name=allow_email_link]").check()

    def exists(p, t):
        p.goto(console.url("/sso-shared"))
        return p.get_by_text(slug(t), exact=True).count() > 0

    csrf_matrix(rec, browser, ops, oc, "add shared SSO provider", iss, console.url("/sso-shared"),
                "form[action$='/sso-shared']", fill=fill, effect=exists,
                ok=lambda r, p: r.status < 400 and exists(p, None))
    ctx = new_ctx(browser, ops)
    page = page_with_dialogs(ctx)
    page.goto(console.url("/sso-shared"))
    row = page.locator(f"tr:has-text('{slug(None)}')")
    toggle = row.locator("form[action$='/email-link']")
    if not toggle.count():
        rec.add(f"{oc} shared SSO email-link toggle", iss, "FAIL", "no toggle form on the row", page)
        ctx.close()
        return
    pid = re.search(r"/sso-shared/(\d+)/", toggle.get_attribute("action")).group(1)
    check_escaping(rec, page, f"{oc} shared SSO list", console.url("/sso-shared"), "alert(1663)")
    tctx = new_ctx(browser)
    tpage = page_with_dialogs(tctx)
    check_escaping(rec, tpage, "tenant login (shared provider label)", tenant.url("/__login"),
                   "alert(1663)")
    tctx.close()
    ctx.close()

    # A normal user links through the shared provider by email.
    sctx, spage, _ = login(browser, tenant, *tuser("sso-super"))
    email = f"shared@{TENANT}.pw.example.test"
    set_user_email(spage, tenant, tenant_user_id(spage, tenant, tuser("shared")[0]), email)
    sctx.close()
    sub = f"shared-sub-{RUN}"
    signed, p, c = sso_signin(browser, tenant, slug(None), sub, email)
    rec.control(f"{oc} shared provider with email linking links a normal user", iss, bool(signed),
                f"signed in={signed}", p)
    c.close()

    def toggled_off(p, t):
        p.goto(console.url("/sso-shared"))
        r = p.locator(f"tr:has-text('{slug(None)}')")
        return "Allow email linking" in r.inner_text()

    csrf_matrix(rec, browser, ops, oc, "shared SSO email-link toggle", iss, console.url("/sso-shared"),
                f"form[action$='/sso-shared/{pid}/email-link']", effect=toggled_off,
                ok=lambda r, p: r.status in (302, 303) and toggled_off(p, None))
    ctx = new_ctx(browser, ops)
    page = page_with_dialogs(ctx)
    if SABOTAGE:
        # An edit that recreates the row: a new id, and the old links orphaned.
        page.goto(console.url("/sso-shared"))
        submit(page, pin(page, f"form[action$='/sso-shared/{pid}/delete']"))
        page.goto(console.url("/sso-shared"))
        submit(page, pin(page, "form[action$='/sso-shared']"), fill=lambda f: fill(f, None))
    page.goto(console.url("/sso-shared"))
    still = page.locator(f"form[action$='/sso-shared/{pid}/email-link']").count() > 0
    rec.verdict(f"{oc} email-link toggle keeps the provider id", iss, still,
                f"row id {pid} {'kept' if still else 'gone'}", page)
    ctx.close()
    name = f"{oc} link survives the toggle: sign-in by subject still works"
    with guarded(rec, name, iss):
        signed, p, c = sso_signin(browser, tenant, slug(None), sub, None, verified=False)
        rec.verdict(name, iss, bool(signed), f"signed in={signed}", p)
        c.close()
    # Delete the shared provider (its label is on every tenant's login page).
    csrf_matrix(rec, browser, ops, oc, "delete shared SSO provider", iss, console.url("/sso-shared"),
                f"form[action$='/sso-shared/{pid}/delete']",
                effect=lambda p, t: not exists(p, None), ok=lambda r, p: not exists(p, None))
    # Whatever this run left behind (tampered adds, a sabotage re-add).
    ctx = new_ctx(browser, ops)
    page = page_with_dialogs(ctx)
    page.goto(console.url("/sso-shared"))
    for a in page.locator("form[action*='/sso-shared/'][action$='/delete']").evaluate_all(
            f"fs => fs.filter(f => f.closest('tr').innerText.includes('pw-shared-{RUN}'))"
            ".map(f => f.getAttribute('action'))"):
        page.goto(console.url("/sso-shared"))
        submit(page, pin(page, f"form[action='{a}']"))
    ctx.close()


def single_suite(rec, browser):
    site = Site(f"http://{SINGLE_HOST}", "/__admin/login", "/__admin", "rustango_admin_session",
                "admin")
    ad = "admin"
    ctx = new_ctx(browser)
    page = ctx.new_page()
    r = page.goto(site.url("/__admin/login"))
    has_login = r.status == 200 and page.locator("input[name=password]").count() > 0
    ctx.close()
    if not has_login:
        rec.add("admin login page", "#1711", "NOT-COVERED",
                f"/__admin/login -> {r.status}: the admin is mounted without session auth, so it "
                "has no login, CSRF or limits")
        return
    # `soakadmin` is the driver's too (its checks change passwords), so it
    # only mints this run's users; everything else runs as our own.
    sctx, spage, _ = login(browser, site, "soakadmin", ADMIN_PW)
    tag = RUN
    sup_user, totp_user, lock_user, chg_user, csrf_user = (
        f"pw-sup-{tag}", f"pw-totp-{tag}", f"pw-lock-{tag}", f"pw-chg-{tag}", f"pw-csrf-{tag}")
    for u in (sup_user, totp_user, lock_user, chg_user, csrf_user):
        create_admin_user(spage, site, u, f"{u}-pw")
    sctx.close()
    sctx, spage, _ = login(browser, site, sup_user, f"{sup_user}-pw")
    sup = state_of(sctx)
    admin_create(spage, site, "commerce_staff", {"name": PAYLOAD[:110]})
    sctx.close()

    check_headers(rec, browser, None, ad, [("login page", site.url("/__admin/login"))])
    check_headers(rec, browser, sup, ad, [("index", site.url("/__admin")),
                                          ("create form", site.url("/__admin/commerce_staff/new"))])
    check_one_csrf_cookie(rec, browser, None, f"{ad} login", site.url("/__admin/login"))
    check_one_csrf_cookie(rec, browser, sup, f"{ad} create form",
                          site.url("/__admin/commerce_staff/new"), "form[action$='/commerce_staff']")
    rec.add(f"{ad} unlisted Host refused", "#1700", "NOT-COVERED",
            "the single-tenant instances set no allowed_hosts")

    # The admin login answers a bad token with the form again, not 403.
    csrf_matrix(rec, browser, None, ad, "login", "#1695", site.url("/__admin/login"),
                "form:has(input[name=password])",
                fill=lambda f, t: (f.locator("[name=username]").fill(csrf_user),
                                   f.locator("[name=password]").fill(f"{csrf_user}-pw")),
                refused=lambda r, p: not has_session(p.context, site),
                ok=lambda r, p: r.status in (302, 303) and has_session(p.context, site))
    section(rec, "admin_write_matrix", admin_write_matrix, rec, browser, sup, site, ad, "#1711")
    cctx, _, _ = login(browser, site, csrf_user, f"{csrf_user}-pw")
    cst = state_of(cctx)
    cctx.close()
    new_pw = f"{csrf_user}-pw2"
    csrf_matrix(rec, browser, cst, ad, "change password", "#1711", site.url("/__admin/account/password"),
                "form:has(input[name=new_password])",
                fill=lambda f, t: (f.locator("[name=current_password]").fill(f"{csrf_user}-pw"),
                                   f.locator("[name=new_password]").fill(new_pw),
                                   f.locator("[name=new_password_confirm]").fill(new_pw)),
                ok=lambda r, p: r.status == 200 and "updated" in p.content().lower())
    cctx, _, _ = login(browser, site, csrf_user, new_pw)
    cst = state_of(cctx)
    cctx.close()
    csrf_matrix(rec, browser, cst, ad, "TOTP enroll submit", "#1711", site.url("/__admin/account/totp"),
                "form:has(input[name=totp_code])",
                fill=lambda f, t: f.locator("[name=totp_code]").fill("000000"),
                ok=lambda r, p: r.status == 200 and "didn't match" in p.content())
    csrf_matrix(rec, browser, cst, ad, "logout", "#1711", site.url("/__admin"),
                "form[action$='/logout']",
                effect=lambda p, t: not signed_in_view(p.context.browser, state_of(p.context), site)[0],
                ok=lambda r, p: r.status in (302, 303))

    ectx = new_ctx(browser, sup)
    epage = page_with_dialogs(ectx)
    check_escaping(rec, epage, f"{ad} list", site.url("/__admin/commerce_staff?q=pw"), "alert(1663)")
    ectx.close()
    section(rec, "check_inline_parent", check_inline_parent, rec, browser, sup, site, ad)
    check_password_change_ends_sessions(rec, browser, site, ad, chg_user, f"{chg_user}-pw",
                                        "/__admin/account/password",
                                        ("current_password", "new_password", "new_password_confirm"))
    section(rec, "check_totp", check_totp, rec, browser, site, ad, totp_user, f"{totp_user}-pw")
    rec.add(f"{ad} SSO sign-in", "#1760", "NOT-COVERED",
            "the single-tenant app is built without admin-sso")
    budget_pause()
    check_login_limits(rec, browser, site, ad, lock_user, f"{lock_user}-pw",
                       (sup_user, f"{sup_user}-pw"))


def budget_pause():
    """The per-IP bucket is shared by every login on an instance; start the limit
    checks on a fresh window in case forwarded IPs are not honoured."""
    if not SABOTAGE:
        time.sleep(int(os.environ.get("PW_LIMIT_PAUSE", "61")))


# ------------------------------------------------------------------- main


def wait_ready(kind, target, timeout=300):
    import urllib.request
    host, port = target.rsplit(":", 1)
    url = f"http://{host}:{port}/_soak/info" if kind == "single" else f"http://{host}:{port}/login"
    hdr = {} if kind == "single" else {"Host": APEX}
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            req = urllib.request.Request(url, headers=hdr)
            with urllib.request.urlopen(req, timeout=5) as r:
                if r.status == 200:
                    return True
        except Exception:
            pass
        time.sleep(3)
    return False


def run_instance(item):
    name, (kind, target) = item
    rec = Rec(name)
    if not wait_ready(kind, target):
        rec.add("instance reachable", "—", "FAIL", f"{target} never answered")
        return rec.checks
    with sync_playwright() as pw:
        browser = launch(pw, target)
        try:
            (saas_suite if kind == "saas" else single_suite)(rec, browser)
        except Exception as e:  # noqa: BLE001
            rec.add("suite ran to the end", "—", "FAIL",
                    f"aborted: {e!s:.300}\n{traceback.format_exc(limit=4)[-600:]}")
        finally:
            browser.close()
    for c in rec.checks:
        c["_control"] = c["name"] in rec.controls
    return rec.checks


def main():
    os.makedirs(SHOTS, exist_ok=True)
    targets = instances()
    print(f"== playwright: {len(targets)} instance(s), run {RUN}"
          + (" [SABOTAGE: every check must FAIL]" if SABOTAGE else ""), flush=True)
    checks = []
    with ProcessPoolExecutor(max_workers=int(os.environ.get("PW_PARALLEL", "6"))) as ex:
        for res in ex.map(run_instance, targets.items()):
            checks.extend(res)
    controls = {(c["instance"], c["name"]) for c in checks if c.pop("_control", False)}
    out = os.path.join(RESULTS, "playwright-sabotage.json" if SABOTAGE else "playwright.json")
    with open(out, "w") as fh:
        json.dump({"checks": checks, "run": RUN, "sabotage": SABOTAGE}, fh, indent=2)
    n = {v: sum(1 for c in checks if c["verdict"] == v) for v in ("PASS", "FAIL", "NOT-COVERED")}
    print(f"\n{n['PASS']} passed, {n['FAIL']} FAILED, {n['NOT-COVERED']} not covered -> {out}")
    if SABOTAGE:
        passed = [c for c in checks if c["verdict"] == "PASS"
                  and (c["instance"], c["name"]) not in controls]
        print(f"sabotage: {len(controls)} positive control(s) skipped; "
              f"{len(passed)} check(s) still passed without the protection")
        for c in passed:
            print(f"    {c['issue']:>6}  {c['name']} [{c['instance']}]")
        return 1 if passed else 0
    return 1 if n["FAIL"] else 0


if __name__ == "__main__":
    sys.exit(main())
