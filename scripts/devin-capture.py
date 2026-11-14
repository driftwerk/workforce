#!/usr/bin/env python3
"""Record what a signed-in Devin CLI sends its hooks, for Desk's Devin engine.

Run it on a computer where `devin` is installed and signed in:

    python3 <(curl -fsSL https://raw.githubusercontent.com/driftwerk/workforce/main/scripts/devin-capture.py)

It starts Devin twice, in a new scratch folder under your home folder:

1. Devin asks you as usual. Trust the folder if it asks, approve what it
   asks, pick any colour, and exit Devin when it says DONE (/exit).
2. It resumes the same session. This time the test hook answers for Devin:
   don't press anything until it says DONE, then exit as before. If Devin
   asks you anything anyway, approve it, and pick Red.

The hooks are added the way Desk will add them: your own Devin config is
copied whole into a private file passed with `--config`, and the hooks are
appended to yours. Nothing in your own config or credentials is changed or
read beyond that copy, which is deleted at the end.

What it writes is one file, `devin-capture.json`, with every hook payload,
`devin --version`, `devin auth status` and `devin list --format json`. Before
it is written, home paths, your user and host name, emails, UUIDs, tokens and
any value under a key that names a secret are replaced. Nothing is sent
anywhere: you send the file yourself.

`devin` is looked up at ~/.local/bin/devin (where its installer puts it), or
at the path in DEVIN_CAPTURE_BIN. It is never looked up on PATH.

Plan mode isn't covered: Devin can't be started in it.
"""

import getpass
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

# The events Devin documents for its hooks (docs.devin.ai, lifecycle hooks
# and the stable changelog). An event it doesn't know could make it refuse
# the config, so only these.
EVENTS = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "Stop",
    "SessionEnd",
    "PreCompact",
    "PostCompact",
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
    "3) Run the shell command `ls -la`. "
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


def strip_jsonc(text):
    """JSON with `//` and `/* */` comments and trailing commas, as plain JSON."""
    out, i, n = [], 0, len(text)
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
        else:
            out.append(c)
            i += 1
    return re.sub(r",(\s*[}\]])", r"\1", "".join(out))


def person_config(home):
    """The person's own Devin config, parsed; empty when there is none."""
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.join(home, ".config")
    path = os.path.join(base, "devin", "config.json")
    if sys.platform == "win32" and os.environ.get("APPDATA"):
        path = os.path.join(os.environ["APPDATA"], "devin", "config.json")
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


