#!/usr/bin/env python3
"""Scan container images and source trees for known vulnerabilities (Trivy) and end-of-life software (xeol).

Writes a Markdown report and exits 1 when anything needs action: a CRITICAL vulnerability with a fix
available, or software already past its end-of-life date. HIGH findings and software reaching end of life
within three months are reported without failing.
"""

import argparse
import datetime
import json
import os
import subprocess
import sys

TRIVY = "aquasec/trivy:0.69.3"
XEOL = "noqcks/xeol:v0.10.8"
TRIVY_CACHE = os.path.expanduser("~/.cache/trivy")
DOCKER_SOCKET = ["-v", "/var/run/docker.sock:/var/run/docker.sock"]


def run_json(cmd):
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0 or not result.stdout.strip():
        raise RuntimeError(f"{' '.join(cmd[:6])}... failed: {result.stderr.strip()[-800:]}")
    return json.loads(result.stdout)


def trivy(target_args):
    os.makedirs(TRIVY_CACHE, exist_ok=True)
    report = run_json([
        "docker", "run", "--rm", *DOCKER_SOCKET, "-v", f"{TRIVY_CACHE}:/root/.cache/trivy", *target_args[0],
        TRIVY, *target_args[1], "--quiet", "--scanners", "vuln", "--severity", "CRITICAL,HIGH",
        "--format", "json", *target_args[2],
    ])
    findings = []
    for result in report.get("Results") or []:
        for vuln in result.get("Vulnerabilities") or []:
            findings.append({
                "id": vuln["VulnerabilityID"],
                "severity": vuln["Severity"],
                "package": vuln["PkgName"],
                "installed": vuln.get("InstalledVersion", ""),
                "fixed": vuln.get("FixedVersion") or "",
                "where": result.get("Target", ""),
            })
    unique = {(f["id"], f["package"], f["installed"]): f for f in findings}
    return sorted(unique.values(), key=lambda f: (f["severity"] != "CRITICAL", f["package"], f["id"]))


def xeol(image):
    report = run_json(["docker", "run", "--rm", *DOCKER_SOCKET, XEOL, "--platform", "linux/amd64", "-q", "-l", "3m", "-o", "json", image])
    today = datetime.date.today().isoformat()
    found = []
    for match in report.get("matches") or []:
        cycle, artifact = match["Cycle"], match["artifact"]
        found.append({
            "product": cycle["ProductName"],
            "version": artifact.get("version") or cycle["ReleaseCycle"],
            "eol": cycle["Eol"],
            "past": cycle["Eol"] <= today,
        })
    return found


def scan_image(image):
    vulns = trivy(([], ["image", "--platform", "linux/amd64"], [image]))
    return {"target": image, "vulns": vulns, "eol": xeol(image)}


def scan_tree(path):
    mount = os.path.abspath(path)
    vulns = trivy((["-v", f"{mount}:/src:ro"], ["fs", "--skip-dirs", "**/node_modules,**/target"], ["/src"]))
    return {"target": f"source: {path}", "vulns": vulns, "eol": []}


def needs_action(scan):
    critical = [v for v in scan["vulns"] if v["severity"] == "CRITICAL" and v["fixed"]]
    past_eol = [e for e in scan["eol"] if e["past"]]
    return critical, past_eol


def render(title, scans, errors):
    lines = [f"# {title}", "", f"Scanned {datetime.date.today().isoformat()}: {len(scans)} targets.", ""]
    flagged = 0
    for scan in scans:
        critical, past_eol = needs_action(scan)
        high = sum(1 for v in scan["vulns"] if v["severity"] == "HIGH")
        unfixed = sum(1 for v in scan["vulns"] if v["severity"] == "CRITICAL" and not v["fixed"])
        soon = [e for e in scan["eol"] if not e["past"]]
        if not (critical or past_eol or high or unfixed or soon):
            continue
        flagged += 1
        status = "ACTION NEEDED" if critical or past_eol else "review"
        lines += [f"## `{scan['target']}` ({status})", ""]
        for e in past_eol:
            lines.append(f"- **End of life**: {e['product']} {e['version']} reached end of life on {e['eol']}")
        for e in soon:
            lines.append(f"- End of life soon: {e['product']} {e['version']} on {e['eol']}")
        for v in critical:
            where = f" in `{v['where']}`" if scan["target"].startswith("source:") else ""
            lines.append(f"- **CRITICAL** {v['id']}: {v['package']} {v['installed']} (fixed in {v['fixed']}){where}")
        if unfixed:
            lines.append(f"- {unfixed} CRITICAL with no fix released yet")
        if high:
            lines.append(f"- {high} HIGH")
        lines.append("")
    for target, error in errors:
        lines += [f"## `{target}` (scan failed)", "", f"```\n{error}\n```", ""]
    if not flagged and not errors:
        lines.append("Nothing found.")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--title", default="Security scan")
    parser.add_argument("--image", action="append", default=[])
    parser.add_argument("--images-file")
    parser.add_argument("--tree", action="append", default=[])
    parser.add_argument("--report", required=True)
    args = parser.parse_args()

    images = list(args.image)
    if args.images_file:
        with open(args.images_file) as f:
            images += [line.strip() for line in f if line.strip()]

    scans, errors = [], []
    for target, scan in [(i, scan_image) for i in images] + [(t, scan_tree) for t in args.tree]:
        print(f"scanning {target}", file=sys.stderr)
        try:
            scans.append(scan(target))
        except (RuntimeError, json.JSONDecodeError) as e:
            errors.append((target, str(e)))

    with open(args.report, "w") as f:
        f.write(render(args.title, scans, errors))
    action = any(any(needs_action(s)) for s in scans)
    sys.exit(1 if action or errors else 0)


if __name__ == "__main__":
    main()
