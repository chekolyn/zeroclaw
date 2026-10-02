#!/usr/bin/env python3
"""The fork patch-lineage checker — the mechanism's core.

Machine-verifies that every patch in the fork's lineage is accounted for by
the structured manifest (PATCHES.md) and the machine-parseable audit map:

  check_presence      every applied row's current_sha is a commit in
                      <base>..HEAD (the sha anchor; per-commit patch-ids are
                      computed ONLY per single commits — a cumulative
                      base..HEAD diff is never used anywhere).
  check_reverse       every commit in
                      `git rev-list --no-merges <base>..HEAD -- <reverse-scope>`
                      that touches anything OUTSIDE the mechanism-file set
                      {PATCHES.md, scripts/check_patches.py, Makefile} maps to
                      an applied row (by resolved sha) or the allowlist.
                      Mechanism-surface commits are exempt even when they
                      touch a scoped path (PATCHES.md is both).
  check_drift         every applied row's touched paths (from per-commit
                      history, never the HEAD tree) lie within the declared
                      reverse-scope — the coverage can never silently narrow.
  check_map_consistency (FAIL_A)
                      rule 1: the allowlist ∩ the row sha-set = ∅
                      rule 2: the audit map's class-c set = the allowlist set
                              (a bijection, both directions)
                      rule 3: no allowlisted commit's patch-id appears in a
                              cached prior-tag patch-id set — the asserted
                              tags PLUS the lineage's base tag (the
                              prior-lineage family) — an upstream-era replay
                              misfiled as a fixup is a failure
                      a/b↔rows: every class-a/b map sha has >=1 applied row
                              and every row's sha is in the map's a∪b set
                              (sha-level — one map row may cover several rows
                              and vice versa, e.g. R2-R4 vs R2/R3/R4).
  check_upstreamed    per row: TBD-RESOLVE = FAIL_U (exit 1, never exit 2);
                      otherwise proven iff the origin commit's per-commit
                      patch-id is in the asserted tag's cached patch-id set
                      OR the row's probe passes against the tag's tree
                      (`git show <tag>:<glob>`). Ancestry alone is NEVER
                      proof (a merge commit can be an ancestor while its
                      patch-id is absent from the tag's set).
  run_probes          the pinned probe registry against the worktree; every
                      probe name referenced by any row must exist in the
                      registry.

THE REPORT (every run, green included — the trust boundary's only surface):
status lines per check + the FULL allowlist enumeration + the audit map's
class-c set. The honest boundary: a native patch consistently misfiled as
class-c (both map and allowlist) passes the machine and is surfaced ONLY by
this per-run enumeration — no drop can pass the machine WITHOUT appearing on
every build's report.

Exit codes: 0 green; 1 any check failure (FAIL_M/FAIL_P/FAIL_R/FAIL_A/
FAIL_U/FAIL_D/FAIL_B); 2 invocation/environment error (bad repo, unreadable
manifest, broken git). The checker NEVER edits anything — read-only git
plumbing only. Zero dependencies beyond git + the python3 stdlib.

The probe registry is FULLY PINNED with verified real paths (both re-verified
with ls before the first run):
  - "allow_scripts-config-read" — crates/zeroclaw-runtime/src/tools/delegate.rs
    (the REAL runtime-crate path; both scan sites, min_count 2)
  - "mqtt-poll-alive" — crates/zeroclaw-channels/src/orchestrator/mqtt.rs
    (the REAL channels-crate path; record_poll_alive lives only there)
"""

from __future__ import annotations

import argparse
import fnmatch
import os
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass, field

# --------------------------------------------------------------------------
# Pinned constants
# --------------------------------------------------------------------------

PROBE_REGISTRY: dict = {
    "allow_scripts-config-read": {
        "glob": "crates/zeroclaw-runtime/src/tools/delegate.rs",
        "pattern": r"root_config.*allow_scripts",
        "min_count": 2,
    },
    "mqtt-poll-alive": {
        "glob": "crates/zeroclaw-channels/src/orchestrator/mqtt.rs",
        "pattern": r"record_poll_alive",
        "min_count": 1,
    },
}

# The mechanism's own surfaces — a commit touching ONLY these files is exempt
# from the reverse check even when one of them (PATCHES.md) is a scoped path.
MECHANISM_FILES = {"PATCHES.md", "scripts/check_patches.py", "Makefile"}

TBD_RESOLVE = "TBD-RESOLVE"

APPLIED_HEADER = ["id", "current_sha", "origin_base", "description", "probe"]
UPSTREAMED_HEADER = ["id", "origin_commit", "asserted_in", "upstream_sha", "probe", "description"]
MAP_HEADER = ["sha", "class", "row_id_or_allowlist", "description"]


class EnvError(Exception):
    """Invocation/environment problem -> exit 2."""


# --------------------------------------------------------------------------
# Data model
# --------------------------------------------------------------------------

@dataclass
class AppliedRow:
    rid: str
    sha: str
    origin_base: str
    description: str
    probe: str

@dataclass
class UpstreamedRow:
    rid: str
    origin: str
    asserted_in: str
    upstream_sha: str
    probe: str
    description: str

@dataclass
class AllowEntry:
    sha: str
    reason: str

@dataclass
class MapRow:
    sha: str
    cls: str
    rid: str
    description: str

@dataclass
class Manifest:
    branch: str | None = None
    base: str | None = None
    scope: list = field(default_factory=list)
    applied: list = field(default_factory=list)
    upstreamed: list = field(default_factory=list)
    allowlist: list = field(default_factory=list)
    map_rows: list = field(default_factory=list)
    problems: list = field(default_factory=list)


# --------------------------------------------------------------------------
# Manifest parsing (by table-header shape — robust to section titles)
# --------------------------------------------------------------------------

_LINEAGE_RE = re.compile(r"^lineage:\s*(\S+)\s+base\s+(\S+)\s*$", re.M)
_SCOPE_RE = re.compile(r"^reverse-scope:\s*(.+?)\s*$", re.M)
_ALLOW_RE = re.compile(r"^allowlist:\s*([0-9a-fA-F]{4,40})\s+—\s*(.+?)\s*$", re.M)
_VALID_CLASSES = {"a", "b", "c"}


def _cells(line: str):
    s = line.strip()
    if not (s.startswith("|") and s.endswith("|")):
        return None
    return [c.strip() for c in s[1:-1].split("|")]


def _is_sep(cells) -> bool:
    return bool(cells) and all(re.fullmatch(r":?-{2,}:?", c) for c in cells)


def _scan_tables(text: str):
    """Yield (lineno, header_cells, [(row_lineno, row_cells), ...]) for the
    three known table shapes, identified by their header row (never by the
    surrounding section title)."""
    lines = text.splitlines()
    out = []
    i = 0
    while i < len(lines):
        c = _cells(lines[i])
        if c is not None and c in (APPLIED_HEADER, UPSTREAMED_HEADER, MAP_HEADER):
            j = i + 1
            rows = []
            while j < len(lines):
                r = _cells(lines[j])
                if r is None:
                    break
                if not _is_sep(r):
                    rows.append((j + 1, r))
                j += 1
            out.append((i + 1, c, rows))
            i = j
        else:
            i += 1
    return out


