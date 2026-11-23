#!/usr/bin/env python3
"""Make the README's screenshots from a demo home. No real account is touched.

    cargo build && python3 scripts/screenshots.py            # writes docs/screenshots/*.svg
    python3 scripts/screenshots.py --cx path/to/cx

It builds a throwaway HOME with four made-up accounts, an API key and a week
of made-up transcripts, serves stand-ins for the three Anthropic endpoints cx
calls on a local port, runs the real dashboard in tmux, and turns what tmux
captured into SVG. Needs python3 and tmux; nothing else.

The pictures are the real program's output, so run this again after changing
the dashboard. Times in them are relative to now.
"""
import argparse, datetime, html, http.server, json, os, pathlib, random, re, shutil, subprocess, sys, tempfile, threading, time

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "docs" / "screenshots"
COLS, ROWS = 150, 46
HOUR_MS = 3600 * 1000

# letter -> (email, alias, 5-hour %, weekly %, Fable weekly %)
ACCOUNTS = {
    "A": ("ada@example.com", "work", 91.0, 64.0, 97.0),
    "B": ("ada.personal@example.com", "home", 38.0, 22.0, 41.0),
    "C": ("team-build@example.com", "build", 12.0, 71.0, 18.0),
    "D": ("research@example.com", "research", 67.0, 88.0, 55.0),
}


def later(hours):
    return (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=hours)).isoformat()


