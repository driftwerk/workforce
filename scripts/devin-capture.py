#!/usr/bin/env python3
"""Record what a signed-in Devin CLI sends its hooks, for Desk's Devin engine.

Run it on a computer where `devin` is installed and signed in:

    python3 <(curl -fsSL https://raw.githubusercontent.com/driftwerk/workforce/main/scripts/devin-capture.py)

It starts Devin twice, in a new scratch folder under your home folder:

1. Devin asks you as usual. Trust the folder if it asks, approve each
   request with "Once" (never "Always": that would save a rule to your
   config), pick any colour, and exit Devin when it says DONE (/exit).
2. It resumes the same session. This time the test hook answers for Devin:
   don't press anything until it says DONE, then exit as before. If Devin
   asks you anything anyway, approve it Once, and pick Red.

The hooks are added the way Desk will add them: your own Devin config is
copied whole into a private file passed with `--config`, and the hooks are
appended to yours. Your own config and credentials are never written; the
copy is deleted after each run, whatever happens.

What it writes is one file, `devin-capture.json`: the hook payloads, field
names and timings, `devin --version`, the labels (never the values) of
`devin auth status`, and `devin list --format json`. Before it is written,
home paths, your user, full and computer names, emails, ids, tokens, MCP
server names and any value under a key that names a secret are replaced.
Nothing is sent anywhere: you send the file yourself.

`devin` is looked up at ~/.local/bin/devin (where its installer puts it), or
at the path in DEVIN_CAPTURE_BIN. It is never looked up on PATH.

Plan mode isn't covered: Devin can't be started in it.
"""

import getpass
import hashlib
import json
import os
import re
import shutil
import signal
import socket
import stat
import subprocess
import sys
import time

# The events Devin has documented longest (docs.devin.ai, lifecycle hooks).
# An event an older CLI doesn't know could lose every hook in the file.
EVENTS = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "Stop",
    "SessionEnd",
]

# The answer the test hook gives the second run's question: not the one a
# person is asked to pick, so the reply says who answered.
HOOK_ANSWER = "Green"
PERSON_ANSWER = "Red"
# How long the second run's hook holds a permission request before it
# approves it: whether Devin waits on it shows in the timings.
HOLD_SECS = 3

FIRST_PROMPT = (
    "This is a short test of Desk's hooks. Do exactly these steps, one at a time: "
    "1) Create a file named hello.txt containing the word hello. "
    "2) Edit hello.txt so it says hello world. "
    "3) Run the shell command `ls -a`. "
    "4) Use your ask_user_question tool to ask me which colour I prefer, "
    "with the options Red, Green and Blue. "
    "5) Reply with one sentence naming the colour I picked, then say DONE."
)

SECOND_PROMPT = (
    "Once more, one step at a time: "
    "1) Edit hello.txt so it says hello again. "
    "2) Run the shell command `echo second`. "
    "3) Use your ask_user_question tool to ask me which colour I prefer, "
    "with the options Red, Green and Blue. "
    "4) Reply with one sentence naming the colour I picked, then say DONE."
)

# The hook Devin runs, written next to the config: it logs what it was sent
# and, in the second run, answers as Desk will.
HOOK = r'''
import json, os, sys, time
run, event, log = sys.argv[1], sys.argv[2], sys.argv[3]
start = time.time()
raw = sys.stdin.read()
try:
    payload = json.loads(raw)
except Exception:
    payload = {"_unreadable": raw[:4000]}
answer = {}
if run == "decide":
    tool = payload.get("tool_name") if isinstance(payload, dict) else None
    if event == "PermissionRequest":
        time.sleep(HOLD_SECS)
        answer = {"decision": "approve"}
    elif event == "PreToolUse" and tool == "ask_user_question":
        answer = {"decision": "block", "reason": "The person answered: HOOK_ANSWER"}
record = {"run": run, "event": event, "start": start, "end": time.time(),
          "answered": answer, "payload": payload}
# The script makes the log before the runs and removes it after: a hook
# that runs once the capture is over finds no log, and leaves none.
try:
    fd = os.open(log, os.O_WRONLY | os.O_APPEND)
except FileNotFoundError:
    sys.exit(0)
with os.fdopen(fd, "a") as f:
    f.write(json.dumps(record) + "\n")
sys.stdout.write(json.dumps(answer))
'''.replace("HOLD_SECS", str(HOLD_SECS)).replace("HOOK_ANSWER", HOOK_ANSWER)


def say(text):
    print("\n\033[1m" + text + "\033[0m", flush=True)


def private_file(path, text):
    """Writes `text` to `path`, which only this user can read."""
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(text)


def remove(path):
    try:
        if os.path.isdir(path) and not os.path.islink(path):
            shutil.rmtree(path, ignore_errors=True)
        else:
            os.remove(path)
    except FileNotFoundError:
        pass


