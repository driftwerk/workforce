#!/usr/bin/env bash
# Verify the privilege-separated executor is actually live and actually
# separating. Run as root ON THE HOST after enabling privsep.
#
# WHY A SCRIPT AND NOT "THE DEPLOY WENT GREEN"
# --------------------------------------------
# Two properties make a bad privsep rollout hard to see. Dispatch is fail-closed,
# so a wrong socket path takes out `exec.run`, every `code.*`, `coding.session`,
# `coding.claude` and `wasm_skill.build` at once. And a Swarm service with
# `failure_action: rollback` will quietly revert a container that cannot start
# and still report a successful deploy. So check the running container, not the
# exit code of whatever put it there.
#
# Exits non-zero if any check fails, so it can gate a rollout.
#
# Usage: privsep-verify.sh [--container NAME] [--service NAME]
set -uo pipefail

CONTAINER=""
SERVICE="${PRIVSEP_SERVICE:-}"
pass=0; fail=0

while [ $# -gt 0 ]; do
    case "$1" in
        --container) CONTAINER="${2:-}"; shift 2 ;;
        --service)   SERVICE="${2:-}"; shift 2 ;;
        -h|--help)   sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "unknown argument '$1'" >&2; exit 2 ;;
    esac
done

# Resolve a running container: explicit name, else the newest task of the named
# service, else anything whose image looks like a workforce hub.
if [ -z "$CONTAINER" ]; then
    if [ -n "$SERVICE" ]; then
        CONTAINER=$(docker ps --filter "label=com.docker.swarm.service.name=${SERVICE}" \
                              --format '{{.ID}}' | head -1)
    fi
    [ -n "$CONTAINER" ] || CONTAINER=$(docker ps --format '{{.ID}} {{.Image}}' |
        grep -Ei 'workforce|hub' | awk '{print $1}' | head -1)
fi
[ -n "$CONTAINER" ] || { echo "privsep-verify: no running hub container found" >&2; exit 2; }
echo "privsep-verify: container ${CONTAINER}"
echo

ck() { # ck <name> <ok|deny> <cmd...>
    local n="$1" want="$2"; shift 2
    if "$@" >/dev/null 2>&1; then got=ok; else got=deny; fi
    if [ "$got" = "$want" ]; then printf '  PASS  %s\n' "$n"; pass=$((pass+1))
    else printf '  FAIL  %s (want=%s got=%s)\n' "$n" "$want" "$got"; fail=$((fail+1)); fi
}
# Run a command as the unprivileged agent, the way the runner does.
asagent() { docker exec -u 0 "$CONTAINER" setpriv --reuid agent --regid workforce \
    --init-groups --reset-env "$@"; }

echo "== process separation =="
docker exec "$CONTAINER" sh -c 'ps -eo pid,uid,args --no-headers 2>/dev/null | head -5' | sed 's/^/  /'
ck "pid 1 is the privsep init"    ok docker exec "$CONTAINER" sh -c 'ps -o args= -p 1 | grep -Eq "tini|workforce-init"'
ck "hub runs as uid 1000"         ok docker exec "$CONTAINER" sh -c 'ps -eo uid,args --no-headers | grep -v exec-runner | grep -qw 1000'
ck "exec-runner runs as uid 1001" ok docker exec "$CONTAINER" sh -c 'pgrep -u 1001 -f exec-runner >/dev/null'
ck "runner socket exists"         ok docker exec "$CONTAINER" test -S /run/workforce/exec.sock

echo
# The hub does not get --reset-env (it needs the vault key), so it inherits
# root's HOME unless the init sets it. At /root every $HOME-relative path the
# hub owns — the Claude Code session store, the Anthropic proxy dir — becomes
# uncreatable for uid 1000, and /cc fails on every turn.
#
# This cannot be checked by reading the hub's environ: Docker drops
# CAP_SYS_PTRACE, so even container-root gets EPERM on another process's
# /proc/<pid>/environ. Check the observable consequences instead — the hub's own
# complaint in the log, and whether the home the passwd entry names is writable.
hub_home=$(docker exec -u 0 "$CONTAINER" sh -c 'getent passwd workforce | cut -d: -f6' 2>/dev/null)
echo "== hub HOME =="
echo "  passwd home: ${hub_home:-<unknown>}"
ck "hub can write \$HOME/.claude" ok docker exec -u 1000 "$CONTAINER" \
    sh -c "mkdir -p '${hub_home}/.claude' && touch '${hub_home}/.claude/.privsep-probe' && rm -f '${hub_home}/.claude/.privsep-probe'"