class Api(http.server.BaseHTTPRequestHandler):
    """The usage, profile and token endpoints, for tokens shaped `at:<letter>:<n>`."""

    def log_message(self, *args):
        pass

    def answer(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def letter(self):
        parts = self.headers.get("Authorization", "").removeprefix("Bearer ").split(":")
        return parts[1] if len(parts) == 3 and parts[0] == "at" and parts[1] in ACCOUNTS else None

    def do_GET(self):
        letter = self.letter()
        if letter is None:
            return self.answer(401, {})
        email, _, five, week, fable = ACCOUNTS[letter]
        if self.path == "/api/oauth/usage":
            return self.answer(200, {
                "five_hour": {"utilization": five, "resets_at": later(1 + ord(letter) % 3)},
                "seven_day": {"utilization": week, "resets_at": later(20 + 30 * (ord(letter) % 4))},
                "limits": [{"kind": "weekly_scoped", "percent": fable, "resets_at": later(20 + 30 * (ord(letter) % 4)),
                            "scope": {"model": {"display_name": "Fable"}}}],
            })
        if self.path == "/api/oauth/profile":
            return self.answer(200, {
                "account": {"uuid": f"uuid-{letter}", "email": email},
                "organization": {"uuid": f"org-{letter}", "name": "Example", "organization_type": "claude_max",
                                 "rate_limit_tier": "default_claude_max_20x"},
            })
        self.answer(404, {})

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        token = json.loads(self.rfile.read(length) or b"{}").get("refresh_token", "")
        kind, letter, generation = (token.split(":") + ["", "", ""])[:3]
        if self.path != "/v1/oauth/token" or kind != "rt" or letter not in ACCOUNTS:
            return self.answer(400, {"error": "invalid_grant"})
        n = int(generation) + 1
        self.answer(200, {"access_token": f"at:{letter}:{n}", "refresh_token": f"rt:{letter}:{n}", "expires_in": 28800,
                          "scope": "user:inference user:profile",
                          "account": {"uuid": f"uuid-{letter}", "email_address": ACCOUNTS[letter][0]},
                          "organization": {"uuid": f"org-{letter}"}})


def transcripts(home):
    """A week of made-up sessions: models, projects, tools, subagents, hooks, stops and summaries."""
    rng = random.Random(7)
    projects = ["/work/storefront", "/work/billing-api", "/work/ios-app", "/work/docs-site"]
    models = ["claude-fable-5-2"] * 5 + ["claude-opus-5-5"] * 3 + ["claude-haiku-5-5"]
    tools = ["Bash"] * 6 + ["Read"] * 5 + ["Edit"] * 4 + ["Grep"] * 2 + ["Agent", "WebSearch", "Write"]
    now = datetime.datetime.now(datetime.timezone.utc)
    request = 0
    for session in range(34):
        letter = rng.choice("AAAABBCD")
        cwd = rng.choice(projects)
        folder = home / ".claude" / "projects" / cwd.replace("/", "-")
        folder.mkdir(parents=True, exist_ok=True)
        at = now - datetime.timedelta(hours=rng.uniform(0.3, 160))
        model = rng.choice(models)
        lines = [{"type": "session-owner", "accountUuid": f"uuid-{letter}", "sessionId": f"s{session}"}]
        cached = 0
        for turn in range(rng.randint(6, 40)):
            # A long pause now and then: the cache has expired and is rebuilt.
            at += datetime.timedelta(seconds=rng.choice([20, 45, 90, 150, 240] * 6 + [900, 4200]))
            stamp = at.isoformat()
            if turn and rng.random() < 0.06:
                lines.append({"type": "user", "timestamp": stamp, "cwd": cwd,
                              "message": {"role": "user", "content": "[Request interrupted by user]"}})
            if turn and rng.random() < 0.03:
                lines.append({"type": "system", "subtype": "compact_boundary", "timestamp": stamp})
                cached = 0
            request += 1
            grew = rng.randint(800, 9000)
            rebuilt = cached == 0 or rng.random() < 0.07
            usage = {"input_tokens": rng.randint(4, 60), "output_tokens": rng.randint(150, 2600),
                     "cache_creation_input_tokens": cached + grew if rebuilt else grew,
                     "cache_read_input_tokens": 0 if rebuilt else cached}
            cached += grew
            content = [{"type": "tool_use", "name": rng.choice(tools), "input": {}}] if rng.random() < 0.8 else \
                [{"type": "text", "text": "demo"}]
            lines.append({"type": "assistant", "timestamp": stamp, "cwd": cwd, "requestId": f"r{request}",
                          "message": {"model": model, "content": content, "usage": usage}})
            if rng.random() < 0.35:
                lines.append({"type": "attachment", "timestamp": stamp, "attachment": {
                    "type": "hook_success", "hookEvent": rng.choice(["PreToolUse", "PostToolUse", "Stop"]),
                    "durationMs": rng.randint(20, 900)}})
        # Compact, as Claude Code writes it: cx finds line types by their exact bytes.
        text = "".join(json.dumps(line, separators=(",", ":")) + "\n" for line in lines)
        (folder / f"s{session}.jsonl").write_text(text)


# ---- ANSI to SVG -------------------------------------------------------------

BASE16 = ["#1d2330", "#e06c75", "#7ec699", "#e5c07b", "#6cb6ff", "#c792ea", "#56c2d6", "#c9d1d9",
          "#5c6773", "#ff7b86", "#8fe0aa", "#f2d48f", "#8cc8ff", "#d9a9f5", "#7ad9ea", "#f0f3f6"]
FG, BG = "#c9d1d9", "#0f131a"


def colour256(n):
    if n < 16:
        return BASE16[n]
    if n < 232:
        n -= 16
        levels = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (levels[n // 36], levels[n // 6 % 6], levels[n % 6])
    grey = 8 + 10 * (n - 232)
    return "#%02x%02x%02x" % (grey, grey, grey)


def cells(line):
    """One captured line as (character, fg, bg, bold, dim) per cell."""
    out, fg, bg, bold, dim, reverse = [], None, None, False, False, False
    for part in re.split(r"(\x1b\[[0-9;:]*m)", line):
        if not part.startswith("\x1b["):
            for ch in part:
                f, b = (bg or BG, fg or FG) if reverse else (fg, bg)
                out.append((ch, f, b, bold, dim))
            continue
        codes = [int(c) if c else 0 for c in re.split("[;:]", part[2:-1])] or [0]
        i = 0
        while i < len(codes):
            c = codes[i]
            if c == 0:
                fg, bg, bold, dim, reverse = None, None, False, False, False
            elif c == 1:
                bold = True
            elif c == 2:
                dim = True
            elif c == 22:
                bold = dim = False
            elif c == 7:
                reverse = True
            elif c == 27:
                reverse = False
            elif 30 <= c <= 37:
                fg = BASE16[c - 30]
            elif 90 <= c <= 97:
                fg = BASE16[c - 82]
            elif c == 39:
                fg = None
            elif 40 <= c <= 47:
                bg = BASE16[c - 40]
            elif 100 <= c <= 107:
                bg = BASE16[c - 92]
            elif c == 49:
                bg = None
            elif c in (38, 48) and i + 1 < len(codes):
                if codes[i + 1] == 5 and i + 2 < len(codes):
                    value, i = colour256(codes[i + 2]), i + 2
                elif codes[i + 1] == 2 and i + 4 < len(codes):
                    value, i = "#%02x%02x%02x" % tuple(codes[i + 2:i + 5]), i + 4
                else:
                    value = None
                if c == 38:
                    fg = value
                else:
                    bg = value
            i += 1
    return out


def svg(screen, title):
    """The captured screen as an SVG: a window frame, then one text run per change of style."""
    lines = [cells(line) for line in screen.split("\n")]
    while lines and not "".join(c[0] for c in lines[-1]).strip():
        lines.pop()
    cw, ch, pad, bar = 8.0, 17.0, 14, 30
    width = max((len(line) for line in lines), default=1) * cw + 2 * pad
    height = len(lines) * ch + 2 * pad + bar
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width:.0f} {height:.0f}" width="{width:.0f}" '
           f'height="{height:.0f}" role="img" aria-label="{html.escape(title)}">',
           f"<title>{html.escape(title)}</title>",
           f'<rect width="100%" height="100%" rx="9" fill="{BG}"/>',
           f'<rect width="100%" height="{bar}" rx="9" fill="#1a2029"/><rect y="{bar - 9}" width="100%" height="9" fill="#1a2029"/>',
           '<circle cx="18" cy="15" r="5.5" fill="#ff5f57"/><circle cx="36" cy="15" r="5.5" fill="#febc2e"/>'
           '<circle cx="54" cy="15" r="5.5" fill="#28c840"/>',
           f'<text x="{width / 2:.0f}" y="19.5" text-anchor="middle" fill="#8b949e" font-size="12" '
           f'font-family="-apple-system,Segoe UI,Helvetica,Arial,sans-serif">{html.escape(title)}</text>',
           '<g font-family="ui-monospace,SFMono-Regular,Menlo,Consolas,Liberation Mono,monospace" font-size="13.2" '
           'xml:space="preserve">']
    for row, line in enumerate(lines):
        y = bar + pad + row * ch
        col = 0
        while col < len(line):
            _, fg, bg, bold, dim = line[col]
            end = col
            while end < len(line) and line[end][1:] == line[col][1:]:
                end += 1
            text = "".join(c[0] for c in line[col:end])
            x, w = pad + col * cw, (end - col) * cw
            if bg:
                out.append(f'<rect x="{x:.1f}" y="{y:.1f}" width="{w + 0.5:.1f}" height="{ch}" fill="{bg}"/>')
            # One element per group of words: a renderer that collapses runs of spaces
            # would otherwise stretch the letters to fill the width.
            for words in re.finditer(r"\S+(?: \S+)*", text):
                body = words.group()
                attrs = (f'x="{x + words.start() * cw:.1f}" y="{y + 12.6:.1f}" textLength="{len(body) * cw:.1f}" '
                         f'lengthAdjust="spacingAndGlyphs" fill="{fg or FG}"')
                if bold:
                    attrs += ' font-weight="700"'
                if dim:
                    attrs += ' opacity="0.62"'
                out.append(f"<text {attrs}>{html.escape(body)}</text>")
            col = end
    out.append("</g></svg>")
    return "\n".join(out) + "\n"


# ---- driving cx --------------------------------------------------------------


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--cx", default=str(ROOT / "target" / "debug" / "cx"), help="the cx binary to picture")
    parser.add_argument("--keep", action="store_true", help="keep the demo home and print where it is")
    args = parser.parse_args()
    cx = pathlib.Path(args.cx).resolve()
    if not cx.is_file():
        sys.exit(f"{cx} is not built: run `cargo build` first")
    if not shutil.which("tmux"):
        sys.exit("tmux is needed to run the dashboard off screen")

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Api)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{server.server_port}"
    home = pathlib.Path(tempfile.mkdtemp(prefix="cx-demo-"))
    env = {"PATH": os.environ["PATH"], "HOME": str(home), "USER": "ada", "CX_HOME": str(home / ".cx"),
           "CX_SECRETS": "file", "CX_API_BASE": base, "CX_TOKEN_URL": f"{base}/v1/oauth/token",
           "CX_CLAUDE_BIN": "/usr/bin/true", "CX_NOTIFY_LOG": str(home / "notify.log"),
           "TERM": "xterm-256color", "COLORTERM": "truecolor", "LANG": "en_US.UTF-8"}

    def run(*argv, stdin=None):
        done = subprocess.run([str(cx), *argv], env=env, input=stdin, capture_output=True, text=True)
        if done.returncode != 0:
            sys.exit(f"cx {' '.join(argv)} failed:\n{done.stdout}{done.stderr}")
        return done.stdout

    try:
        (home / ".claude").mkdir()
        expires = int(time.time() * 1000) + 8 * HOUR_MS
        for letter, (email, alias, *_ignored) in ACCOUNTS.items():
            (home / ".claude" / ".credentials.json").write_text(json.dumps({"claudeAiOauth": {
                "accessToken": f"at:{letter}:1", "refreshToken": f"rt:{letter}:1", "expiresAt": expires,
                "scopes": ["user:inference", "user:profile"], "subscriptionType": "max"}}))
            (home / ".claude.json").write_text(json.dumps({"oauthAccount": {
                "accountUuid": f"uuid-{letter}", "emailAddress": email, "organizationUuid": f"org-{letter}",
                "organizationName": "Example", "displayName": alias}}))
            run("add")
        for number, (_, alias, *_ignored) in enumerate(ACCOUNTS.values(), start=1):
            run("alias", str(number), alias)
        run("add-key", "--label", "console", stdin="sk-ant-api03-demo-key\n")
        transcripts(home)
        # The account with the most history is the one in use, so its cards are full.
        run("use", "1")
        run("ls", "--fresh")

        OUT.mkdir(parents=True, exist_ok=True)
        session = f"cx-shots-{os.getpid()}"
        tmux = lambda *argv: subprocess.run(["tmux", *argv], check=True, capture_output=True, text=True).stdout
        exports = " ".join(f"{k}={subprocess.list2cmdline([v])}" for k, v in env.items())

        def picture(name, title, command, keys=(), wait=4.0):
            tmux("new-session", "-d", "-s", session, "-x", str(COLS), "-y", str(ROWS), f"env -i {exports} {command}")
            try:
                time.sleep(wait)
                for key in keys:
                    tmux("send-keys", "-t", session, key)
                    time.sleep(1.2)
                screen = tmux("capture-pane", "-t", session, "-e", "-p")
            finally:
                subprocess.run(["tmux", "kill-session", "-t", session], capture_output=True)
            if not screen.strip():
                sys.exit(f"{name}: the screen was empty")
            (OUT / f"{name}.svg").write_text(svg(screen, title))
            print(f"wrote docs/screenshots/{name}.svg")

        picture("dashboard", "cx: the dashboard", str(cx))
        picture("waste", "cx: waste and speed", str(cx), keys=["v"])
        picture("settings", "cx: settings", str(cx), keys=["S"])
        picture("list", "cx ls · cx why", f"sh -c '{cx} ls --offline; echo; {cx} why; sleep 30'", wait=3.0)
    finally:
        server.shutdown()
        if args.keep:
            print(f"demo home kept: {home}")
        else:
            shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    main()