def parse_manifest(text: str) -> Manifest:
    m = Manifest()
    mlines = text.splitlines()

    lm = _LINEAGE_RE.findall(text)
    if len(lm) == 1:
        m.branch, m.base = lm[0]
    else:
        m.problems.append(
            f"lineage metadata: expected exactly one 'lineage: <branch> base <tag>' "
            f"line, found {len(lm)}")

    sm = _SCOPE_RE.findall(text)
    if len(sm) == 1:
        m.scope = [p.strip() for p in sm[0].split(",") if p.strip()]
        if not m.scope:
            m.problems.append("reverse-scope metadata: the line declares no paths")
    else:
        m.problems.append(
            f"reverse-scope metadata: expected exactly one 'reverse-scope: <paths>' "
            f"line, found {len(sm)}")

    seen = {"applied": 0, "upstreamed": 0, "map": 0}
    for lineno, header, rows in _scan_tables(text):
        if header == APPLIED_HEADER:
            seen["applied"] += 1
            for ln, cells in rows:
                if len(cells) != len(header):
                    m.problems.append(
                        f"applied-rows table: line {ln} has {len(cells)} cells, "
                        f"expected {len(header)}: {mlines[ln - 1].strip()[:100]}")
                    continue
                m.applied.append(AppliedRow(*cells))
        elif header == UPSTREAMED_HEADER:
            seen["upstreamed"] += 1
            for ln, cells in rows:
                if len(cells) != len(header):
                    m.problems.append(
                        f"upstreamed-rows table: line {ln} has {len(cells)} cells, "
                        f"expected {len(header)}: {mlines[ln - 1].strip()[:100]}")
                    continue
                m.upstreamed.append(UpstreamedRow(*cells))
        else:
            seen["map"] += 1
            for ln, cells in rows:
                if len(cells) != len(header):
                    m.problems.append(
                        f"audit-map table: line {ln} has {len(cells)} cells, "
                        f"expected {len(header)}: {mlines[ln - 1].strip()[:100]}")
                    continue
                if cells[1] not in _VALID_CLASSES:
                    m.problems.append(
                        f"audit-map table: line {ln}: class {cells[1]!r} is not "
                        f"one of a/b/c")
                    continue
                m.map_rows.append(MapRow(*cells))

    if seen["applied"] == 0:
        m.problems.append(
            "applied-rows table not found (expected header "
            "'| id | current_sha | origin_base | description | probe |')")
    elif seen["applied"] > 1:
        m.problems.append(f"{seen['applied']} applied-rows tables found; expected exactly one")
    if seen["map"] == 0:
        m.problems.append(
            "audit-map table not found (expected header "
            "'| sha | class | row_id_or_allowlist | description |')")
    elif seen["map"] > 1:
        m.problems.append(f"{seen['map']} audit-map tables found; expected exactly one")
    if seen["upstreamed"] > 1:
        m.problems.append(f"{seen['upstreamed']} upstreamed-rows tables found; expected at most one")

    m.allowlist = [AllowEntry(sha, reason) for sha, reason in _ALLOW_RE.findall(text)]
    return m


# --------------------------------------------------------------------------
# Read-only git plumbing
# --------------------------------------------------------------------------

def _git(repo: str, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True)


def _resolve(repo: str, sha_text: str):
    """Resolve any sha form (8-char, 9-char, full, tag, ...) to a full commit
    sha; None when unresolvable/ambiguous. Never assumes a fixed width."""
    if not sha_text:
        return None
    p = _git(repo, "rev-parse", "--verify", "--quiet", f"{sha_text}^{{commit}}")
    out = p.stdout.strip()
    return out if p.returncode == 0 and out else None


def _rev_range(repo: str, base: str):
    p = _git(repo, "rev-list", "--no-merges", f"{base}..HEAD")
    if p.returncode != 0:
        return None
    return [l for l in p.stdout.split() if l]


def _commit_paths(repo: str, full_sha: str):
    p = _git(repo, "diff-tree", "--no-commit-id", "-r", "--name-only", full_sha)
    if p.returncode != 0:
        return None
    return [l for l in p.stdout.splitlines() if l]


def _subject(repo: str, full_sha: str) -> str:
    p = _git(repo, "show", "-s", "--format=%s", full_sha)
    return p.stdout.strip() if p.returncode == 0 else ""


def _head_branch(repo: str):
    p = _git(repo, "symbolic-ref", "--short", "-q", "HEAD")
    return p.stdout.strip() if p.returncode == 0 and p.stdout.strip() else None


def _patch_id(repo: str, full_sha: str):
    """Per-commit patch-id (git show <sha> | git patch-id --stable) — a
    cumulative base..HEAD diff is never used anywhere in this checker.
    Returns None for commits with no textual diff (e.g. merge commits)."""
    show = _git(repo, "show", full_sha)
    if show.returncode != 0:
        return None
    pid = subprocess.run(
        ["git", "-C", repo, "patch-id", "--stable"],
        input=show.stdout, capture_output=True, text=True,
    )
    out = pid.stdout.strip()
    return out.split()[0] if out else None


def _tag_patch_ids(repo: str, tag: str, cache: dict):
    """The whole-tag per-commit patch-id set, cached per tag within a run
    (a single build's cost, not per-row). Streams git log -p through one
    patch-id process; returns {patch_id: carrier_commit}."""
    if tag in cache:
        return cache[tag]
    log = subprocess.Popen(
        ["git", "-C", repo, "log", "--no-merges", "--root", "-p", "--full-index", tag],
        stdout=subprocess.PIPE,
    )
    pid = subprocess.Popen(
        ["git", "-C", repo, "patch-id", "--stable"],
        stdin=log.stdout, stdout=subprocess.PIPE,
    )
    log.stdout.close()
    out = pid.communicate()[0].decode("utf-8", "replace")
    log.wait()
    ids = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2:
            ids[parts[0]] = parts[1]
    cache[tag] = ids
    return ids


def _tag_tree_files(repo: str, tag: str):
    p = _git(repo, "ls-tree", "-r", "--name-only", tag)
    return [l for l in p.stdout.splitlines() if l] if p.returncode == 0 else None


def _worktree_probe_files(repo: str, glob: str):
    """Files in the worktree matching the probe glob (direct path first; a
    magic glob walks the tree, skipping .git and target/)."""
    direct = os.path.join(repo, glob)
    if os.path.isfile(direct):
        return [glob]
    matched = []
    for root, dirs, files in os.walk(repo):
        dirs[:] = [d for d in dirs if d not in (".git", "target")]
        for f in files:
            rel = os.path.relpath(os.path.join(root, f), repo).replace(os.sep, "/")
            if fnmatch.fnmatch(rel, glob):
                matched.append(rel)
    return matched


def _in_scope(path: str, scope) -> bool:
    return any(path == s or path.startswith(s + "/") for s in scope)


# --------------------------------------------------------------------------
# The checks
# --------------------------------------------------------------------------