def strip_jsonc(text):
    """JSON with `//` and `/* */` comments and trailing commas, as plain JSON:
    nothing inside a string is touched."""
    out, i, n = [], 0, len(text)

    def next_significant(j):
        while j < n:
            if text[j].isspace():
                j += 1
            elif text.startswith("//", j):
                while j < n and text[j] != "\n":
                    j += 1
            elif text.startswith("/*", j):
                end = text.find("*/", j + 2)
                j = n if end < 0 else end + 2
            else:
                return text[j]
        return ""

    while i < n:
        c = text[i]
        if c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            out.append(text[i : j + 1])
            i = j + 1
        elif text.startswith("//", i):
            while i < n and text[i] != "\n":
                i += 1
        elif text.startswith("/*", i):
            end = text.find("*/", i + 2)
            i = n if end < 0 else end + 2
        elif c == "," and next_significant(i + 1) in ("}", "]"):
            i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


def devin_config_dir(home):
    if sys.platform == "win32" and os.environ.get("APPDATA"):
        return os.path.join(os.environ["APPDATA"], "devin")
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.join(home, ".config")
    return os.path.join(base, "devin")


def person_config(home):
    """The person's own Devin config, parsed; empty when there is none."""
    path = os.path.join(devin_config_dir(home), "config.json")
    try:
        with open(path) as f:
            text = f.read()
    except FileNotFoundError:
        return {}, False
    try:
        value = json.loads(strip_jsonc(text))
        return (value if isinstance(value, dict) else {}), True
    except ValueError:
        say("Your Devin config couldn't be read; the capture runs without it.")
        return {}, True


def fingerprint(home):
    """A hash of each of the person's Devin config files, to tell whether a
    run changed one."""
    out = {}
    for name in ("config.json", "mcp_config.json"):
        try:
            with open(os.path.join(devin_config_dir(home), name), "rb") as f:
                out[name] = hashlib.sha256(f.read()).hexdigest()
        except OSError:
            out[name] = None
    return out


def config_for(run, mine, hook, log):
    """The person's config with the logging hooks appended to theirs."""
    config = json.loads(json.dumps(mine))
    hooks = config.get("hooks")
    if not isinstance(hooks, dict):
        hooks = {}
    for event in EVENTS:
        command = " ".join(
            shell_quote(a) for a in [sys.executable, "-I", hook, run, event, log]
        )
        entry = {"hooks": [{"type": "command", "command": command, "timeout": 30}]}
        groups = hooks.get(event)
        hooks[event] = (groups if isinstance(groups, list) else []) + [entry]
    config["hooks"] = hooks
    return config


def shell_quote(s):
    if sys.platform == "win32":
        return '"' + s.replace('"', '\\"') + '"'
    return "'" + s.replace("'", "'\"'\"'") + "'"


def terminal():
    """This terminal, even when the script itself came in on stdin."""
    if sys.stdin.isatty():
        return None
    try:
        return open("/dev/tty", "r+")
    except OSError:
        return None


def run_devin(devin, args, cwd):
    """Devin in this terminal, until the person exits it. A Ctrl+C is
    Devin's (it clears a draft first), never this script's."""
    tty = terminal()
    previous = signal.signal(signal.SIGINT, signal.SIG_IGN)
    child = None
    try:
        child = subprocess.Popen(
            [devin] + args,
            cwd=cwd,
            stdin=tty,
            stdout=tty,
            stderr=tty,
            preexec_fn=lambda: signal.signal(signal.SIGINT, signal.SIG_DFL),
        )
        return child.wait()
    finally:
        # Whatever stopped this script stops Devin too.
        if child is not None and child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        signal.signal(signal.SIGINT, previous)
        if tty:
            tty.close()


def output_of(devin, args, cwd):
    try:
        p = subprocess.run(
            [devin] + args,
            cwd=cwd,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=20,
        )
        return {"exit": p.returncode, "stdout": p.stdout, "stderr": p.stderr}
    except Exception as e:  # noqa: BLE001 - recorded, not raised
        return {"error": type(e).__name__}


def read_log(log):
    records = []
    try:
        with open(log) as f:
            for line in f:
                line = line.strip()
                if line:
                    try:
                        record = json.loads(line)
                    except ValueError:
                        record = None
                    records.append(record if isinstance(record, dict) else {"_unreadable": True})
    except FileNotFoundError:
        pass
    return records


def ask_yes_no(question):
    tty = terminal()
    try:
        if tty:
            tty.write(question + " [y/n] ")
            tty.flush()
            reply = tty.readline()
        else:
            reply = input(question + " [y/n] ")
    except (EOFError, OSError):
        return None
    finally:
        if tty:
            tty.close()
    reply = reply.strip().lower()
    return reply.startswith("y") if reply else None