ck "hub did not fall back to root's HOME" deny docker exec -u 0 "$CONTAINER" test -e /root/.claude
# Wrapped in sh -c: an unquoted pipe here would feed ck's OWN output into grep
# rather than the log, and the check would silently never run.
ck "no 'session store unwritable' in the log" deny \
    sh -c "docker logs '$CONTAINER' --tail 2000 2>&1 | grep -q 'session store unwritable'"
# workforce-init names the runner's user to the hub (WORKFORCE_RUNNER_USER), which
# is how the hub tells configuration the agent's commands planted from its own.
# Unnamed, it logs this once at boot, so the whole log is searched.
ck "hub knows whom the runner runs as" deny \
    sh -c "docker logs '$CONTAINER' 2>&1 | grep -q 'it was not told as whom'"

echo
echo "== secret isolation =="
ck "agent cannot read /proc/1/environ" deny asagent cat /proc/1/environ
# Enumerated by position, not by name: a repository is free to commit a file
# called `vault.enc`, and a name-matching sweep reports that checkout content as
# a leaked credential — failing this gate on every install that holds such a
# repo. These four positions are where the hub's own vaults live, and match the
# bound `workforce-init` sweeps at boot.
vaults=$(docker exec "$CONTAINER" sh -c 'for v in \
    /app/data/vault.enc /app/data/vault-key.txt \
    /app/data/data/vault.enc /app/data/data/vault-key.txt \
    /app/data/agents/*/vault.enc /app/data/agents/*/vault-key.txt \
    /app/data/agents/*/data/vault.enc /app/data/agents/*/data/vault-key.txt; do
    [ -f "$v" ] && echo "$v"
done' 2>/dev/null)
if [ -z "$vaults" ]; then
    echo "  SKIP  no vault files on this install"
else
    while IFS= read -r v; do
        [ -n "$v" ] || continue
        mode=$(docker exec "$CONTAINER" stat -c '%a' "$v" 2>/dev/null)
        ck "agent cannot read $(basename "$(dirname "$v")")/$(basename "$v") (mode $mode)" deny asagent cat "$v"
    done <<< "$vaults"
fi

echo
# Anything named like a vault at the same positions — an operator's backup copy
# (`vault.enc.frozen`, `vault-key.txt.bak`) is the same secret under a name the
# boot sweep does not match.
copies=$(docker exec "$CONTAINER" sh -c 'for v in \
    /app/data/vault.enc?* /app/data/vault-key.txt?* \
    /app/data/data/vault.enc?* /app/data/data/vault-key.txt?* \
    /app/data/agents/*/vault.enc?* /app/data/agents/*/vault-key.txt?* \
    /app/data/agents/*/data/vault.enc?* /app/data/agents/*/data/vault-key.txt?*; do
    [ -f "$v" ] && echo "$v"
done' 2>/dev/null)
while IFS= read -r v; do
    [ -n "$v" ] || continue
    ck "agent cannot read vault copy ${v#/app/data/}" deny asagent sh -c ': < "$1"' _ "$v"
done <<< "$copies"

echo
# What the hub keeps for itself: transcripts, traces (every prompt and raw tool
# output), per-user notes, staged artifacts, Claude Code briefs and the SQLite
# stores. One agent uid runs every member's commands, so any of these it can
# open is readable by every user of every agent. The positions are the ones the
# hub closes at boot (crates/hub/src/private_state.rs); a store with no file
# yet is checked at its directory alone.
#
# Each check OPENS a file (`: < file`), because a listing proves nothing about
# the files in it. The controls run first, on a probe the hub uid makes in
# /tmp: the same open must succeed while the probe is readable and fail once it
# is the hub's alone, both for the file and for a directory around it. If
# `asagent` were broken every deny below would pass without checking anything;
# if it ran with more than the agent's reach, the closed probe would show it.
echo "== hub-private state is closed to the agent uid =="
probe=$(docker exec -u 1000 "$CONTAINER" sh -c 'd=$(mktemp -d /tmp/privsep-probe.XXXXXX) &&
    echo x > "$d/f" && chmod 0755 "$d" && chmod 0644 "$d/f" && echo "$d"' 2>/dev/null)
if [ -z "$probe" ]; then
    ck "control: hub uid can make a probe in /tmp" ok false
else
    ck "control: agent can open a hub file it may read" ok asagent sh -c ': < "$1"' _ "$probe/f"
    docker exec -u 1000 "$CONTAINER" chmod 0600 "$probe/f"
    ck "control: agent cannot open a hub file closed to it" deny asagent sh -c ': < "$1"' _ "$probe/f"
    docker exec -u 1000 "$CONTAINER" sh -c 'chmod 0644 "$1/f" && chmod 0700 "$1"' _ "$probe"
    ck "control: agent cannot open a readable file in a closed directory" deny \
        asagent sh -c ': < "$1"' _ "$probe/f"
    docker exec -u 1000 "$CONTAINER" rm -rf "$probe"
fi
# Agent workspaces: each agent's directory, and a `workspace` directory inside
# it for an agent configured to use one. Either counts only once the hub has
# closed it (its `memory/users` is the hub's), so a repository or a leftover
# directory of that name the hub never loaded is not checked as one.
workspaces=$(docker exec "$CONTAINER" sh -c 'for d in /app/data/agents/*/ /app/data/agents/*/workspace/; do
    [ -d "${d}memory/users" ] && [ "$(stat -c %u "${d}memory/users")" = 1000 ] && echo "${d%/}"
done' 2>/dev/null)
echo "  [workspaces] ${workspaces:-none the hub has closed under /app/data/agents}" | tr '\n' ' '; echo
# shellcheck disable=SC2086 # agent ids never contain spaces
private_dirs=$(docker exec "$CONTAINER" sh -c 'for d in /app/data/traces /app/data/artifacts \
    /app/data/cc-briefs /app/data/archived_sessions /app/data/memory-backups /app/data/scheduler \
    /app/data/roles /app/data/workflows /app/data/playbooks /app/data/monitors \
    /app/data/wasm-skills; do
    [ -d "$d" ] && echo "$d"
done
for ws in "$@"; do
    for name in sessions memory/users users artifacts-v2 scheduler heartbeats learning .local; do
        [ -d "$ws/$name" ] && echo "$ws/$name"
    done
done' _ $workspaces 2>/dev/null)
if [ -z "$private_dirs" ]; then
    echo "  SKIP  no hub-private directories on this install"
else
    while IFS= read -r d; do
        [ -n "$d" ] || continue
        name=${d#/app/data/}
        mode=$(docker exec "$CONTAINER" stat -L -c '%a %U' "$d" 2>/dev/null)
        # A link at a private position is either an operator's relocation or
        # something planted; name both owners so the report says which.
        if docker exec "$CONTAINER" test -L "$d" 2>/dev/null; then
            link=$(docker exec "$CONTAINER" sh -c 'echo "link by $(stat -c %U "$1") to $(readlink -f "$1")"' _ "$d" 2>/dev/null)
            mode="$mode, $link"
        fi
        ck "agent cannot list $name ($mode)" deny asagent ls "$d"
        # The newest file, so a trace or transcript written since the last boot
        # proves the mode it was created with, not only the boot-time repair.
        f=$(docker exec "$CONTAINER" sh -c 'find -H "$1" -maxdepth 3 -type f -printf "%T@ %p\n" \
            2>/dev/null | sort -nr | head -1 | cut -d" " -f2-' _ "$d")
        [ -n "$f" ] || continue
        fmode=$(docker exec "$CONTAINER" stat -c '%a' "$f" 2>/dev/null)
        ck "agent cannot open newest file in $name (mode $fmode)" deny asagent sh -c ': < "$1"' _ "$f"
        ck "hub can open newest file in $name" ok docker exec -u 1000 "$CONTAINER" sh -c ': < "$1"' _ "$f"
    done <<< "$private_dirs"
fi
dbs=$(docker exec "$CONTAINER" sh -c 'for f in /app/data/*.db /app/data/*.db-wal /app/data/*.db-shm \
    /app/data/*.db-journal; do [ -f "$f" ] && [ ! -L "$f" ] && echo "$f"; done' 2>/dev/null)
while IFS= read -r f; do
    [ -n "$f" ] || continue
    fmode=$(docker exec "$CONTAINER" stat -c '%a' "$f" 2>/dev/null)
    ck "agent cannot open ${f#/app/data/} (mode $fmode)" deny asagent sh -c ': < "$1"' _ "$f"
done <<< "$dbs"

echo
# Configuration the hub reads (an agent's config.toml and identity files, the
# config overlay) must be unchangeable by the agent uid: not writable, and not
# replaceable by renaming or removing it, which needs only write access to the
# directory and is what the sticky bit stops. Rename and remove are probed on a
# file the hub makes for the purpose, never on real configuration; the controls
# show the agent CAN do both to an entry of its own, so a deny is not vacuous.
# The files are the ones crates/common/src/config_files.rs names, in every
# workspace found above. One the agent uid made itself, where none existed, is
# never read by the hub, so it is reported rather than failed.
echo "== configuration is not agent-changeable =="
# shellcheck disable=SC2086
for root in /app/data /app/data/agents $workspaces; do
    name=${root#/app/}
    own="$root/.privsep-own.$$"
    hub="$root/.privsep-hub.$$"
    if ! asagent sh -c 'touch "$1"' _ "$own" 2>/dev/null; then
        ck "control: agent can create an entry in $name" ok false
        continue
    fi
    ck "control: agent can rename its own entry in $name" ok asagent mv "$own" "$own.moved"
    asagent rm -f "$own.moved" 2>/dev/null
    docker exec -u 1000 "$CONTAINER" sh -c 'echo x > "$1" && chmod 0644 "$1"' _ "$hub"
    ck "agent cannot rename a hub entry in $name" deny asagent mv "$hub" "$hub.moved"
    ck "agent cannot remove a hub entry in $name" deny asagent rm -f "$hub"
    docker exec -u 1000 "$CONTAINER" rm -f "$hub" "$hub.moved" 2>/dev/null
done
# shellcheck disable=SC2086
configs=$(docker exec "$CONTAINER" sh -c '[ -f /app/data/workforce.overlay.toml ] &&
    echo /app/data/workforce.overlay.toml
for ws in "$@"; do
    for name in config.toml SOUL.md AGENTS.md IDENTITY.md TOOLS.md HEARTBEAT.md WELCOME.md \
            vision_tiers.toml transcription_tiers.toml pricing.toml; do
        [ -f "$ws/$name" ] && echo "$ws/$name"
    done
done' _ $workspaces 2>/dev/null)
if [ -z "$configs" ]; then
    echo "  SKIP  no configuration files on this install"
else
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        fmode=$(docker exec "$CONTAINER" stat -c '%a %U' "$f" 2>/dev/null)
        if [ "${fmode#* }" = agent ]; then
            echo "  INFO  ${f#/app/data/} is the agent uid's own ($fmode), so the hub ignores it"
        else
            ck "agent cannot write ${f#/app/data/} ($fmode)" deny asagent test -w "$f"
        fi
    done <<< "$configs"
fi

echo
# Installed skills run their own scripts as the agent, so the tree stays
# readable to it, but the hub takes group write off everything in it after
# every install: a skill the agent could edit would run with whatever it wrote.
echo "== skills are readable but not agent-writable =="
if docker exec "$CONTAINER" test -d /app/data/skills 2>/dev/null; then
    ck "agent cannot create in skills" deny \
        asagent sh -c 't=/app/data/skills/.privsep-probe.$$; touch "$t" && rm -f "$t"'
    f=$(docker exec "$CONTAINER" sh -c 'find /app/data/skills -type f -printf "%T@ %p\n" \
        2>/dev/null | sort -nr | head -1 | cut -d" " -f2-')
    if [ -n "$f" ]; then
        fmode=$(docker exec "$CONTAINER" stat -c '%a' "$f" 2>/dev/null)
        ck "agent can read newest skill file (mode $fmode)" ok asagent sh -c ': < "$1"' _ "$f"
        ck "agent cannot write newest skill file (mode $fmode)" deny asagent test -w "$f"
    fi
else
    echo "  SKIP  no skills directory on this install"
fi

echo
echo "== shared trees are agent-writable =="
for d in /app/data/worktrees /app/data/worktree-bases /app/workspace /app/.claude; do
    if docker exec "$CONTAINER" test -d "$d" 2>/dev/null; then
        ck "agent can write $d" ok asagent sh -c "t=$d/.privsep-probe.\$\$; touch \$t && rm -f \$t"
    else
        echo "  SKIP  $d (absent)"
    fi
done

echo
# The trees a coding session actually works in. These are created by the hub and
# predate privsep on any upgraded install, where they carry no group write and
# the agent cannot edit a file — a session started after the rollout succeeds
# while resuming an older one silently cannot. Probing an EXISTING tree is the
# point; a freshly created one would pass on a broken install.
echo "== project coding-session trees are agent-writable =="
trees=$(docker exec "$CONTAINER" sh -c 'ls -d /app/data/agents/*/projects/*/work/session/*/ \
    /app/data/agents/*/projects/*/cc-sessions/*/ 2>/dev/null | head -4')
if [ -z "$trees" ]; then
    echo "  SKIP  no project sessions on this install"
else
    while IFS= read -r t; do
        [ -n "$t" ] || continue
        mode=$(docker exec "$CONTAINER" stat -c '%a' "$t" 2>/dev/null)
        ck "agent can write ${t#/app/data/agents/} (mode $mode)" ok \
            asagent sh -c "p=$t.privsep-probe.\$\$; touch \$p && rm -f \$p"
    done <<< "$trees"
fi

echo
# Where `wasm_skill.build` compiles. Same shape as the session trees above and
# the same failure: a project scaffolded before the rollout is hub-owned, so the
# dispatched cargo cannot take its build lock, while a project scaffolded after
# it builds fine. Probing an EXISTING project is the point — a fresh one passes
# on a broken install.
echo "== WASM skill projects are agent-writable =="
skills=$(docker exec "$CONTAINER" sh -c 'ls -d /app/data/agents/*/wasm-skills/*/ \
    /app/data/agents/*/workspace/wasm-skills/*/ 2>/dev/null | head -4')
if [ -z "$skills" ]; then
    echo "  SKIP  no skill projects on this install"
else
    while IFS= read -r s; do
        [ -n "$s" ] || continue
        mode=$(docker exec "$CONTAINER" stat -c '%a' "$s" 2>/dev/null)
        ck "agent can write ${s#/app/data/agents/} (mode $mode)" ok \
            asagent sh -c "p=$s.privsep-probe.\$\$; touch \$p && rm -f \$p"
    done <<< "$skills"
fi

echo
echo "== git configs are not agent-writable =="
# Every base a hub-side git runs in, not just `worktree-bases`: the projects
# subsystem keeps its own under each agent, and a planted config there is read
# by the hub on the next `ensure`.
cfgs=$(docker exec "$CONTAINER" sh -c 'find /app/data/worktree-bases \
    /app/data/agents/*/projects/*/work/bases -maxdepth 3 -type f -name config 2>/dev/null | head -5')
if [ -z "$cfgs" ]; then
    echo "  SKIP  no git bases on this install"
else
    while IFS= read -r c; do
        [ -n "$c" ] || continue
        ck "agent cannot write $(basename "$(dirname "$c")")/config" deny asagent sh -c "echo x >> $c"
    done <<< "$cfgs"
fi

echo
echo "== Claude Code =="
# The suite once passed 26/26 while Claude Code ran as the hub with the vault in
# reach, because nothing in it was about Claude Code. Assert the posture matches
# what the configuration asks for, so neither an unset flag nor an absent
# process can make this pass without checking anything.
if docker exec -u 0 "$CONTAINER" grep -qE '^[[:space:]]*sandbox_claude_code[[:space:]]*=[[:space:]]*true' /app/workforce.toml 2>/dev/null; then
    want_sandboxed=1
else
    want_sandboxed=0
fi
echo "  [exec] sandbox_claude_code: $([ "$want_sandboxed" = 1 ] && echo on || echo off)"

cc_pid=$(docker exec -u 0 "$CONTAINER" pgrep -f 'claude --print|claude -p|/bin/claude' 2>/dev/null | head -1)
if [ -z "$cc_pid" ]; then
    if [ "$want_sandboxed" = 1 ]; then
        echo "  SKIP  no Claude Code turn in flight — re-run during one to confirm"
    else
        echo "  SKIP  sandboxing is off; Claude Code runs as the hub by design"
    fi
else
    cc_uid=$(docker exec -u 0 "$CONTAINER" ps -o uid= -p "$cc_pid" 2>/dev/null | tr -d ' ')
    echo "  Claude Code pid $cc_pid runs as uid ${cc_uid:-<unreadable>}"
    if [ -z "$cc_uid" ]; then
        echo "  FAIL  could not read Claude Code's uid"
        fail=$((fail+1))
    elif [ "$want_sandboxed" = 1 ]; then
        ck "Claude Code is not the hub uid" ok sh -c "[ '$cc_uid' != '1000' ]"
        ck "Claude Code cannot read the vault" deny \
            docker exec -u "$cc_uid" "$CONTAINER" cat /app/data/vault.enc
        ck "Claude Code cannot read the vault key file" deny \
            docker exec -u "$cc_uid" "$CONTAINER" cat /app/data/vault-key.txt
        # Its own uid can read its environment where root cannot (no
        # CAP_SYS_PTRACE). The control proves the read works, so the deny below
        # is about the variable and not about an unreadable file.
        ck "control: Claude Code's environment is readable as its uid" ok \
            docker exec -u "$cc_uid" "$CONTAINER" sh -c ": < /proc/$cc_pid/environ"
        ck "Claude Code has no WORKFORCE_VAULT_KEY" deny \
            docker exec -u "$cc_uid" "$CONTAINER" sh -c \
                "tr '\\0' '\\n' < /proc/$cc_pid/environ | grep -q '^WORKFORCE_VAULT_KEY='"
    else
        echo "  INFO  sandboxing is off, so uid $cc_uid is expected"
    fi
fi

echo
echo "== dispatch round-trip (as the hub uid, through the socket) =="
# The request is the frame the hub's client sends for `exec.run`: a
# `ClientFrame::Request` carrying `Request::Exec`, both tagged enums
# (crates/agent-runtime/src/tools/exec_runner/protocol.rs). A protocol test there
# reads REQUEST out of this file, so the two cannot drift apart. The socket and
# the expected uid are arguments so the check can be run against any runner.
docker exec -i -u 1000 "$CONTAINER" python3 - /run/workforce/exec.sock 1001 <<'PY' 2>&1 | sed 's/^/  /'
import json, socket, struct, sys

REQUEST = '{"frame":"request","kind":"exec","program":"id","args":["-u"],"workdir":null,"timeout_secs":30,"allow_retry":false,"env":[],"stdin":null,"scope":null,"mcp":null}'
path, want_uid = sys.argv[1], sys.argv[2]

def fail(why):
    print(f"FAIL  {why}"); sys.exit(1)

def read_exact(s, n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c: return None
        b += c
    return b

def frames(s):
    while True:
        h = read_exact(s, 4)
        if h is None: return
        b = read_exact(s, struct.unpack("<I", h)[0])
        if b is None: return
        yield json.loads(b)

try:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    # The runner heartbeats every 10s while a job runs; silence past this is a hang.
    s.settimeout(45)
    s.connect(path)
    body = REQUEST.encode()
    s.sendall(struct.pack("<I", len(body)) + body)
except OSError as e:
    fail(f"cannot reach runner socket {path}: {e}")
version, out, end = None, "", None
try:
    for ev in frames(s):
        kind = ev.get("kind")
        if kind == "ready": version = ev.get("protocol_version")
        elif kind == "stdout": out += ev.get("chunk", "")
        elif kind in ("done", "refused", "failed"):
            end = ev; break
except socket.timeout:
    fail("the runner stopped answering mid-request")
except (OSError, ValueError) as e:
    fail(f"reading the runner's answer: {e}")
if end is None:
    fail("the runner closed the connection before finishing" +
         ("" if version else " without announcing a protocol: it refused this peer"
                            " or could not read the request"))
if end["kind"] != "done":
    fail(f"the runner {end['kind']} the request: {end.get('reason') or end.get('message')}")
uid = out.strip()
if uid == want_uid and end.get("exit_code") == 0:
    print(f"PASS  runner executed as uid {want_uid} (protocol v{version})")
else:
    fail(f"expected uid {want_uid} exit 0, got uid={uid!r} done={end}")
PY
rt=${PIPESTATUS[0]}
[ "$rt" = 0 ] && pass=$((pass+1)) || fail=$((fail+1))

echo
echo "==================== $pass passed, $fail failed ===================="
[ "$fail" = 0 ] || exit 1