def run_checks(repo: str, manifest_path: str, registry: dict = PROBE_REGISTRY) -> tuple:
    """Run all checks; return (exit_code, report_text). Never edits anything."""
    if not os.path.isdir(repo):
        raise EnvError(f"repo path is not a directory: {repo}")
    p = _git(repo, "rev-parse", "--git-dir")
    if p.returncode != 0:
        raise EnvError(f"not a git repository: {repo} ({p.stderr.strip()})")
    if not os.path.isfile(manifest_path):
        raise EnvError(f"manifest not found: {manifest_path}")
    try:
        with open(manifest_path, encoding="utf-8") as fh:
            text = fh.read()
    except OSError as exc:
        raise EnvError(f"manifest unreadable: {manifest_path} ({exc})")

    manifest = parse_manifest(text)
    lines: list[str] = [
        "== fork patch-lineage checker ==",
        f"repo: {repo}",
        f"manifest: {manifest_path}",
    ]
    if manifest.branch and manifest.base:
        lines.append(f"lineage: {manifest.branch} (base {manifest.base}) — "
                     f"reverse-scope: {len(manifest.scope)} path(s)")
    else:
        lines.append("lineage: UNPARSED — the metadata lines are missing/malformed "
                     "(see manifest section below)")

    # Every failure: (check, cls, header, [details]). Sections are rendered as
    # one status line per failure class, details indented underneath.
    failures: list[tuple] = []
    n_fail_sections = 0

    def fail(check: str, cls: str, header: str, details: list | tuple):
        failures.append((check, cls, header, list(details)))

    # -- manifest parse problems (FAIL_M) -----------------------------------
    if manifest.problems:
        fail("manifest", "FAIL_M", f"{len(manifest.problems)} parse problem(s):",
             manifest.problems)
    lines.append(
        f"manifest: OK — {len(manifest.applied)} applied rows, "
        f"{len(manifest.upstreamed)} upstreamed rows, {len(manifest.allowlist)} "
        f"allowlist entries, {len(manifest.map_rows)} audit-map rows"
        if not manifest.problems else
        f"manifest: FAIL_M — {len(manifest.problems)} parse problem(s) (see below)"
    )

    # -- the branch assertion (the metadata parse's first check) ------------
    if manifest.branch is None:
        fail("branch", "FAIL_M",
             "the lineage metadata line is missing/unparseable — cannot assert the branch",
             [])
    else:
        head = _head_branch(repo)
        if head is None:
            fail("branch", "FAIL_M",
                 f"HEAD is detached; the manifest declares lineage '{manifest.branch}'",
                 [])
        elif head != manifest.branch:
            fail("branch", "FAIL_M",
                 f"HEAD is on '{head}' but the manifest declares lineage "
                 f"'{manifest.branch}' (wrong checkout)", [])
        else:
            lines.append(f"branch: OK — HEAD is on '{head}' (the declared lineage branch)")

    # -- resolve the declared base + the lineage range ----------------------
    range_list = None
    base_full = _resolve(repo, manifest.base) if manifest.base else None
    if base_full is None and manifest.base:
        fail("range", "FAIL_M",
             f"the declared base '{manifest.base}' does not resolve", [])
    else:
        range_list = _rev_range(repo, manifest.base)
        if range_list is None:
            fail("range", "FAIL_M",
                 f"git rev-list {manifest.base}..HEAD failed — the declared base is "
                 f"not an ancestor-compatible ref", [])
    range_set = set(range_list) if range_list else set()

    # -- resolve every sha once (never assume a fixed width) ----------------
    row_full: dict = {}       # full sha -> [row ids]
    row_by_id: dict = {}
    allow_full: dict = {}     # full sha -> reason (first wins)
    map_full: dict = {}       # full sha -> [MapRow]
    presence_fail_p, presence_fail_m = [], []

    for row in manifest.applied:
        full = _resolve(repo, row.sha)
        if full is None:
            presence_fail_p.append(
                f"row '{row.rid}': sha '{row.sha}' does not resolve — the patch is "
                f"absent from the repository (a drop or a stale manifest)")
            continue
        row_full.setdefault(full, []).append(row.rid)
        row_by_id[row.rid] = full
        if range_set and full not in range_set:
            presence_fail_p.append(
                f"row '{row.rid}': sha '{row.sha}' is not in "
                f"{manifest.base}..HEAD — the patch is not on the current lineage")
        if row.origin_base and _resolve(repo, row.origin_base) is None:
            presence_fail_m.append(
                f"row '{row.rid}': origin_base '{row.origin_base}' does not resolve")
        if row.probe not in ("-", "") and row.probe not in registry:
            fail("probes", "FAIL_M",
                 "manifest probe-key violation(s):",
                 [f"applied row '{row.rid}' references probe '{row.probe}' which is "
                  f"not in the registry"])

    for entry in manifest.allowlist:
        full = _resolve(repo, entry.sha)
        if full is None:
            fail("map-consistency", "FAIL_M",
                 "allowlist sha(s) that do not resolve:",
                 [f"allowlist entry '{entry.sha}' does not resolve"])
            continue
        allow_full.setdefault(full, entry.reason)

    for mrow in manifest.map_rows:
        full = _resolve(repo, mrow.sha)
        if full is None:
            fail("map-consistency", "FAIL_M",
                 "audit-map sha(s) that do not resolve:",
                 [f"audit-map row '{mrow.sha}' (class {mrow.cls}) does not resolve"])
            continue
        map_full.setdefault(full, []).append(mrow)

    # -- check_presence -------------------------------------------------------
    if manifest.applied:
        if presence_fail_p:
            fail("presence", "FAIL_P",
                 f"{len(presence_fail_p)} applied row(s) unanchored on the lineage:",
                 presence_fail_p)
        if presence_fail_m:
            fail("presence", "FAIL_M",
                 f"{len(presence_fail_m)} row origin_base(s) that do not resolve:",
                 presence_fail_m)
        if not presence_fail_p and not presence_fail_m:
            ids = ", ".join(r.rid for r in manifest.applied)
            lines.append(f"presence: OK — {len(manifest.applied)}/{len(manifest.applied)} "
                         f"applied rows anchored in {manifest.base}..HEAD ({ids})")
    else:
        fail("presence", "FAIL_M", "no applied rows parsed — cannot verify presence", [])

    # -- check_reverse (path-scoped, mechanism carve-out) --------------------
    if range_set and manifest.scope:
        rev = subprocess.run(
            ["git", "-C", repo, "rev-list", "--no-merges",
             f"{manifest.base}..HEAD", "--", *manifest.scope],
            capture_output=True, text=True)
        if rev.returncode != 0:
            fail("reverse", "FAIL_M",
                 f"git rev-list {manifest.base}..HEAD -- <scope> failed: "
                 f"{rev.stderr.strip()}", [])
            scoped = []
        else:
            scoped = [l for l in rev.stdout.split() if l]
        rowed = allowed = exempt = 0
        unmapped = []
        for sha in scoped:
            paths = _commit_paths(repo, sha) or []
            outside = [p for p in paths if p not in MECHANISM_FILES]
            if not outside:
                exempt += 1
                continue
            if sha in row_full:
                rowed += 1
            elif sha in allow_full:
                allowed += 1
            else:
                unmapped.append(
                    f"{sha[:12]} {_subject(repo, sha)}: touches "
                    f"{', '.join(outside[:3])} outside the mechanism set "
                    f"{{PATCHES.md, scripts/check_patches.py, Makefile}} with no row "
                    f"and no allowlist entry")
        if unmapped:
            fail("reverse", "FAIL_R",
                 f"{len(unmapped)} unmapped scoped commit(s):", unmapped)
        else:
            lines.append(f"reverse: OK — {len(scoped)} scoped commits: {rowed} rowed, "
                         f"{allowed} allowlisted, {exempt} mechanism-exempt; "
                         f"0 unmapped")
    elif manifest.scope or not range_set:
        fail("reverse", "FAIL_M",
             "the reverse check could not run (metadata/base problems above)", [])

    # -- the drift rule (row paths ⊆ declared reverse-scope) -----------------
    drift_bad = []
    for row in manifest.applied:
        full = row_by_id.get(row.rid)
        if full is None:
            continue
        paths = _commit_paths(repo, full) or []
        for p in paths:
            if not _in_scope(p, manifest.scope):
                drift_bad.append(
                    f"row '{row.rid}' touches '{p}' — outside the declared "
                    f"reverse-scope")
    if drift_bad:
        fail("drift", "FAIL_D",
             f"{len(drift_bad)} row/path(s) outside the reverse-scope "
             f"(the coverage can never silently narrow):", drift_bad)
    elif manifest.applied and manifest.scope:
        lines.append(f"drift: OK — every applied row's touched paths lie within the "
                     f"reverse-scope ({len(manifest.scope)} path(s))")

    # -- the audit-map shas must be lineage commits ---------------------------
    map_bad_range = []
    for full, rows_ in map_full.items():
        if range_set and full not in range_set:
            map_bad_range.append(
                f"audit-map sha {full[:12]} is outside {manifest.base}..HEAD")
    if map_bad_range:
        fail("map-consistency", "FAIL_M",
             f"{len(map_bad_range)} audit-map sha(s) outside the lineage range:",
             map_bad_range)

    # -- check_map_consistency (FAIL_A: three rules + the a/b↔rows split) ----
    rule1, rule2, rule3, split = [], [], [], []
    map_ab = {f for f, rows_ in map_full.items() if any(r.cls in ("a", "b") for r in rows_)}
    map_c = {f for f, rows_ in map_full.items() if any(r.cls == "c" for r in rows_)}
    for full in allow_full:
        if full in row_full:
            rule1.append(
                f"rule 1: allowlisted sha {full[:12]} ('{allow_full[full]}') also "
                f"carries applied row(s): {', '.join(row_full[full])}")
    for full in map_c:
        if full not in allow_full:
            rule2.append(
                f"rule 2: class-c audit-map sha {full[:12]} is missing from the "
                f"allowlist")
    for full in allow_full:
        if full not in map_c:
            rule2.append(
                f"rule 2: allowlist entry {full[:12]} ('{allow_full[full]}') is not "
                f"class-c in the audit map")
    for full in map_ab:
        if full not in row_full:
            split.append(
                f"a/b coverage: class-a/b audit-map sha {full[:12]} has no applied "
                f"row")
    for full in row_full:
        if full not in map_ab:
            split.append(
                f"a/b coverage: applied row(s) {', '.join(row_full[full])} at "
                f"{full[:12]} have no class-a/b audit-map entry")
    asserted_tags = []
    for urow in manifest.upstreamed:
        if urow.asserted_in not in asserted_tags:
            asserted_tags.append(urow.asserted_in)
    # Rule 3's target sets — the prior-lineage family: the asserted tags PLUS
    # the lineage's declared base tag (the controller's ruling; the expansion
    # also closes the strict-reading vacuity where a manifest with zero
    # upstreamed rows would otherwise leave rule 3 with no target sets).
    rule3_targets = []
    for tag in asserted_tags + ([manifest.base] if manifest.base else []):
        if tag and tag not in rule3_targets and _resolve(repo, tag) is not None:
            rule3_targets.append(tag)
    pid_cache: dict = {}
    for full, reason in allow_full.items():
        pid = _patch_id(repo, full)
        if pid is None:
            continue
        hits = [t for t in rule3_targets if pid in _tag_patch_ids(repo, t, pid_cache)]
        if hits:
            rule3.append(
                f"rule 3: allowlisted sha {full[:12]} ('{reason}') has per-commit "
                f"patch-id {pid} present in prior-tag patch-id set(s) "
                f"{', '.join(repr(t) for t in hits)} — an upstream-era replay "
                f"misfiled as a fixup")
    if rule1 or rule2 or rule3 or split:
        details = rule1 + rule2 + rule3 + split
        fail("map-consistency", "FAIL_A",
             f"{len(rule1)} rule-1 + {len(rule2)} rule-2 + {len(rule3)} rule-3 + "
             f"{len(split)} a/b-coverage violation(s):", details)
    else:
        lines.append(
            f"map-consistency: OK — {len(manifest.map_rows)} audit-map rows "
            f"(a/b {len(map_ab)}, c {len(map_c)}); rows↔a/b {len(row_full)}="
            f"{len(map_ab)}; class-c↔allowlist bijection {len(map_c)}="
            f"{len(allow_full)}; overlap ∅; no upstream-era/prior-lineage "
            f"replay among allowlisted (asserted tags + the base tag)")

    # -- check_upstreamed ------------------------------------------------------
    if manifest.upstreamed:
        u_fail = []
        proven_pid = proven_probe = 0
        for urow in manifest.upstreamed:
            if urow.upstream_sha == TBD_RESOLVE:
                u_fail.append(
                    f"row '{urow.rid}': TBD-RESOLVE — the upstreamed claim is "
                    f"unresolved (exit 1, never a pass and never exit 2)")
                continue
            origin_full = _resolve(repo, urow.origin)
            up_full = _resolve(repo, urow.upstream_sha)
            if origin_full is None:
                u_fail.append(f"row '{urow.rid}': origin commit '{urow.origin}' "
                              f"does not resolve")
                continue
            if up_full is None:
                u_fail.append(f"row '{urow.rid}': upstream_sha '{urow.upstream_sha}' "
                              f"does not resolve")
                continue
            if _resolve(repo, urow.asserted_in) is None:
                u_fail.append(f"row '{urow.rid}': asserted tag '{urow.asserted_in}' "
                              f"does not resolve")
                continue
            ids = _tag_patch_ids(repo, urow.asserted_in, pid_cache)
            pid = _patch_id(repo, origin_full)
            if pid and pid in ids:
                proven_pid += 1
                continue
            if urow.probe not in ("-", ""):
                spec = registry.get(urow.probe)
                if spec is None:
                    u_fail.append(
                        f"row '{urow.rid}': probe '{urow.probe}' is not in the "
                        f"registry")
                    continue
                files = _tag_tree_files(repo, urow.asserted_in) or []
                count = 0
                for f in files:
                    if not fnmatch.fnmatch(f, spec["glob"]):
                        continue
                    blob = _git(repo, "show", f"{urow.asserted_in}:{f}")
                    if blob.returncode == 0:
                        count += len(re.findall(spec["pattern"], blob.stdout))
                if count >= spec["min_count"]:
                    proven_probe += 1
                else:
                    u_fail.append(
                        f"row '{urow.rid}': origin patch-id absent from "
                        f"'{urow.asserted_in}'s set and the probe '{urow.probe}' "
                        f"found {count} match(es) in the tag tree "
                        f"(min {spec['min_count']})")
            else:
                u_fail.append(
                    f"row '{urow.rid}': origin patch-id "
                    f"{pid if pid else '(none)'} is absent from "
                    f"'{urow.asserted_in}'s patch-id set; no probe to fall back on "
                    f"(ancestry alone is not proof)")
        if u_fail:
            fail("upstreamed", "FAIL_U", f"{len(u_fail)} unproven claim(s):", u_fail)
        else:
            lines.append(f"upstreamed: OK — {len(manifest.upstreamed)}/"
                         f"{len(manifest.upstreamed)} claims proven "
                         f"(patch-id {proven_pid}, tag-tree probe {proven_probe})")
    else:
        lines.append("upstreamed: OK — no upstreamed claims declared")

    # -- run_probes (the registry against the worktree) -----------------------
    probe_fail = []
    probe_ok = []
    for name, spec in registry.items():
        files = _worktree_probe_files(repo, spec["glob"])
        if not files:
            probe_fail.append(
                f"probe '{name}': glob '{spec['glob']}' matches no file in the "
                f"worktree")
            continue
        count = 0
        for rel in files:
            with open(os.path.join(repo, rel), encoding="utf-8", errors="replace") as fh:
                count += len(re.findall(spec["pattern"], fh.read()))
        if count < spec["min_count"]:
            probe_fail.append(
                f"probe '{name}': expected >= {spec['min_count']} match(es) of "
                f"'{spec['pattern']}' in '{spec['glob']}', found {count}")
        else:
            probe_ok.append(f"{name} {count}/{spec['min_count']}")
    if probe_fail:
        fail("probes", "FAIL_B", f"{len(probe_fail)} probe(s) below the floor:",
             probe_fail)
    else:
        lines.append("probes: OK — " + "; ".join(probe_ok))

    # -- THE REPORT: the trust boundary's only surface ------------------------
    check_order = ["manifest", "branch", "range", "presence", "reverse", "drift",
                   "map-consistency", "upstreamed", "probes"]
    for check in check_order:
        mine = [f for f in failures if f[0] == check]
        for _, cls, header, details in mine:
            n_fail_sections += 1
            lines.append(f"{check}: {cls} — {header}")
            for d in details:
                lines.append(f"    {d}")
    leftover = [f for f in failures if f[0] not in check_order]
    for _, cls, header, details in leftover:
        n_fail_sections += 1
        lines.append(f"checker: {cls} — {header}")
        for d in details:
            lines.append(f"    {d}")

    lines.append(f"-- allowlist ({len(manifest.allowlist)}) --")
    for entry in manifest.allowlist:
        lines.append(f"  {entry.sha} — {entry.reason}")
    if not manifest.allowlist:
        lines.append("  (empty)")
    class_c = [mr for mr in manifest.map_rows if mr.cls == "c"]
    lines.append(f"-- audit-map class-c ({len(class_c)}) --")
    for mr in class_c:
        lines.append(f"  {mr.sha} — {mr.description}")
    if not class_c:
        lines.append("  (empty)")

    if n_fail_sections == 0:
        lines.append("result: GREEN — every patch in the lineage is accounted for; "
                     "the allowlist + class-c enumeration above is the trust "
                     "boundary: no drop can pass without appearing on this report")
        return (0, "\n".join(lines) + "\n")
    lines.append(f"result: RED — {n_fail_sections} failure section(s); "
                 "every patch must map to a row or the allowlist, and the "
                 "allowlist + class-c enumeration above must be human-verified")
    return (1, "\n".join(lines) + "\n")