# The only words a label of `devin auth status` is kept with. Anything else
# could be a name: of the person, their org or their team.
LABEL_WORDS = set(
    """
    a account accounts active an and api are as at auth authenticated by config
    created credential credentials current default device display email expires
    expiry false file for full has host id in is key keys keychain logged login
    method mode name no none not of oauth on org orgs organization organizations
    out path plan primary provider region role roles scope scopes server session
    signed source sso status stored subscription team teams the tier to token
    true type updated url user username using valid verified via webapp with
    workspace workspaces yes you title date time count repo branch state kind
    model last first summary description message messages cost duration
    """.split()
)

# Things a computer's name is made of that name no one, and that model names
# (`gemini-2.5-pro`) are made of too.
MACHINE_WORDS = {"macbook", "pro", "air", "mini", "imac", "studio", "max", "ultra", "mac"}

# Words never scrubbed as a name, even when they are one (a user called
# `dev`, a role of `admin`): they are what field names and values are made of.
COMMON_WORDS = LABEL_WORDS | MACHINE_WORDS | set(
    """
    user users dev admin administrator root test tester guest owner member
    members viewer editor free enterprise personal null ubuntu runner devin
    cognition local home work shared staff everyone wheel git main master
    com org net edu gov
    """.split()
)


def plain_label(label):
    """Whether `label` is made only of LABEL_WORDS (any case, snake, kebab or
    camel), so it can say nothing of anyone."""
    if not re.match(r"^[A-Za-z][A-Za-z _()/-]*$", label):
        return False
    words = re.findall(r"[A-Z]?[a-z]+|[A-Z]+(?![a-z])", label)
    return bool(words) and all(w.lower() in LABEL_WORDS for w in words)


def labels_of(said, thrown=None):
    """`devin auth status`'s lines as their labels alone. A label is kept
    only if each of its words is a LABEL_WORD; its value never is. A line
    that isn't a label and a value is kept only if all its words are
    LABEL_WORDS, and otherwise counted. What is left out goes to `thrown`,
    so it is scrubbed from everything else too."""
    thrown = [] if thrown is None else thrown
    lines = []
    for line in said.splitlines():
        line = line.strip()
        if not line:
            continue
        m = re.match(r"^([^:]{1,40}?)\s*:\s*(.*)$", line) or re.match(
            r"^(\S.{0,38}?\S)\s{2,}(\S.*)$", line
        )
        if m:
            if plain_label(m.group(1)):
                label = m.group(1)
            else:
                label = "<label>"
                thrown.append(m.group(1))
            if m.group(2):
                thrown.append(m.group(2))
            lines.append(label + (": <value>" if m.group(2) else ":"))
        elif plain_label(line):
            lines.append(line)
        else:
            thrown.extend(w for w in line.split() if w.lower() not in LABEL_WORDS)
            n = len(line.split())
            lines.append("<line: %d word%s>" % (n, "" if n == 1 else "s"))
    return lines


def key_paths(value, thrown=None, prefix="", strings=True):
    """A JSON value's key paths (`a.b`, `a[].c`), each key kept only if it is
    a plain label: a key could be an org's name. Keys left out, and strings
    when `strings`, go to `thrown`."""
    paths = set()
    if isinstance(value, dict):
        for k, x in value.items():
            plain = plain_label(k)
            if not plain and thrown is not None:
                thrown.append(k)
            path = (prefix + "." if prefix else "") + (k if plain else "<key>")
            paths.add(path)
            paths |= key_paths(x, thrown, path, strings)
    elif isinstance(value, list):
        for x in value:
            paths |= key_paths(x, thrown, prefix + "[]", strings)
    elif isinstance(value, str) and strings and thrown is not None and not plain_label(value):
        thrown.append(value)
    return paths


SIGNED_IN_KEY = re.compile(r"(?i)^(is_?)?((logged|signed)_?in|authenticated)$")


def auth_status(raw, thrown=None):
    thrown = [] if thrown is None else thrown
    out = {k: raw.get(k) for k in ("exit", "error") if k in raw}
    stdout, stderr = raw.get("stdout") or "", raw.get("stderr") or ""
    try:
        parsed = json.loads(stdout)
    except ValueError:
        parsed = None
    if isinstance(parsed, (dict, list)):
        # JSON says itself by its keys; none of its values are kept.
        out["jsonKeyPaths"] = sorted(key_paths(parsed, thrown))
        flags = [
            v for k, v in (parsed.items() if isinstance(parsed, dict) else [])
            if SIGNED_IN_KEY.match(k) and isinstance(v, bool)
        ]
        out["signedIn"] = flags[0] if flags else None
        out["labels"] = labels_of(stderr, thrown)
        return out
    lower = (stdout + "\n" + stderr).lower()
    out["signedIn"] = (
        False
        if re.search(r"not (logged|signed) in", lower)
        else (True if re.search(r"(logged|signed) in", lower) else None)
    )
    out["labels"] = labels_of(stdout + "\n" + stderr, thrown)
    return out