def config_for(run, mine, hook, log):
    """The person's config with the logging hooks appended to theirs."""
    config = json.loads(json.dumps(mine))
    hooks = config.get("hooks")
    if not isinstance(hooks, dict):
        hooks = {}
    for event in EVENTS:
        command = " ".join(
            shell_quote(a) for a in [sys.executable, hook, run, event, log]
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
            [devin] + args, cwd=cwd, capture_output=True, text=True, timeout=60
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


SECRET_KEYS = re.compile(
    r"(token|secret|password|passwd|api[_-]?key|apikey|authorization|cookie|credential|session_key|private)",
    re.I,
)
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
UUID = re.compile(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
JWT = re.compile(r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+")
BEARER = re.compile(r"(?i)\b(bearer|token|basic)\s+[A-Za-z0-9._~+/=-]{8,}")
# Long runs with a digit in them: keys, tokens, hashes. Path segments are
# split by `/`, so a path is never one.
LONG = re.compile(r"(?=[A-Za-z0-9_+=-]*[0-9])[A-Za-z0-9_+=-]{32,}")
ORG = re.compile(r"(?im)^(\s*[^:\n]*\b(organi[sz]ation|org|team|workspace|account)\b[^:\n]*:\s*)(.+)$")


class Scrubber:
    """Replaces what could say who the person is, or let anyone in."""

    def __init__(self, home):
        self.home = home
        self.user = getpass.getuser()
        self.host = socket.gethostname().split(".")[0]
        self.seen = {}

    def mark(self, kind):
        # Each one numbered, so the same id reads the same everywhere.
        def replace(m):
            key = (kind, m.group(0).lower())
            if key not in self.seen:
                self.seen[key] = "<%s-%d>" % (kind, len(self.seen) + 1)
            return self.seen[key]

        return replace

    def text(self, s, org=False):
        if self.home and len(self.home) > 1:
            s = s.replace(self.home, "~")
        s = UUID.sub(self.mark("uuid"), s)
        s = JWT.sub("<token>", s)
        s = BEARER.sub(lambda m: m.group(1) + " <token>", s)
        s = EMAIL.sub("<email>", s)
        s = LONG.sub(self.mark("long"), s)
        if org:
            s = ORG.sub(lambda m: m.group(1) + "<org>", s)
        for word, mark in ((self.user, "<user>"), (self.host, "<host>")):
            if word and len(word) > 2:
                s = re.sub(r"\b" + re.escape(word) + r"\b", mark, s)
        return s

    def value(self, v, org=False):
        if isinstance(v, str):
            return self.text(v, org)
        if isinstance(v, list):
            return [self.value(x, org) for x in v]
        if isinstance(v, dict):
            out = {}
            for k, x in v.items():
                k2 = self.text(k)
                if SECRET_KEYS.search(k) and not isinstance(x, (dict, list, bool)) and x not in (None, ""):
                    out[k2] = "<redacted>"
                else:
                    out[k2] = self.value(x, org)
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


def findings(records, prompted_again, asked_again):
    """What the capture was for, read off the records."""
    ids = set()
    for r in records:
        p = r.get("payload")
        if isinstance(p, dict):
            ids.update(k for k in p if re.search(r"(^|_)(tool_use|tool_call|call|use)_?id$", k))
    decide = [r for r in records if r.get("run") == "decide"]
    waits = []
    for i, r in enumerate(decide):
        if r.get("event") != "PermissionRequest":
            continue
        tool = (r.get("payload") or {}).get("tool_name")
        after = next(
            (
                x
                for x in decide[i + 1 :]
                if x.get("event") == "PostToolUse"
                and (x.get("payload") or {}).get("tool_name") == tool
            ),
            None,
        )
        waits.append(
            {
                "tool": tool,
                "heldSecs": round(r["end"] - r["start"], 2),
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
        "resumedSessionStartSources": [
            (r.get("payload") or {}).get("source")
            for r in records
            if r.get("event") == "SessionStart"
        ],
    }


def main():
    home = os.path.expanduser("~")
    devin = os.environ.get("DEVIN_CAPTURE_BIN") or os.path.join(home, ".local", "bin", "devin")
    if not (os.path.isfile(devin) and os.access(devin, os.X_OK)):
        say("No Devin at %s. Install it, or set DEVIN_CAPTURE_BIN to its path." % devin)
        return 1
    stamp = time.strftime("%Y%m%d-%H%M%S")
    folder = os.path.join(home, "devin-capture-" + stamp)
    work = os.path.join(folder, "work")
    os.makedirs(work, mode=0o700)
    os.chmod(folder, stat.S_IRWXU)
    if shutil.which("git"):
        subprocess.call(["git", "init", "-q", work])
    hook = os.path.join(folder, "hook.py")
    log = os.path.join(folder, "hooks.jsonl")
    private_file(hook, HOOK)
    mine, had_config = person_config(home)

    version = output_of(devin, ["--version"], work)
    status = output_of(devin, ["auth", "status"], work)

    say("Run 1 of 2: trust the folder if Devin asks, approve what it asks and pick any "
        "colour. Exit Devin (/exit) when it says DONE.")
    observe = os.path.join(folder, "observe.json")
    private_file(observe, json.dumps(config_for("observe", mine, hook, log)))
    first_exit = run_devin(
        devin, ["--config", observe, "--permission-mode", "normal", "--", FIRST_PROMPT], work
    )

    started = [
        (r.get("payload") or {}).get("session_id")
        for r in read_log(log)
        if r.get("event") == "SessionStart"
    ]
    session = next((s for s in started if isinstance(s, str) and s), None)
    listed = output_of(devin, ["list", "--format", "json"], work)

    say(
        "Run 2 of 2: the test hook answers for Devin now. Don't press anything until it "
        "says DONE, then exit Devin. If it asks you anything anyway, approve it and pick %s."
        % PERSON_ANSWER
    )
    decide = os.path.join(folder, "decide.json")
    private_file(decide, json.dumps(config_for("decide", mine, hook, log)))
    resume = ["-r", session] if session else []
    second_exit = run_devin(
        devin,
        ["--config", decide, "--permission-mode", "normal"] + resume + ["--", SECOND_PROMPT],
        work,
    )

    say("Two questions about run 2:")
    prompted = ask_yes_no("Did Devin show its own approval prompt in run 2?")
    asked = ask_yes_no("Did Devin show you the colour question in run 2?")

    records = read_log(log)
    events, tools = shapes(records)
    scrub = Scrubber(home)
    capture = {
        "capturedAt": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "platform": sys.platform,
        "hadOwnConfig": had_config,
        "version": scrub.value(version),
        "authStatus": scrub.value(status, org=True),
        "list": scrub.value(listed),
        "resumedById": session is not None,
        "exits": [first_exit, second_exit],
        "findings": findings(records, prompted, asked),
        "eventFields": events,
        "toolInputFields": tools,
        "records": scrub.value(records),
    }
    out = os.path.join(folder, "devin-capture.json")
    private_file(out, json.dumps(capture, indent=2))
    for path in (hook, log, observe, decide):
        try:
            os.remove(path)
        except FileNotFoundError:
            pass
    shutil.rmtree(work, ignore_errors=True)
    say("Done. Send this one file (look it over first if you like):\n  " + out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
