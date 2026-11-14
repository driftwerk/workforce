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
fd = os.open(log, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
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
                        records.append(json.loads(line))
                    except ValueError:
                        records.append({"_unreadable": True})
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


def labels_of(said):
    """`devin auth status` as its labels alone: each line's label with its
    value replaced, a sentence by its first words. Its shape is what Desk
    needs; nothing it says of the person is kept."""
    lines = []
    for line in said.splitlines():
        line = line.strip()
        if not line:
            continue
        m = re.match(r"^([^:]{1,40}?)\s*:\s*(.*)$", line) or re.match(
            r"^(\S.{0,38}?\S)\s{2,}(\S.*)$", line
        )
        if m:
            lines.append(m.group(1) + ": <value>" if m.group(2) else m.group(1) + ":")
            continue
        words = line.split()
        kept = words[:3]
        lines.append(" ".join(kept) + (" <…>" if len(words) > 3 else ""))
    return lines


def auth_status(raw):
    out = {k: raw.get(k) for k in ("exit", "error") if k in raw}
    said = (raw.get("stdout") or "") + "\n" + (raw.get("stderr") or "")
    lower = said.lower()
    out["signedIn"] = (
        False
        if re.search(r"not (logged|signed) in", lower)
        else (True if re.search(r"(logged|signed) in", lower) else None)
    )
    out["labels"] = labels_of(said)
    return out


SECRET_KEYS = re.compile(
    r"(token|secret|password|passwd|api[_-]?key|apikey|authori[sz]ation|auth|bearer|cookie|"
    r"credential|session_key|private|signature|(^|_)(key|pat)$)",
    re.I,
)
# Whatever is under these is the person's, wholesale.
WHOLE_KEYS = re.compile(r"^(env|headers)$", re.I)
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
UUID = re.compile(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
JWT = re.compile(r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+")
BEARER = re.compile(r"(?i)\b(bearer|token|basic)(\s*[:=]?\s*)[A-Za-z0-9._~+/=-]{8,}")
VENDOR_KEY = re.compile(
    r"(?i)\b(sk|pk|rk|ghp|gho|ghs|ghu|github_pat|xox[abpr]|glpat|dvn|cog|akia|asia)[-_A-Za-z0-9]{6,}"
)
QUERY_SECRET = re.compile(
    r"(?i)([?&;](?:[a-z_]*(?:token|key|secret|sig|code|password|auth)[a-z_]*)=)[^&\s\"']+"
)
# Long runs with a digit in them: keys, tokens, hashes. A path's segments
# are split by `/`; a base64 run with `/` in it is caught by the second.
LONG = re.compile(r"(?=[A-Za-z0-9_+=-]*[0-9])[A-Za-z0-9_+=-]{32,}")
LONG_B64 = re.compile(r"(?<![\w~/.])(?=[A-Za-z0-9+/_=-]*[0-9])(?=[A-Za-z0-9+/_=-]*[a-z])(?=[A-Za-z0-9+/_=-]*[A-Z])[A-Za-z0-9+/_=-]{32,}")
MCP = re.compile(r"\bmcp__(.+?)__")


def own_words():
    """The person's user, full and computer names: never in what is sent."""
    words = {getpass.getuser(), socket.gethostname().split(".")[0]}
    try:
        import pwd

        gecos = pwd.getpwuid(os.getuid()).pw_gecos.split(",")[0]
        words.add(gecos)
        words.update(gecos.split())
    except Exception:  # noqa: BLE001 - not every system has them
        pass
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
                words.add(name)
                words.update(name.replace("-", " ").split())
    # Longest first, so a full name goes before its parts.
    return sorted((w for w in words if w and len(w) > 2), key=len, reverse=True)


class Scrubber:
    """Replaces what could say who the person is, or let anyone in."""

    def __init__(self, home):
        self.home = home
        self.words = own_words()
        self.seen = {}

    def mark(self, kind):
        # Each one numbered, so the same id reads the same everywhere.
        def replace(m):
            key = (kind, m.group(0).lower())
            if key not in self.seen:
                self.seen[key] = "<%s-%d>" % (kind, len(self.seen) + 1)
            return self.seen[key]

        return replace

    def text(self, s):
        if self.home and len(self.home) > 1:
            s = s.replace(self.home, "~")
        s = MCP.sub(lambda m: "mcp__" + self.mark("server")(m) + "__", s)
        s = UUID.sub(self.mark("uuid"), s)
        s = JWT.sub("<token>", s)
        s = BEARER.sub(lambda m: m.group(1) + m.group(2) + "<token>", s)
        s = QUERY_SECRET.sub(lambda m: m.group(1) + "<redacted>", s)
        s = VENDOR_KEY.sub("<key>", s)
        s = EMAIL.sub("<email>", s)
        s = LONG.sub(self.mark("long"), s)
        s = LONG_B64.sub(self.mark("long"), s)
        for word in self.words:
            s = re.sub(r"(?<!\w)" + re.escape(word) + r"(?!\w)", "<name>", s, flags=re.I)
        return s

    def value(self, v):
        if isinstance(v, str):
            return self.text(v)
        if isinstance(v, list):
            return [self.value(x) for x in v]
        if isinstance(v, dict):
            out = {}
            for k, x in v.items():
                k2 = self.text(k)
                if WHOLE_KEYS.search(k) and isinstance(x, dict):
                    out[k2] = {self.text(name): "<redacted>" for name in x}
                elif SECRET_KEYS.search(k) and not isinstance(x, bool) and x not in (None, ""):
                    out[k2] = "<redacted>"
                else:
                    out[k2] = self.value(x)
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
        p = r.get("payload") or {}
        tool, cid = p.get("tool_name"), call_id(p)

        def same(x):
            q = x.get("payload") or {}
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
        (r.get("payload") or {}).get("last_assistant_message")
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
            {"run": r.get("run"), "source": (r.get("payload") or {}).get("source")}
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
    try:
        os.makedirs(work, mode=0o700)
        if shutil.which("git"):
            subprocess.call(
                ["git", "init", "-q", work], stdin=subprocess.DEVNULL
            )
        private_file(hook, HOOK)
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
        finally:
            remove(observe)

        if not read_log(log):
            say(
                "Devin didn't run the test hooks, so there is nothing to capture. "
                "Update Devin (`devin update`, or its installer) and try again; "
                "if it still happens, send the output of `devin --version`."
            )
            return 1

        started = [
            (r.get("payload") or {}).get("session_id")
            for r in read_log(log)
            if r.get("event") == "SessionStart"
        ]
        session = next((s for s in started if isinstance(s, str) and s), None)
        listed = output_of(devin, ["list", "--format", "json"], work)
        try:
            entries = json.loads(listed.get("stdout") or "null")
            if isinstance(entries, list):
                # Only this capture's own session, should the list hold others.
                listed["stdout"] = json.dumps(
                    [e for e in entries if session and session in json.dumps(e)]
                )
                listed["entries"] = len(entries)
        except ValueError:
            pass

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
        finally:
            remove(decide)

        say("Two questions about run 2:")
        prompted = ask_yes_no("Did Devin show its own approval prompt in run 2?")
        asked = ask_yes_no("Did Devin show you the colour question in run 2?")

        records = read_log(log)
        events, tools = shapes(records)
        result = {
            "capturedAt": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
            "platform": sys.platform,
            "hadOwnConfig": had_config,
            "userConfigChangedDuringRun": fingerprint(home) != before,
            "version": version,
            "authStatus": auth_status(status),
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
        scrub = Scrubber(home)
        result = {k: scrub.value(v) for k, v in result.items()}
        private_file(out, json.dumps(result, indent=2))
        if not session:
            say("Note: run 1 said no session id, so run 2 was a new session, not a resume.")
        say("Done. Send this one file (look it over first if you like):\n  " + out)
        return 0
    finally:
        for path in (hook, log, observe, decide, work):
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
    try:
        return capture(devin, home, folder)
    except KeyboardInterrupt:
        say("Stopped; nothing was kept.")
        return 130


if __name__ == "__main__":
    sys.exit(main())