# ==========================================================================
# The selftest harness (--selftest)
# ==========================================================================
#
# Eleven fixtures, thirteen enumerated cases (the list is authoritative):
#   the deep green lineage (6+ commits, two generations, a moved-site replay,
#   a realignment fixup, a tooling commit OUT of scope, a scoped-path
#   unlisted commit, an upstream-era replay commit; heterogeneous
#   row-bases) plus ten derived variants.
#
# (1)  presence-rewritten-sha        the clean era replay (identical
#                                    per-commit patch-id, rewritten sha,
#                                    its own row) verifies GREEN; the
#                                    moved-site replay anchors at the
#                                    squash (patch-id equality NOT required
#                                    for era rows).
# (2)  false-upstream-claim absent   an origin whose patch-id is absent from
#                                    the asserted tag's set -> FAIL_U.
# (3)  false-upstream-claim
#      unrelated-ancestor            the origin is a MERGE commit — a true
#                                    ancestor of the asserted tag — but its
#                                    per-commit patch-id is absent from the
#                                    tag's set -> FAIL_U (ancestry is not
#                                    proof).
# (4)  unlisted-scoped-commit        a scoped-path commit with no row and no
#                                    allowlist entry -> FAIL_R.
# (5)  out-of-scope-tooling-commit   a PATCHES.md-only commit (scoped path,
#                                    mechanism surface) passes with no row
#                                    and no allowlist entry; a docs/notes.md
#                                    commit is outside the scope entirely.
# (6)  heterogeneous-bases           rows with origin_base v0.9 (era) and
#                                    v1.0 (native) both verify GREEN.
# (7)  allowlist∩rows overlap        FAIL_A rule 1.
# (8)  class-c↔allowlist bijection
#      violation                     an allowlist entry that is not class-c
#                                    in the audit map -> FAIL_A rule 2.
# (9)  upstream-era-replay-in-
#      allowlist                     an allowlisted commit whose patch-id is
#                                    in the asserted tag's cached set ->
#                                    FAIL_A rule 3.
# (9b) base-tag-native-replay-in-
#      allowlist (fix round 1)       an allowlisted commit whose patch-id is
#                                    in the BASE tag's set but NOT the prior
#                                    asserted tag's — only the expanded arm
#                                    (asserted tags + base) catches it; the
#                                    case also drops the upstreamed rows,
#                                    pinning the zero-asserted-tags vacuity.
# (10) row-path-outside-reverse-
#      scope                         a row touching a path outside the
#                                    declared scope -> FAIL_D.
# (11) TBD-RESOLVE                   an unresolved upstreamed claim -> exit 1
#                                    exactly (never exit 2), FAIL_U.
# (12) one-of-two-sites probe        the delegate-scan probe finds 1 of the
#                                    required 2 sites -> FAIL_B.
# (13) the GREEN report carries      the full allowlist enumeration + the
#      the enumeration               map's class-c set on every run.