def session_list(raw, session=None, thrown=None):
    """`devin list` as its shape alone: key paths and a count for JSON, a
    line count otherwise. Nothing it says is kept; the names in it (keys
    that aren't labels, `/org/<name>` in a URL) go to `thrown`, and the
    parsed list is returned too, for its NAME_KEYS."""
    thrown = [] if thrown is None else thrown
    out = {k: raw.get(k) for k in ("exit", "error") if k in raw}
    stdout = raw.get("stdout") or ""
    try:
        parsed = json.loads(stdout)
    except ValueError:
        parsed = None
    if isinstance(parsed, (dict, list)):
        out["jsonKeyPaths"] = sorted(key_paths(parsed, thrown, strings=False))
        for found in NAMED_SEGMENT.findall(stdout) + GIT_SSH.findall(stdout):
            thrown.extend(found[1:3])
        if isinstance(parsed, list):
            out["entries"] = len(parsed)
            out["listsThisSession"] = bool(session) and any(
                session in json.dumps(e) for e in parsed
            )
    else:
        out["stdoutLines"] = len(stdout.splitlines())
    out["stderrLines"] = len((raw.get("stderr") or "").splitlines())
    return out, parsed


SECRET_KEYS = re.compile(
    r"(?i)(token|secret|passw(or)?d|api[_-]?key|authori[sz]ation|auth(?!or)|bearer|cookie|"
    r"credential|session[_-]?key|private|signature|(?-i:[a-z0-9]Key)$|(^|[_-])(key|pat)$)"
)
# Whatever is under these is the person's: their names are kept, not their
# values.
WHOLE_KEYS = re.compile(r"(?i)(^|_)(env|environment|env_vars|headers|extra_headers)$")
# A key whose value names someone: an org, a team, a user, an author.
NAME_KEYS = re.compile(
    r"(?i)(^|_)(org|orgs|organization|team|workspace|account|user|username|owner|"
    r"created_by|updated_by|author|committer|display_name|full_name|login|email)"
    r"(_(name|id|display_name|login|email|slug|handle))?$"
)
# Starting only where a run starts: linear on a long run with no `@`.
EMAIL = re.compile(r"(?<![A-Za-z0-9._%+-])[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
UUID = re.compile(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
JWT = re.compile(r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+")
# `token: x`, `Bearer x`: a separator, then a value with a digit in it (so
# `token_count` and `token budget` stay as they are).
BEARER = re.compile(
    r"(?i)(?<![A-Za-z0-9])(bearer|token|basic)(\s+|\s*[:=]\s*)"
    r"((?=[A-Za-z0-9._~+/=-]*[0-9])[A-Za-z0-9._~+/=-]{8,})"
)
# `name=value`, `name: "value"`, `\"name\": \"value\"`: judged by the name.
KEY_VALUE = re.compile(
    r"(?<![A-Za-z0-9_.-])([A-Za-z_][A-Za-z0-9_.-]{0,63})(\\?[\"']?[ \t]*[:=][ \t]*)"
    r"(?:(\\?[\"'])([^\"'\\\n]+)|([^\s\"'\\,;&}\])<]+))"
)
NOT_A_SECRET = re.compile(r"(?i)^(-?[0-9.]+|true|false|null|none|bearer|basic|token)$")
# The vendors' key shapes, exactly: case, separator and length.
VENDOR_KEY = re.compile(
    r"\b(sk-[A-Za-z0-9_-]{20,}|gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{20,}"
    r"|xox[abpr]-[A-Za-z0-9-]{10,}|glpat-[A-Za-z0-9_-]{20,}|(AKIA|ASIA)[0-9A-Z]{16}\b)"
)
QUERY_SECRET = re.compile(
    r"(?i)([?&;](?:[a-z_]*(?:token|key|secret|sig|code|password|auth)[a-z_]*)=)([^&\s\"']+)"
)
# `git@host:user/repo`, and `https://github.com/user/repo`.
GIT_SSH = re.compile(r"(?<![\w.-])([\w.-]+@[A-Za-z0-9.-]+):([\w.-]+?)/([\w.-]+?)(\.git)?(?![\w.-])")
GIT_HTTPS = re.compile(
    r"(?i)(https?://(?:[^/@\s]+@)?(?:www\.)?(?:github\.com|gitlab\.com|bitbucket\.org)/)"
    r"([\w.-]+)(?:/([\w.-]+?)(\.git)?(?![\w.-]))?"
)
URL_USER = re.compile(r"(?i)(\b[a-z][a-z0-9+.-]*://)([^/@\s:]+)(:[^/@\s]*)?@")
# A path segment after one of these names someone: `/org/acme/`, `/home/alice`.
NAMED_SEGMENT = re.compile(
    r"(?i)(/(?:orgs?|organi[sz]ations?|teams?|workspaces?|users?|u|accounts?|home|members?)/)"
    r"([^/\s?#\"'<>]+)"
)
# Long runs: whether one is a key, token or hash is decided by `long_run`.
LONG = re.compile(r"[A-Za-z0-9+/_-]{32,}={0,2}")
MCP = re.compile(r"\bmcp__([A-Za-z0-9_-]+?)__")


def mixed_random(run):
    """A digit, and upper and lower case in fair share: base64, not a name
    (camelCase has far fewer capitals)."""
    letters = [c for c in run if c.isalpha()]
    if not letters or not any(c.isdigit() for c in run):
        return False
    upper = sum(c.isupper() for c in letters)
    return upper >= 0.2 * len(letters) and len(letters) - upper >= 0.2 * len(letters)


def looks_random(run):
    """Whether a run with no `/` in it reads as a key, token or hash rather
    than a name: base64-like, or mostly digits and hex."""
    if mixed_random(run):
        return True
    digits = sum(c.isdigit() for c in run)
    if not digits:
        return False
    hexish = sum(c in "0123456789abcdefABCDEF" for c in run)
    return hexish >= 0.9 * len(run) or digits >= 0.25 * len(run)


def name_parts(value):
    """`value` and its parts of three or more characters, as words to
    scrub: never a common word, a number, or a sentence."""
    if not isinstance(value, str) or len(value) > 80:
        return []
    out = []
    # An address's domain is a provider's (gmail, com), not the person's.
    local = value.split("@")[0] if EMAIL.fullmatch(value.strip()) else value
    for w in [value.strip()] + re.split(r"[^A-Za-z0-9]+", local):
        w = re.sub(r"['\u2019]s$", "", w.strip(" \t\"'()[],"))
        if len(w) >= 3 and re.search(r"[A-Za-z]", w) and w.lower() not in COMMON_WORDS:
            out.append(w)
    return out


def git_config(key):
    if not shutil.which("git"):
        return ""
    try:
        return subprocess.run(
            ["git", "config", "--get", key],
            capture_output=True,
            text=True,
            timeout=5,
            stdin=subprocess.DEVNULL,
        ).stdout.strip()
    except Exception:  # noqa: BLE001
        return ""


def own_words():
    """The person's user, full, git and computer names: never in what is sent."""
    words = [getpass.getuser()]
    machine = {socket.gethostname().split(".")[0]}
    try:
        import pwd

        words.append(pwd.getpwuid(os.getuid()).pw_gecos.split(",")[0])
    except Exception:  # noqa: BLE001 - not every system has them
        pass
    words += [git_config("user.name"), git_config("user.email")]
    if sys.platform == "darwin":
        for key in ("ComputerName", "LocalHostName"):
            try:
                name = subprocess.run(
                    ["scutil", "--get", key],
                    capture_output=True,
                    text=True,
                    timeout=5,
                    stdin=subprocess.DEVNULL,
                ).stdout.strip()
            except Exception:  # noqa: BLE001
                name = ""
            if name:
                machine.add(name)
    # "Alice's MacBook Pro": Alice, not MacBook or Pro.
    return [p for w in words + sorted(machine) for p in name_parts(w)]


class Scrubber:
    """Replaces what could say who the person is, or let anyone in."""

    def __init__(self, home, folder=None, words=()):
        self.home = home
        self.folder = os.path.basename(folder) if folder else None
        self.words = set()
        self.add_words(own_words())
        self.add_words(words)
        self.seen = {}

    def add_words(self, values, keep=()):
        """Adds `values` (and their parts) to the names scrubbed; never one of
        `keep`, the payloads' own field names."""
        keep = {k.lower() for k in keep}
        for v in values:
            self.words.update(p for p in name_parts(v) if p.lower() not in keep)
        # Longest first, so a full name goes before its parts.
        self.ordered = sorted(self.words, key=len, reverse=True)

    def learn(self, value, keys=None):
        """The values under NAME_KEYS anywhere in `value`, as names; returns
        every key seen."""
        found, keys = [], set() if keys is None else keys

        def walk(v):
            if isinstance(v, list):
                for x in v:
                    walk(x)
            elif isinstance(v, dict):
                for k, x in v.items():
                    keys.add(k)
                    if NAME_KEYS.search(k) and isinstance(x, str):
                        found.append(x)
                    walk(x)

        walk(value)
        self.add_words(found, keep=keys)
        return keys

    def place(self, kind, found, fold=False):
        # Each one numbered, so the same value reads the same everywhere and
        # two never read the same.
        key = (kind, found.lower() if fold else found)
        if key not in self.seen:
            self.seen[key] = "<%s-%d>" % (kind, len(self.seen) + 1)
        return self.seen[key]

    def mark(self, kind, fold=False):
        return lambda m: self.place(kind, m.group(0), fold)

    def name(self, found):
        return self.place("name", found, fold=True) if found else found

    def long_run(self, m):
        run = m.group(0)
        if "/" not in run:
            return self.place("long", run) if looks_random(run) else run
        # Base64 first, the `/`s ignored; a path's segments only if it isn't.
        core = run.strip("/")
        lead, trail = run[: len(run) - len(run.lstrip("/"))], run[len(run.rstrip("/")) :]
        if len(core.replace("/", "")) >= 32 and mixed_random(core.replace("/", "")):
            return lead + self.place("long", core) + trail
        return "/".join(
            self.place("long", seg) if len(seg) >= 32 and looks_random(seg) else seg
            for seg in run.split("/")
        )

    def key_value(self, m):
        found = m.group(4) or m.group(5)
        if NOT_A_SECRET.match(found) or found.startswith("<"):
            return m.group(0)
        if SECRET_KEYS.search(m.group(1)):
            hidden = self.place("redacted", found)
        elif NAME_KEYS.search(m.group(1)) or re.match(r"(?i)^organi[sz]ation$", m.group(1)):
            hidden = self.name(found)
        else:
            return m.group(0)
        return m.group(1) + m.group(2) + (m.group(3) or "") + hidden

    def email(self, m):
        # `git@github.com:` is a remote's host, not an address.
        if m.group(0).lower().startswith("git@") and m.string[m.end() : m.end() + 1] == ":":
            return m.group(0)
        return self.place("email", m.group(0), fold=True)

    def text(self, s):
        # JSON in a string is scrubbed as JSON, so its keys judge its values.
        if s[:1] in "{[" and s.rstrip()[-1:] in "}]":
            try:
                parsed = json.loads(s)
            except ValueError:
                parsed = None
            if isinstance(parsed, (dict, list)):
                return json.dumps(self.value(parsed))
        if self.home and len(self.home) > 1:
            s = re.sub(re.escape(self.home) + r"(?=/|$|[\s\"'])", "~", s)
        if self.folder:
            s = s.replace(self.folder, "<capture-folder>")
        s = MCP.sub(lambda m: "mcp__" + self.place("server", m.group(1)) + "__", s)
        s = UUID.sub(self.mark("uuid", fold=True), s)
        s = JWT.sub(self.mark("token"), s)
        s = BEARER.sub(lambda m: m.group(1) + m.group(2) + self.place("token", m.group(3)), s)
        s = KEY_VALUE.sub(self.key_value, s)
        s = QUERY_SECRET.sub(lambda m: m.group(1) + self.place("redacted", m.group(2)), s)
        s = VENDOR_KEY.sub(self.mark("key"), s)
        s = GIT_SSH.sub(
            lambda m: m.group(1) + ":" + self.name(m.group(2)) + "/" + self.name(m.group(3)) + (m.group(4) or ""),
            s,
        )
        s = GIT_HTTPS.sub(
            lambda m: m.group(1) + self.name(m.group(2))
            + ("/" + self.name(m.group(3)) + (m.group(4) or "") if m.group(3) else ""),
            s,
        )
        s = URL_USER.sub(lambda m: m.group(1) + self.name(m.group(2)) + "@", s)
        s = NAMED_SEGMENT.sub(
            lambda m: m.group(1) + (m.group(2) if m.group(2).startswith("<") else self.name(m.group(2))),
            s,
        )
        s = EMAIL.sub(self.email, s)
        s = LONG.sub(self.long_run, s)
        for word in self.ordered:
            s = re.sub(
                r"(?<!\w)" + re.escape(word) + r"(?!\w)",
                lambda m: self.name(m.group(0)),
                s,
                flags=re.I,
            )
        return s

    def value(self, v, secret=False):
        """`v` scrubbed. Under a key that names a secret, every string is
        replaced; under one that names someone, it is a name. Keys, numbers,
        booleans and the shape are kept."""
        if isinstance(v, str):
            return "<redacted>" if secret and v else self.text(v)
        if isinstance(v, list):
            return [self.value(x, secret) for x in v]
        if isinstance(v, dict):
            out = {}
            for k, x in v.items():
                k2 = self.text(k) if isinstance(k, str) else k
                while k2 in out:  # never merge two keys into one
                    k2 = "%s#" % k2
                hidden = secret or bool(WHOLE_KEYS.search(k) or SECRET_KEYS.search(k))
                if not hidden and NAME_KEYS.search(k) and isinstance(x, str) and x:
                    out[k2] = self.name(x)
                else:
                    out[k2] = self.value(x, hidden)
            return out
        return v


def shapes(records):
    """Which fields each event, and each tool's input, came with."""
    events, tools = {}, {}
    for r in records:
        p = r.get("payload")
        if not isinstance(p, dict):
            continue
        events.setdefault(r.get("event"), set()).update(p.keys())
        name, given = p.get("tool_name"), p.get("tool_input")
        if isinstance(name, str) and isinstance(given, dict):
            tools.setdefault(name, set()).update(given.keys())
    return (
        {k: sorted(v) for k, v in events.items()},
        {k: sorted(v) for k, v in tools.items()},
    )


CALL_ID = re.compile(r"(?i)(call|use).?id$")


def payload_of(record):
    p = record.get("payload")
    return p if isinstance(p, dict) else {}


def findings(records, prompted_again, asked_again):
    """What the capture was for, read off the records."""
    ids = set()
    for r in records:
        p = r.get("payload")
        if isinstance(p, dict):
            ids.update(k for k in p if CALL_ID.search(k))

    def call_id(p):
        return next((p[k] for k in sorted(ids) if isinstance(p, dict) and p.get(k)), None)

    decide = [r for r in records if r.get("run") == "decide" and "start" in r]
    waits = []
    for i, r in enumerate(decide):
        if r.get("event") != "PermissionRequest":
            continue
        p = payload_of(r)
        tool, cid = p.get("tool_name"), call_id(p)

        def same(x):
            q = payload_of(x)
            if cid is not None:
                return call_id(q) == cid
            return q.get("tool_name") == tool

        after = next(
            (x for x in decide[i + 1 :] if x.get("event") == "PostToolUse" and same(x)),
            None,
        )
        waits.append(
            {
                "tool": tool,
                "heldSecs": round(r["end"] - r["start"], 2),
                "pairedBy": "callId" if cid is not None else "toolName",
                "toolEndedAfterTheAnswer": None if after is None else after["start"] >= r["end"],
            }
        )
    stops = [
        payload_of(r).get("last_assistant_message")
        for r in decide
        if r.get("event") == "Stop"
    ]
    said = " ".join(s for s in stops if isinstance(s, str))
    return {
        "callIdFields": sorted(ids),
        "cwdInPayloads": any(isinstance(r.get("payload"), dict) and "cwd" in r["payload"] for r in records),
        "stopHasLastAssistantMessage": any(
            isinstance(r.get("payload"), dict) and "last_assistant_message" in r["payload"]
            for r in records
            if r.get("event") == "Stop"
        ),
        "permissionRequestWaits": waits,
        "personSawApprovalPromptInSecondRun": prompted_again,
        "personSawQuestionInSecondRun": asked_again,
        "secondRunReplyNamesHookAnswer": HOOK_ANSWER.lower() in said.lower() if said else None,
        "secondRunReplyNamesPersonAnswer": PERSON_ANSWER.lower() in said.lower() if said else None,
        "sessionStartSources": [
            {"run": r.get("run"), "source": payload_of(r).get("source")}
            for r in records
            if r.get("event") == "SessionStart"
        ],
    }


def own_hooks(mine):
    """The person's own hooks and permission rules, counted, not shown: they
    run in the capture too, and may change what it reads."""
    hooks = mine.get("hooks") if isinstance(mine.get("hooks"), dict) else {}
    perms = mine.get("permissions") if isinstance(mine.get("permissions"), dict) else {}
    return {
        "ownHookEvents": sorted(hooks.keys()),
        "ownPermissionRuleCount": sum(
            len(v) for v in perms.values() if isinstance(v, list)
        ),
    }


def on_signal(signum, _frame):
    raise SystemExit(128 + signum)


def capture(devin, home, folder):
    work = os.path.join(folder, "work")
    hook = os.path.join(folder, "hook.py")
    log = os.path.join(folder, "hooks.jsonl")
    observe = os.path.join(folder, "observe.json")
    decide = os.path.join(folder, "decide.json")
    out = os.path.join(folder, "devin-capture.json")
    part = out + ".part"
    try:
        os.makedirs(work, mode=0o700)
        if shutil.which("git"):
            subprocess.call(
                ["git", "init", "-q", work], stdin=subprocess.DEVNULL
            )
        private_file(hook, HOOK)
        # The hooks only append to it: once it is removed, none can make it again.
        private_file(log, "")
        mine, had_config = person_config(home)
        before = fingerprint(home)

        version = output_of(devin, ["--version"], work)
        status = output_of(devin, ["auth", "status"], work)

        say(
            "Run 1 of 2: trust the folder if Devin asks, approve each request with Once "
            "(not Always), and pick any colour. Exit Devin (/exit) when it says DONE."
        )
        private_file(observe, json.dumps(config_for("observe", mine, hook, log)))
        try:
            first_exit = run_devin(
                devin,
                ["--config", observe, "--permission-mode", "normal", "--", FIRST_PROMPT],
                work,
            )
            time.sleep(1)  # for a hook still writing
        finally:
            remove(observe)

        if not read_log(log) and first_exit:
            say(
                "Devin ended (exit %s) before it ran anything, so there is nothing to "
                "capture. Check that `devin` starts on its own, then try again." % first_exit
            )
            return 1
        if not read_log(log):
            say(
                "Devin didn't run the test hooks, so there is nothing to capture. "
                "Update Devin (`devin update`, or its installer) and try again; "
                "if it still happens, send the output of `devin --version`."
            )
            return 1

        started = [
            payload_of(r).get("session_id")
            for r in read_log(log)
            if r.get("event") == "SessionStart"
        ]
        session = next((s for s in started if isinstance(s, str) and s), None)
        listed = output_of(devin, ["list", "--format", "json"], work)

        say(
            "Run 2 of 2: the test hook answers for Devin now. Don't press anything until it "
            "says DONE, then exit Devin. If it asks you anything anyway, approve it Once and "
            "pick %s." % PERSON_ANSWER
        )
        private_file(decide, json.dumps(config_for("decide", mine, hook, log)))
        resume = ["-r", session] if session else []
        try:
            second_exit = run_devin(
                devin,
                ["--config", decide, "--permission-mode", "normal"] + resume + ["--", SECOND_PROMPT],
                work,
            )
            time.sleep(1)
        finally:
            remove(decide)

        say("Two questions about run 2:")
        prompted = ask_yes_no("Did Devin show its own approval prompt in run 2?")
        asked = ask_yes_no("Did Devin show you the colour question in run 2?")

        records = read_log(log)
        events, tools = shapes(records)
        # What auth status and the list say of anyone is scrubbed everywhere.
        thrown = []
        status = auth_status(status, thrown)
        listed, entries = session_list(listed, session, thrown)
        result = {
            "capturedAt": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
            "platform": sys.platform,
            "hadOwnConfig": had_config,
            "userConfigChangedDuringRun": fingerprint(home) != before,
            "version": version,
            "authStatus": status,
            "list": listed,
            "resumedById": session is not None,
            "exits": [first_exit, second_exit],
            "findings": findings(records, prompted, asked),
            "eventFields": events,
            "toolInputFields": tools,
            "records": records,
        }
        result.update(own_hooks(mine))
        # The capture's own keys are its own ("authStatus" names no secret);
        # everything under them is scrubbed.
        scrub = Scrubber(home, folder)
        keys = scrub.learn(records)
        scrub.learn(entries, keys)
        scrub.add_words(thrown, keep=keys)
        result = {k: scrub.value(v) for k, v in result.items()}
        # Written whole or not at all: an interrupt never leaves half of it.
        private_file(part, json.dumps(result, indent=2))
        os.replace(part, out)
        if not session:
            say("Note: run 1 said no session id, so run 2 was a new session, not a resume.")
        say("Done. Send this one file (look it over first if you like):\n  " + out)
        return 0
    finally:
        # Nothing stops the clean-up.
        for s in (signal.SIGINT, signal.SIGHUP, signal.SIGTERM):
            signal.signal(s, signal.SIG_IGN)
        for path in (hook, log, observe, decide, part, work):
            remove(path)
        if not os.path.exists(out):
            remove(folder)


def main():
    home = os.path.expanduser("~")
    devin = os.environ.get("DEVIN_CAPTURE_BIN") or os.path.join(home, ".local", "bin", "devin")
    if not (os.path.isfile(devin) and os.access(devin, os.X_OK)):
        say("No Devin at %s. Install it, or set DEVIN_CAPTURE_BIN to its path." % devin)
        return 1
    for s in (signal.SIGHUP, signal.SIGTERM):
        signal.signal(s, on_signal)
    folder = os.path.join(
        home, "devin-capture-%s-%d" % (time.strftime("%Y%m%d-%H%M%S"), os.getpid())
    )
    os.makedirs(folder, mode=0o700)
    os.chmod(folder, stat.S_IRWXU)
    out = os.path.join(folder, "devin-capture.json")
    try:
        return capture(devin, home, folder)
    except KeyboardInterrupt:
        if os.path.exists(out):
            say("Stopped after the capture was written. Send this one file:\n  " + out)
        else:
            say("Stopped; nothing was kept.")
        return 130


if __name__ == "__main__":
    sys.exit(main())