FIXTURE_REGISTRY: dict = {
    "delegate-scan": {
        "glob": "crates/runtime/src/delegate.rs",
        "pattern": r"root_config.*allow_scripts",
        "min_count": 2,
    },
    "poll-alive": {
        "glob": "crates/channels/src/orchestrator/mqtt.rs",
        "pattern": r"record_poll_alive",
        "min_count": 1,
    },
    "up-content": {
        "glob": "crates/runtime/src/lib.rs",
        "pattern": r"fn upstream_adapted",
        "min_count": 1,
    },
}


# ---- harness git plumbing -------------------------------------------------

def _h_git(repo: str, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    p = subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True)
    if check and p.returncode != 0:
        raise RuntimeError(f"fixture git {args} failed: {p.stderr.strip()}")
    return p


def _h_out(repo: str, *args: str) -> str:
    return _h_git(repo, *args).stdout.strip()


def _h_commit(repo: str, msg: str) -> str:
    _h_git(repo, "add", "-A")
    _h_git(repo, "commit", "-q", "-m", msg)
    return _h_out(repo, "rev-parse", "HEAD")


def _h_short(repo: str, sha: str, width: int) -> str:
    return _h_out(repo, "rev-parse", f"--short={width}", sha)


def _h_write(repo: str, rel: str, content: str) -> None:
    path = os.path.join(repo, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as fh:
        fh.write(content)


def _h_unlink(repo: str, rel: str) -> None:
    os.unlink(os.path.join(repo, rel))


def _h_patch_id(repo: str, sha: str):
    show = subprocess.run(["git", "-C", repo, "show", sha], capture_output=True, text=True)
    pid = subprocess.run(
        ["git", "-C", repo, "patch-id", "--stable"],
        input=show.stdout, capture_output=True, text=True,
    )
    out = pid.stdout.strip()
    return out.split()[0] if out else None


def _h_is_ancestor(repo: str, sha: str, ref: str) -> bool:
    return _h_git(repo, "merge-base", "--is-ancestor", sha, ref, check=False).returncode == 0


# ---- the deep lineage fixture --------------------------------------------
#
# History (branch cheknet-lineage):
#   C0 base content
#   F1 (branch feature) + M (merge --no-ff)      <- the merge ancestor (case 3)
#   E1 era patch "era_one" in lib.rs             <- moved-site replay source
#   E2 era patch "era_two.rs" (new file)         <- clean replay source
#   UPT upstream-native "upstream_native"
#   ADPT upstream "upstream_adapted" (adapted position)
#   tag v0.9                                     <- prior era tag
#   branch old-era off E1: O_PROV (twin of UPT), O_ABS, O_ADPT
#   R1 the v1.0 release: drops both era patches, renames base -> base_v2
#   tag v1.0                                     <- lineage base
#   SQUASH  replays era_one at a MOVED site (patch-id != E1) + Cargo.toml
#   FIXUP   realignment fixup (class-c, allowlisted)
#   REPLAY  cherry-pick of E2 (patch-id == E2, rewritten sha)
#   N1      delegate scan sites (the U1 analog, probe delegate-scan)
#   N2      record_poll_alive (the mqtt analog, probe poll-alive)
#   TA      PATCHES.md-only (mechanism surface; scoped path; no row, no entry)
#   TB      docs/notes.md-only (outside the scope entirely)
#
# reverse-scope: crates/runtime, crates/channels, src/main.rs, Cargo.toml,
# PATCHES.md.

LIB_C0 = "fn base() {\n}\n"
LIB_F1 = "fn feature_a() {}\nfn base() {\n}\n"
LIB_E1 = "fn feature_a() {}\nfn base() {\n}\npub fn era_one() {}\n"
LIB_UPT = LIB_E1 + "fn upstream_native() {}\n"
LIB_ADPT = LIB_UPT + "fn upstream_adapted() {} // adapted position\n"
LIB_O_PROV = LIB_UPT  # same parent tree (E1) as UPT -> identical diff -> identical patch-id
LIB_O_ABS = "fn absent_claim() {}\n" + LIB_UPT
LIB_O_ADPT = "fn upstream_adapted() {}\n" + LIB_O_ABS
LIB_R1 = (
    "fn feature_a() {}\n"
    "fn base_v2() {\n}\n"
    "fn upstream_native() {}\n"
    "fn upstream_adapted() {} // adapted position\n"
)
LIB_SQUASH = (
    "fn feature_a() {}\n"
    "fn base_v2() {\n}\n"
    "pub fn era_one() {}\n"  # moved site: context differs from E1's diff
    "fn upstream_native() {}\n"
    "fn upstream_adapted() {} // adapted position\n"
)
CARGO_C0 = "[package]\nname = \"fx\"\nversion = \"0.1.0\"\n"
CARGO_R1 = CARGO_C0 + "upstream-dep = \"1\"\n"
CARGO_SQUASH = CARGO_R1 + "cheknet-dep = \"1\"\n"
CARGO_FIXUP = CARGO_SQUASH + "fixup-artifact = true\n"
MQTT_C0 = "fn poll() {\n}\n"
MQTT_N2 = "fn poll() {\n}\nfn record_poll_alive() {}\n"


def _build_fixture(where: str, delegate_sites: int = 2, extra: list | None = None,
                   base_native_replay: bool = False) -> dict:
    """Build the deep lineage fixture; return the named shas.

    With base_native_replay=True the fixture additionally carries the
    base-tag-native replay arm: a commit BN in the v1.0 base history whose
    content the release drops, then a clean cherry-pick of BN (REPLAY-BN)
    later in the lineage — its per-commit patch-id is in the BASE tag's set
    but NOT in the prior-era tag's (v0.9's) set, so only the expanded rule-3
    arm (asserted tags + base) can catch it being misfiled as a fixup.
    """
    repo = os.path.join(where, "repo")
    os.makedirs(repo)
    _h_git(repo, "init", "-q")
    _h_git(repo, "symbolic-ref", "HEAD", "refs/heads/cheknet-lineage")
    _h_git(repo, "config", "user.email", "selftest@fixture.local")
    _h_git(repo, "config", "user.name", "fixture selftest")
    _h_git(repo, "config", "commit.gpgsign", "false")

    _h_write(repo, "crates/runtime/src/lib.rs", LIB_C0)
    _h_write(repo, "crates/channels/src/orchestrator/mqtt.rs", MQTT_C0)
    _h_write(repo, "src/main.rs", "fn main() {}\n")
    _h_write(repo, "Cargo.toml", CARGO_C0)
    _h_write(repo, "docs/notes.md", "notes\n")
    _h_write(repo, "Makefile", "build:\n\techo build\n")
    _h_write(repo, "PATCHES.md", "# fixture PATCHES\n")
    _h_write(repo, "scripts/check_patches.py", "# fixture mechanism placeholder\n")
    c0 = _h_commit(repo, "C0: base content")

    _h_git(repo, "checkout", "-q", "-b", "feature")
    _h_write(repo, "crates/runtime/src/lib.rs", LIB_F1)
    f1 = _h_commit(repo, "F1: feature_a")
    _h_git(repo, "checkout", "-q", "cheknet-lineage")
    _h_git(repo, "merge", "--no-ff", "-m", "M: merge feature", "feature")
    m = _h_out(repo, "rev-parse", "HEAD")

    _h_write(repo, "crates/runtime/src/lib.rs", LIB_E1)
    e1 = _h_commit(repo, "E1: era_one (pre-v0.9 era patch)")
    _h_write(repo, "crates/runtime/src/era_two.rs", "pub fn era_two_help() {}\n")
    e2 = _h_commit(repo, "E2: era_two (new file)")
    _h_write(repo, "crates/runtime/src/lib.rs", LIB_UPT)
    upt = _h_commit(repo, "UPT: upstream_native (the upstream twin)")
    _h_write(repo, "crates/runtime/src/lib.rs", LIB_ADPT)
    adpt = _h_commit(repo, "ADPT: upstream_adapted (adapted position)")
    _h_git(repo, "tag", "v0.9")

    _h_git(repo, "branch", "old-era", e1)
    _h_git(repo, "checkout", "-q", "old-era")
    _h_write(repo, "crates/runtime/src/lib.rs", LIB_O_PROV)
    o_prov = _h_commit(repo, "O_PROV: origin twin of UPT")
    _h_write(repo, "crates/runtime/src/lib.rs", LIB_O_ABS)
    o_abs = _h_commit(repo, "O_ABS: origin of the absent claim")
    _h_write(repo, "crates/runtime/src/lib.rs", LIB_O_ADPT)
    o_adpt = _h_commit(repo, "O_ADPT: origin of the adapted claim")
    _h_git(repo, "checkout", "-q", "cheknet-lineage")

    bn = None
    if base_native_replay:
        _h_write(repo, "crates/runtime/src/base_native.rs",
                 "pub fn base_native_help() {}\n")
        bn = _h_commit(repo, "BN: base-native helper (pre-v1.0 history)")
    _h_write(repo, "crates/runtime/src/lib.rs", LIB_R1)
    _h_unlink(repo, "crates/runtime/src/era_two.rs")
    if base_native_replay:
        _h_unlink(repo, "crates/runtime/src/base_native.rs")
    _h_write(repo, "Cargo.toml", CARGO_R1)
    _h_commit(repo, "R1: the v1.0 release (drops the era patches)")
    _h_git(repo, "tag", "v1.0")

    _h_write(repo, "crates/runtime/src/lib.rs", LIB_SQUASH)
    _h_write(repo, "Cargo.toml", CARGO_SQUASH)
    squash = _h_commit(repo, "SQUASH: realign cheknet patches onto v1.0")
    _h_write(repo, "Cargo.toml", CARGO_FIXUP)
    fixup = _h_commit(repo, "FIXUP: repair the realignment Cargo.toml artifact")
    _h_git(repo, "cherry-pick", e2)
    replay = _h_out(repo, "rev-parse", "HEAD")
    replay_bn = None
    if base_native_replay:
        _h_git(repo, "cherry-pick", bn)
        replay_bn = _h_out(repo, "rev-parse", "HEAD")

    sites = "\n".join(
        f"fn scan_{i}() {{ let v{i} = root_config.slot{i}.allow_scripts; }}"
        for i in range(1, delegate_sites + 1)
    )
    _h_write(repo, "crates/runtime/src/delegate.rs", sites + "\n")
    n1 = _h_commit(repo, "N1: the delegate scan fix (both sites)")
    _h_write(repo, "crates/channels/src/orchestrator/mqtt.rs", MQTT_N2)
    n2 = _h_commit(repo, "N2: the poll liveness stamp")
    _h_write(repo, "PATCHES.md", "# fixture PATCHES\nfixture bookkeeping\n")
    ta = _h_commit(repo, "TA: PATCHES.md bookkeeping (mechanism surface)")
    _h_write(repo, "docs/notes.md", "notes\nfixture note\n")
    tb = _h_commit(repo, "TB: docs/notes.md bookkeeping (outside the scope)")

    extras = {}
    for label, (rel, content, msg) in (extra or []):
        _h_write(repo, rel, content)
        extras[label] = _h_commit(repo, msg)

    return {
        "repo": repo,
        "c0": c0, "f1": f1, "m": m, "e1": e1, "e2": e2, "upt": upt, "adpt": adpt,
        "o_prov": o_prov, "o_abs": o_abs, "o_adpt": o_adpt,
        "bn": bn, "squash": squash, "fixup": fixup, "replay": replay,
        "replay_bn": replay_bn, "n1": n1, "n2": n2,
        "ta": ta, "tb": tb, "extras": extras,
    }


# ---- the fixture manifest -------------------------------------------------

MANIFEST_TEMPLATE = """# Fixture patch manifest

### Applied rows

| id | current_sha | origin_base | description | probe |
|----|-------------|-------------|-------------|-------|
| era-one | {sq9} | v0.9 | the moved-site era replay (the realignment squash carrier) | - |
| era-two | {rp8} | v0.9 | the clean era replay — rewritten sha, identical per-commit patch-id | - |
| native-delegate | {n1_9} | v1.0 | the delegate scan fix — both sites | delegate-scan |
| native-poll | {n2_8} | v1.0 | the poll liveness stamp | poll-alive |

### Upstreamed rows

| id | origin_commit | asserted_in | upstream_sha | probe | description |
|----|---------------|-------------|--------------|-------|-------------|
| U-PROV | {op9} | v0.9 | {upt9} | - | the proven twin claim — the origin patch-id is in the asserted tag's set |
| U-ADPT | {oa8} | v0.9 | {ad9} | up-content | the adapted claim — probe-proven against the tag tree |

## Checker allowlist

allowlist: {fx9} — realignment-fixup: repair the realignment Cargo.toml artifact

## The 2026-10-02 lineage audit

lineage: cheknet-lineage base v1.0

reverse-scope: crates/runtime, crates/channels, src/main.rs, Cargo.toml, PATCHES.md

| sha | class | row_id_or_allowlist | description |
|-----|-------|---------------------|-------------|
| {sq9} | b | era-one | the realignment squash — moved-site era replay |
| {rp8} | b | era-two | the clean era replay |
| {n1_9} | a | native-delegate | the delegate scan fix |
| {n2_8} | a | native-poll | the poll liveness stamp |
| {fx9} | c | realignment-fixup | the Cargo.toml repair |
"""


def _green_manifest(fx: dict) -> str:
    r = fx["repo"]
    return MANIFEST_TEMPLATE.format(
        sq9=_h_short(r, fx["squash"], 9),
        rp8=_h_short(r, fx["replay"], 8),
        n1_9=_h_short(r, fx["n1"], 9),
        n2_8=_h_short(r, fx["n2"], 8),
        op9=_h_short(r, fx["o_prov"], 9),
        upt9=_h_short(r, fx["upt"], 9),
        oa8=_h_short(r, fx["o_adpt"], 8),
        ad9=_h_short(r, fx["adpt"], 9),
        fx9=_h_short(r, fx["fixup"], 9),
    )


def _write_manifest(where: str, name: str, text: str) -> str:
    path = os.path.join(where, name)
    with open(path, "w") as fh:
        fh.write(text)
    return path


# ---- case runner -----------------------------------------------------------

def _run_selftest() -> int:
    pinned_ok = PROBE_REGISTRY == {
        "allow_scripts-config-read": {
            "glob": "crates/zeroclaw-runtime/src/tools/delegate.rs",
            "pattern": r"root_config.*allow_scripts",
            "min_count": 2,
        },
        "mqtt-poll-alive": {
            "glob": "crates/zeroclaw-channels/src/orchestrator/mqtt.rs",
            "pattern": r"record_poll_alive",
            "min_count": 1,
        },
    }
    print("== check_patches selftest ==")
    print(f"registry pinning (both entries, real paths, min_counts): "
          f"{'PASS' if pinned_ok else 'FAIL — PROBE_REGISTRY drifted from the pinned spec'}")

    results = []
    with tempfile.TemporaryDirectory(prefix="check-patches-selftest-") as td:
        # Fixture 1: the deep green lineage.
        fxg = _build_fixture(os.path.join(td, "01-green"))
        green_manifest = _write_manifest(td, "manifest-green.md", _green_manifest(fxg))

        # Fixture 2: green + SCRUFF (an unlisted scoped commit).
        fxs = _build_fixture(
            os.path.join(td, "02-unlisted"),
            extra=[("scruff", ("crates/channels/src/orchestrator/mqtt.rs",
                               MQTT_N2 + "fn unlisted() {}\n", "SCRUFF: unlisted scoped commit"))],
        )
        scruff_manifest = _write_manifest(td, "manifest-unlisted.md", _green_manifest(fxs))

        # Fixture 3: green + EXTRA (an allowlist entry absent from the map).
        fxe = _build_fixture(
            os.path.join(td, "03-bijection"),
            extra=[("extra", ("docs/notes.md", "notes\nfixture note\nextra\n",
                              "EXTRA: another docs commit"))],
        )
        bij_manifest = _write_manifest(td, "manifest-bijection.md", _green_manifest(fxe))

        # Fixture 4: the one-site delegate (min_count fails).
        fx1 = _build_fixture(os.path.join(td, "04-onesite"), delegate_sites=1)
        onesite_manifest = _write_manifest(td, "manifest-onesite.md", _green_manifest(fx1))

        rg = fxg["repo"]
        cases = []

        def add(num, label, repo, manifest, expect_exit, must_contain, extra_checks=None):
            cases.append((num, label, repo, manifest, expect_exit,
                          must_contain, extra_checks or (lambda ctx: None)))

        # -- the GREEN fixture ------------------------------------------------
        def green_sanity(ctx):
            if _h_patch_id(rg, fxg["replay"]) != _h_patch_id(rg, fxg["e2"]):
                return "fixture invalid: REPLAY's per-commit patch-id != E2's (not a rewritten-sha replay)"
            if _h_patch_id(rg, fxg["squash"]) == _h_patch_id(rg, fxg["e1"]):
                return "fixture invalid: SQUASH's patch-id == E1's (not a moved-site replay)"
            return None

        add(1, "presence-rewritten-sha", rg, green_manifest, 0,
            ["result: GREEN", "era-two"], green_sanity)
        add(5, "out-of-scope-tooling-commit (passes)", rg, green_manifest, 0,
            ["1 mechanism-exempt", "result: GREEN"])
        add(6, "heterogeneous-bases", rg, green_manifest, 0,
            ["result: GREEN", "presence: OK"], green_sanity)
        add(13, "GREEN report carries the allowlist + class-c enumeration", rg, green_manifest, 0,
            ["-- allowlist (1) --", "-- audit-map class-c (1) --",
             "realignment-fixup: repair the realignment Cargo.toml artifact",
             "the Cargo.toml repair", "result: GREEN"])

        # -- (2) false upstreamed claim, absent -------------------------------
        def absent_sanity(ctx):
            if _h_patch_id(rg, fxg["o_abs"]) in _tag_set_fixture(rg, "v0.9"):
                return "fixture invalid: O_ABS's patch-id IS in v0.9's set"
            return None

        adpt_row = ("| U-ADPT | {} | v0.9 | {} | up-content | the adapted claim — "
                    "probe-proven against the tag tree |").format(
            _h_short(rg, fxg["o_adpt"], 8), _h_short(rg, fxg["adpt"], 9))
        m2 = _green_manifest(fxg).replace(
            adpt_row,
            adpt_row + "\n" +
            "| U-ABS | {} | v0.9 | {} | - | the false claim — origin patch-id absent "
            "from the tag set |".format(_h_short(rg, fxg["o_abs"], 8),
                                        _h_short(rg, fxg["o_abs"], 9)))
        add(2, "false-upstream-claim absent", rg, _write_manifest(td, "m2.md", m2), 1,
            ["upstreamed: FAIL_U", "U-ABS"], absent_sanity)

        # -- (3) false upstreamed claim, unrelated ancestor --------------------
        def merge_sanity(ctx):
            if not _h_is_ancestor(rg, fxg["m"], "v0.9"):
                return "fixture invalid: the merge M is not an ancestor of v0.9"
            if _h_patch_id(rg, fxg["m"]) in _tag_set_fixture(rg, "v0.9"):
                return "fixture invalid: the merge's patch-id IS in v0.9's set"
            return None

        m3 = _green_manifest(fxg).replace(
            adpt_row,
            adpt_row + "\n" +
            "| U-MERGE | {} | v0.9 | {} | - | the false claim — a merge ancestor is "
            "not patch-id proof |".format(_h_short(rg, fxg["m"], 9),
                                          _h_short(rg, fxg["m"], 8)))
        add(3, "false-upstream-claim unrelated-ancestor", rg, _write_manifest(td, "m3.md", m3), 1,
            ["upstreamed: FAIL_U", "U-MERGE"], merge_sanity)

        # -- (4) unlisted scoped commit ---------------------------------------
        add(4, "unlisted-scoped-commit", fxs["repo"], scruff_manifest, 1,
            ["reverse: FAIL_R", "SCRUFF"])

        # -- (7) allowlist∩rows overlap ----------------------------------------
        m7 = _green_manifest(fxg) + (
            "allowlist: {} — realignment-fixup: misfiled (also carries applied rows)\n"
        ).format(_h_short(rg, fxg["squash"], 9))
        add(7, "allowlist∩rows overlap (FAIL_A rule 1)", rg, _write_manifest(td, "m7.md", m7), 1,
            ["map-consistency: FAIL_A", "rule 1"])

        # -- (8) class-c↔allowlist bijection violation -------------------------
        m8 = _green_manifest(fxe) + (
            "allowlist: {} — tooling: extra entry absent from the audit map\n"
        ).format(_h_short(fxe["repo"], fxe["extras"]["extra"], 8))
        add(8, "class-c↔allowlist bijection violation (FAIL_A rule 2)", fxe["repo"],
            _write_manifest(td, "m8.md", m8), 1,
            ["map-consistency: FAIL_A", "rule 2"])

        # -- (9) upstream-era replay in the allowlist --------------------------
        rp9 = _h_short(rg, fxg["replay"], 9)
        rp8 = _h_short(rg, fxg["replay"], 8)
        m9 = _green_manifest(fxg)
        m9 = m9.replace(
            "| era-two | {} | v0.9 | the clean era replay — rewritten sha, identical per-commit patch-id | - |\n".format(rp8),
            "")
        m9 = m9.replace(
            "| {} | b | era-two | the clean era replay |".format(rp8),
            "| {} | c | realignment-fixup | the clean era replay — misfiled |".format(rp9))
        m9 += "\nallowlist: {} — realignment-fixup: misfiled era replay\n".format(rp8)
        add(9, "upstream-era-replay-in-allowlist (FAIL_A rule 3)", rg,
            _write_manifest(td, "m9.md", m9), 1,
            ["map-consistency: FAIL_A", "rule 3"])

        # -- (9b) base-tag-native replay in the allowlist (fix round 1) -------
        # The expanded rule-3 arm: an allowlisted commit whose per-commit
        # patch-id lives in the BASE tag's (v1.0's) set but NOT in the prior
        # asserted tag's (v0.9's) — the strict reading could not catch it.
        # The case manifest also drops the upstreamed rows entirely, pinning
        # the vacuity closure: with zero asserted tags, the base tag is the
        # only rule-3 target and the misfiling still fails.
        fxb = _build_fixture(os.path.join(td, "05-base-native"),
                             base_native_replay=True)
        rb_repo = fxb["repo"]

        def base_arm_sanity(ctx):
            if _h_patch_id(rb_repo, fxb["replay_bn"]) != _h_patch_id(rb_repo, fxb["bn"]):
                return ("fixture invalid: REPLAY-BN's per-commit patch-id != BN's "
                        "(not a clean base-tag-native replay)")
            if _h_patch_id(rb_repo, fxb["replay_bn"]) in _tag_set_fixture(rb_repo, "v0.9"):
                return ("fixture invalid: REPLAY-BN's patch-id IS in v0.9's set — "
                        "the strict arm would already catch it (not a base-arm case)")
            if not _h_is_ancestor(rb_repo, fxb["bn"], "v1.0"):
                return "fixture invalid: BN is not an ancestor of the base tag v1.0"
            if _h_is_ancestor(rb_repo, fxb["bn"], "v0.9"):
                return "fixture invalid: BN is an ancestor of v0.9 (not base-native)"
            return None

        m9b = _green_manifest(fxb)
        for u_line in (
            "| U-PROV | {} | v0.9 | {} | - | the proven twin claim — the origin patch-id is in the asserted tag's set |".format(
                _h_short(rb_repo, fxb["o_prov"], 9), _h_short(rb_repo, fxb["upt"], 9)),
            "| U-ADPT | {} | v0.9 | {} | up-content | the adapted claim — probe-proven against the tag tree |".format(
                _h_short(rb_repo, fxb["o_adpt"], 8), _h_short(rb_repo, fxb["adpt"], 9)),
        ):
            m9b = m9b.replace(u_line + "\n", "")
        rb8 = _h_short(rb_repo, fxb["replay_bn"], 8)
        rb9 = _h_short(rb_repo, fxb["replay_bn"], 9)
        m9b = m9b.replace(
            "| {} | c | realignment-fixup | the Cargo.toml repair |".format(
                _h_short(rb_repo, fxb["fixup"], 9)),
            "| {} | c | realignment-fixup | the Cargo.toml repair |\n"
            "| {} | c | realignment-fixup | the base-native replay — misfiled |".format(
                _h_short(rb_repo, fxb["fixup"], 9), rb8))
        m9b += "\nallowlist: {} — realignment-fixup: misfiled base-native replay\n".format(rb9)
        add("9b", "base-tag-native-replay-in-allowlist (FAIL_A rule 3, the base arm)",
            rb_repo, _write_manifest(td, "m9b.md", m9b), 1,
            ["map-consistency: FAIL_A", "rule 3", "'v1.0'",
             "no upstreamed claims declared"], base_arm_sanity)

        # -- (10) row path outside the reverse-scope ----------------------------
        tb9 = _h_short(rg, fxg["tb"], 9)
        n2_8 = _h_short(rg, fxg["n2"], 8)
        fx9 = _h_short(rg, fxg["fixup"], 9)
        m10 = _green_manifest(fxg)
        m10 = m10.replace(
            "| native-poll | {} | v1.0 | the poll liveness stamp | poll-alive |\n".format(n2_8),
            "| native-poll | {} | v1.0 | the poll liveness stamp | poll-alive |\n"
            "| native-doc | {} | v1.0 | the docs-touching patch | - |\n".format(n2_8, tb9))
        m10 = m10.replace(
            "| {} | c | realignment-fixup | the Cargo.toml repair |".format(fx9),
            "| {} | a | native-doc | the docs-touching patch |\n"
            "| {} | c | realignment-fixup | the Cargo.toml repair |".format(tb9, fx9))
        add(10, "row-path-outside-reverse-scope", rg, _write_manifest(td, "m10.md", m10), 1,
            ["drift: FAIL_D", "native-doc"])

        # -- (11) TBD-RESOLVE ----------------------------------------------------
        m11 = _green_manifest(fxg).replace(
            adpt_row,
            adpt_row + "\n" +
            "| U-TBD | {} | v0.9 | TBD-RESOLVE | - | the unresolved claim |".format(
                _h_short(rg, fxg["o_abs"], 9)))
        add(11, "TBD-RESOLVE → exit 1, never exit 2 (FAIL_U)", rg, _write_manifest(td, "m11.md", m11), 1,
            ["upstreamed: FAIL_U", "U-TBD", "TBD-RESOLVE"])

        # -- (12) one-of-two-sites probe ---------------------------------------
        add(12, "one-of-two-sites probe (min_count fails)", fx1["repo"], onesite_manifest, 1,
            ["probes: FAIL_B", "delegate-scan"])

        # run the thirteen enumerated cases ------------------------------------
        for num, label, repo, manifest, expect, must, extra_checks in cases:
            code, report = run_checks(repo, manifest, FIXTURE_REGISTRY)
            problems = []
            if code == 2:
                problems.append(f"exit 2 (stub/error) — expected {expect}")
            elif code != expect:
                problems.append(f"exit {code} — expected {expect}")
            for s in must:
                if s not in report:
                    problems.append(f"report missing {s!r}")
            err = extra_checks(None)
            if err:
                problems.append(err)
            ok = not problems
            results.append(ok)
            print(f"case ({num}) {label}: {'PASS' if ok else 'FAIL'}")
            for p in problems:
                print(f"    - {p}")

        # Fixture 11 (harness integrity, unnumbered): the drop detector itself —
        # a row whose sha left the lineage must fail presence.
        poll_row = "| native-poll | {} | v1.0 | the poll liveness stamp | poll-alive |".format(
            _h_short(rg, fxg["n2"], 8))
        mdrop = _green_manifest(fxg).replace(
            poll_row,
            poll_row + "\n| ghost | deadbeef | v1.0 | the dropped patch | - |")
        code, report = run_checks(rg, _write_manifest(td, "manifest-drop.md", mdrop),
                                  FIXTURE_REGISTRY)
        drop_ok = code == 1 and "presence: FAIL_P" in report and "ghost" in report
        print(f"integrity (fixture 11) presence drop-detector: "
              f"{'PASS' if drop_ok else 'FAIL'}")
        if not drop_ok:
            print(f"    - exit {code}; report carries FAIL_P/ghost: "
                  f"{'presence: FAIL_P' in report and 'ghost' in report}")

    n_pass = sum(results)
    total = len(results)
    green = n_pass == total and pinned_ok and drop_ok
    print(f"{'GREEN' if green else 'RED'}: {n_pass}/{total} cases pass — the "
          f"thirteen enumerated + the fix-round-1 base-tag arm "
          f"(+1 integrity assertion; twelve fixtures)")
    return 0 if green else 1


_tag_cache: dict = {}


def _tag_set_fixture(repo: str, tag: str) -> set:
    """Harness-side copy of the tag patch-id set (fixture validity checks)."""
    key = (repo, tag)
    if key in _tag_cache:
        return _tag_cache[key]
    revs = _h_out(repo, "rev-list", "--no-merges", tag).split()
    ids = set()
    for sha in revs:
        pid = _h_patch_id(repo, sha)
        if pid:
            ids.add(pid)
    _tag_cache[key] = ids
    return ids


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------

def main(argv=None) -> int:
    ap = argparse.ArgumentParser(
        prog="check_patches.py",
        description="The fork patch-lineage checker (read-only; never edits anything).",
    )
    ap.add_argument("--repo", default=".", help="the git repository to check (default: cwd)")
    ap.add_argument("--manifest", default=None,
                    help="the structured manifest (default: <repo>/PATCHES.md)")
    ap.add_argument("--selftest", action="store_true",
                    help="run the built-in selftest fixtures (eleven fixtures, thirteen cases)")
    args = ap.parse_args(argv)

    if args.selftest:
        return _run_selftest()

    repo = os.path.abspath(args.repo)
    manifest = os.path.abspath(args.manifest) if args.manifest else os.path.join(repo, "PATCHES.md")
    try:
        code, report = run_checks(repo, manifest, PROBE_REGISTRY)
    except EnvError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    print(report)
    return code


if __name__ == "__main__":
    sys.exit(main())
