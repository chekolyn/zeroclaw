//! The rules builtin hook: pod-side instruction loading over a `rules/*.md`
//! corpus with YAML frontmatter, implementing VS Code-style
//! global / workspace / repo scopes and deterministic `applyTo`/`agents:`
//! attach.
//!
//! This is the fork leg of the "ZeroClaw Context Layers" design. It ships the
//! SAME corpus the opencode `rules-bind` plugin manifest-injects, as a bounded
//! manifest + core block appended to the system prompt, with rule bodies
//! attaching when an `applyTo` glob matches an in-play path or an `agents:`
//! frontmatter lists the turn's alias.
//!
//! Failure model (§4.7): the hook can never break a turn. Every path that can
//! fail fails open — a missing dir degrades to no injection, a malformed
//! frontmatter skips that file, and the runner's panic isolation catches any
//! panic. Each rung of the fail-open ladder logs once with a stable reason.

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::hooks::traits::{HookHandler, HookResult};
use zeroclaw_api::hook::{PromptBuildHookContext, ToolCallHookContext};
use zeroclaw_api::tool::ToolResult;
use zeroclaw_config::schema::RulesHookConfig;

// ── Frontmatter grammar ─────────────────────────────────────────

/// Parsed YAML frontmatter for one rules file.
///
/// `name` must equal the file stem; `description` must carry a when-trigger in
/// its first sentence; `applyTo` is a comma-separated glob list (optional);
/// `agents` is a list of aliases the rule always attaches for (optional);
/// `core` marks a rule whose lines are eager-injected (optional).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RuleFrontmatter {
    pub name: String,
    pub description: String,
    pub apply_to: Vec<String>,
    pub agents: Vec<String>,
    pub core: bool,
}

/// A discovered rule: the on-disk path, its parsed frontmatter, and the
/// resolved scope it came from.
#[derive(Debug, Clone)]
pub(crate) struct Rule {
    pub path: PathBuf,
    pub frontmatter: RuleFrontmatter,
    /// Which scope supplied this rule: `"global"`, `"workspace"`, or a repo
    /// root path for `"repo"`.
    pub scope: String,
    /// File modification time (unix seconds) for recency ordering.
    pub mtime_secs: u64,
    /// Raw body (text after the frontmatter block).
    pub body: String,
    /// Depth of the rule's path relative to its scope root, in path
    /// components. Used by deepest-wins dedup (§4.10/N4): when the same name
    /// appears twice within a scope, the deeper (more nested) file wins.
    pub depth: usize,
}

impl Rule {
    fn name(&self) -> &str {
        let fm = &self.frontmatter.name;
        if !fm.is_empty() {
            fm
        } else {
            // Fall back to the file stem when frontmatter omits `name`.
            self.path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── AGENTS.md delivery (§4.10) ──────────────────────────────────

/// The stable block-start marker for idempotent injection (I1). On build, if
/// this marker is already present in the prompt, the hook replaces everything
/// from the marker to the end rather than appending — a body delivered twice
/// within a turn, or re-sent across turns, cannot accumulate.
const RULES_HOOK_MARKER: &str = "<!-- rules-hook:v1 -->";

/// The inline `<!-- core -->` marker. The IDENTICAL marker opens AND closes
/// extraction (a toggle state machine) — ported from the opencode
/// `rules-bind` plugin's `extractCoreBlock` (spec §3.5a/§4.4). Only the lines
/// BETWEEN marker pairs are eager-injected as the core block; an ODD marker
/// count yields no extraction (never capture-to-EOF).
const CORE_MARKER: &str = "<!-- core -->";

/// The toggle-state-machine core extractor (the `rules-bind` plugin's
/// `extractCoreBlock`, ported). Each `<!-- core -->` line toggles extraction;
/// the identical marker opens AND closes, and multiples are handled (a file
/// may carry several pairs). Returns `None` on an odd marker count — the
/// caller treats the rule as contributing nothing to the core block rather
/// than capturing the trailing un-closed region to EOF (the odd-marker parity
/// guarantee, spec §4.4).
fn extract_core_block(body: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    let mut in_core = false;
    for line in body.lines() {
        if line.trim() == CORE_MARKER {
            in_core = !in_core;
            continue;
        }
        if in_core {
            out.push(line);
        }
    }
    if in_core {
        // Odd marker count: an un-closed pair — refuse to capture-to-EOF.
        return None;
    }
    Some(out.join("\n").trim().to_string())
}

/// Chars cap for a repo-root `AGENTS.md` body delivered once per session.
///
/// Canary parity: ~12,000 chars (~11.7K measured on canary) keeps the
/// once-per-session repo instructions digest small without losing the root
/// body's narrative spine. A 67K-char workspace/fork AGENTS.md is head-
/// truncated here rather than injected whole. Over-budget bodies are head-
/// truncated with an explicit note (§4.10 "bounded digest when over budget").
const AGENTS_MD_ROOT_BODY_MAX_CHARS: usize = 12_000;

/// Total char budget for the ENTIRE AGENTS.md block a turn injects — root
/// bodies + in-play attached bodies + the nested index + pointers.
///
/// The per-body cap alone does not bound the block: a workspace with several
/// repos delivers several root bodies, and (measured on prod) the nested index
/// alone was ~54K chars of `<instruction><folder>` entries for 261 discovered
/// `AGENTS.md` files, against ~13K of bodies. Without a total budget the block
/// grows with the workspace, not with what the turn needs.
const AGENTS_MD_BLOCK_BUDGET: usize = 16_000;
/// Char budget for the nested (out-of-play) `AGENTS.md` INDEX. Nested files are
/// discoverability, not law: the index tells the agent a file exists so it can
/// `read` it when it works there. Beyond this budget the listing is truncated
/// with a count, because 261 entries × ~200 chars was the dominant cost of the
/// measured 67,592-char block.
const AGENTS_MD_INDEX_BUDGET: usize = 2_000;

/// A discovered `AGENTS.md` delivery item (§4.10): VS Code's root + nested
/// `AGENTS.md` listing, with the pod's location-driven attach replacing the
/// model's judgment.
#[derive(Debug, Clone)]
pub(crate) struct AgentsMd {
    /// Absolute path to the `AGENTS.md` file.
    pub path: PathBuf,
    /// The directory containing this file — its implicit `applyTo: <dir>/**`.
    pub dir: PathBuf,
    /// Scope: `"workspace"` or `"repo:<root>"`.
    pub scope: String,
    /// Whether this is the scope's ROOT `AGENTS.md` (delivered once per
    /// session) rather than a nested one (implicit-dir attach).
    pub is_root: bool,
    /// Raw body text.
    pub body: String,
}

// ── The matcher (§4.5) ─────────────────────────────────────────

/// Whether an empty `applyTo` list means "never matches in-play" (manifest-only)
/// vs. the `**`/`**/*`/`*` always-match semantics.
fn pattern_always_matches(pattern: &str) -> bool {
    matches!(pattern.trim(), "**" | "**/*" | "*")
}

/// Partition rules into the two attach lifecycles.
///
/// Returns `(in_play_matched, agent_scoped)` where `in_play_matched` are rules
/// whose `applyTo` glob matches an in-play path, and `agent_scoped` are rules
/// whose `agents:` frontmatter lists `alias`. A rule matching BOTH is counted
/// only on the agent-scoped side (agent-scoping is the always-on contract).
/// Pure on `(rules, alias, in_play)` — no global state — so the ordering law
/// is unit-testable without the process-global registry.
fn partition_attach_rules<'a>(
    rules: &'a [Rule],
    alias: &str,
    in_play: &[String],
) -> (Vec<&'a Rule>, Vec<&'a Rule>) {
    let mut agent_scoped: Vec<&Rule> = Vec::new();
    let mut in_play_matched: Vec<&Rule> = Vec::new();
    for rule in rules {
        let matches_agent = rule
            .frontmatter
            .agents
            .iter()
            .any(|a| a.eq_ignore_ascii_case(alias));
        if matches_agent {
            agent_scoped.push(rule);
            continue;
        }
        let matches_in_play = if rule.frontmatter.apply_to.is_empty() {
            false
        } else {
            in_play.iter().any(|path| {
                apply_to_matches(&rule.frontmatter.apply_to.join(", "), path)
            })
        };
        if matches_in_play {
            in_play_matched.push(rule);
        }
    }
    (in_play_matched, agent_scoped)
}

/// Match a single comma-separated `applyTo` entry against a path.
///
/// VS Code `_matches` semantics: comma-separated patterns; bare relative
/// patterns are prefixed `**/`; matching is case-insensitive; `**`, `**/*`
/// and `*` always match. A pattern is `!`-negated to exclude.
pub(crate) fn apply_to_matches(apply_to: &str, path: &str) -> bool {
    let mut matched = false;
    let mut saw_positive = false;
    for raw in apply_to.split(',') {
        let pattern = raw.trim();
        if pattern.is_empty() {
            continue;
        }
        let (negate, pattern) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest.trim()),
            None => (false, pattern),
        };
        let hits = pattern_always_matches(pattern)
            || glob_matches_pattern(pattern, path);
        if negate {
            if hits {
                matched = false;
            }
        } else {
            saw_positive = true;
            matched |= hits;
        }
    }
    if !saw_positive {
        false
    } else {
        matched
    }
}

/// Glob-match a single (non-negated) pattern against a path, applying the
/// `**/` bare-pattern prefix and case-insensitivity.
fn glob_matches_pattern(pattern: &str, path: &str) -> bool {
    // Bare relative patterns are prefixed `**/` (VS Code semantics).
    let candidate = if pattern.starts_with('/') || pattern.contains("**/") || pattern.starts_with("**") {
        pattern.to_string()
    } else {
        format!("**/{pattern}")
    };
    let haystack = path.replace('\\', "/");
    let haystack_lc = haystack.to_lowercase();
    let candidate_lc = candidate.to_lowercase();
    // Translate the prefix-stripped candidate into a glob that also matches
    // the bare basename for multi-segment patterns.
    match glob::Pattern::new(&candidate_lc) {
        Ok(pat) => pat.matches(&haystack_lc) || pat.matches_path(Path::new(&haystack)),
        Err(_) => false,
    }
}

// ── Frontmatter parse + body split ──────────────────────────────

/// Split a raw file into `(frontmatter_yaml, body)`.
///
/// Frontmatter is delimited by a leading `---` line and a closing `---` line
/// (the `...` terminator is also accepted). A file without a leading `---`
/// has no frontmatter.
fn split_frontmatter(raw: &str) -> Option<(String, String)> {
    let mut lines = raw.lines();
    let first = lines.next()?;
    if first.trim() != "---" {
        return None;
    }
    let mut fm_lines: Vec<&str> = Vec::new();
    let mut body_start = first.len() + 1; // skip "---\n"
    for line in lines {
        if line.trim() == "---" || line.trim() == "..." {
            // Body starts after the closing delimiter line.
            let body = if let Some(rest) = raw[body_start..].find('\n') {
                raw[body_start + rest + 1..].to_string()
            } else {
                String::new()
            };
            return Some((fm_lines.join("\n"), body.trim_start_matches('\n').to_string()));
        }
        fm_lines.push(line);
        body_start += line.len() + 1;
    }
    None
}

/// Parse `(name, description, applyTo, agents, core)` from frontmatter YAML.
///
/// Absent keys default; `applyTo` accepts either a scalar (comma-separated) or
/// a YAML list; `agents` accepts a scalar or a list.
fn parse_frontmatter(yaml: &str) -> Result<RuleFrontmatter, String> {
    #[derive(serde::Deserialize, Default)]
    #[serde(untagged)]
    enum ListOrScalar {
        #[default]
        Empty,
        List(Vec<String>),
        Scalar(String),
    }
    impl ListOrScalar {
        fn into_vec(self) -> Vec<String> {
            match self {
                ListOrScalar::List(v) => v,
                ListOrScalar::Scalar(s) => s
                    .split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect(),
                ListOrScalar::Empty => Vec::new(),
            }
        }
    }

    #[derive(serde::Deserialize, Default)]
    struct RawFm {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        #[serde(rename = "applyTo")]
        apply_to: Option<ListOrScalar>,
        #[serde(default)]
        agents: Option<ListOrScalar>,
        #[serde(default)]
        core: Option<bool>,
    }

    let raw: RawFm = yaml_serde::from_str(yaml).map_err(|e| format!("yaml: {e}"))?;
    Ok(RuleFrontmatter {
        name: raw.name.unwrap_or_default(),
        description: raw.description.unwrap_or_default(),
        apply_to: raw.apply_to.map(ListOrScalar::into_vec).unwrap_or_default(),
        agents: raw.agents.map(ListOrScalar::into_vec).unwrap_or_default(),
        core: raw.core.unwrap_or(false),
    })
}

// ── Discovery ───────────────────────────────────────────────────

/// Recursively scan a directory for `*.md` rules files, skipping `AGENTS.md`
/// (which is engine bootstrap territory, not the rules corpus).
///
/// Symlink-safety (N6): a symlinked directory is NOT followed. Following a
/// self-referential symlink recurses forever, and following a symlink back up
/// the tree re-scans already-seen files. We track canonicalised directory
/// paths in `seen` and skip any directory whose canonical path was already
/// visited; a symlink resolves to its target's canonical path, so a loop or a
/// back-dip lands on an already-seen path and is skipped.
fn scan_rules_dir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        if let Ok(canon) = std::fs::canonicalize(&current)
            && !seen.insert(canon)
        {
            // Already visited (directly or via a symlink) — skip.
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if stem.eq_ignore_ascii_case("AGENTS") || stem.eq_ignore_ascii_case("CLAUDE") || stem.eq_ignore_ascii_case("copilot-instructions") {
                    continue;
                }
                out.push(path);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Recursively scan a directory for `AGENTS.md` files (§4.10).
///
/// Returns `(path, is_root)` pairs: the directory's own `AGENTS.md` is the
/// root (`is_root = true`); any `AGENTS.md` in a subdirectory is nested
/// (`is_root = false`). Unlike the rules corpus, AGENTS.md files are NOT
/// skipped here — the exclusion of the agent-workspace ROOT `AGENTS.md`
/// (engine bootstrap) happens at the call sites that know which of these is
/// the workspace floor.
fn scan_agents_md(dir: &Path) -> Vec<(PathBuf, bool)> {
    scan_agents_md_with(dir, &DiscoveryFilter::default())
}

/// A directory-prune + ignore filter for a bounded discovery walk. Applied by
/// [`scan_agents_md_with`] and the repo-root finder so both honour the same
/// law: `.git/` internals and caches that can never hold a discovery marker
/// (`.agents/rules`, `AGENTS.md`) are pruned, and a gitignore-style exclusion
/// hides the rest where practical.
#[derive(Debug, Clone, Default)]
struct DiscoveryFilter {
    /// A gitignore matcher governing this walk (the root repo's own ignore
    /// rules), if the caller supplied one.
    ignores: Option<GitIgnore>,
}

/// Minimal gitignore matcher: a list of literal directory-prefix patterns and
/// file patterns, evaluated against a path RELATIVE to the walk root.
///
/// This is deliberately a small subset (glob prefix + suffix `*` matching, `!`
/// negation dropped) rather than a full spec implementation — its only job is
/// to keep the discovery walk from descending into heavy dirs (`target/`,
/// `node_modules/`, `build/`) that a repo has already told git to ignore, and
/// failing to match is always safe (an unmatched path is scanned, which is the
/// current status quo, not a regression).
#[derive(Debug, Clone, Default)]
pub(crate) struct GitIgnore {
    /// Directory prefixes to prune (normalised, trailing `/` dropped).
    dirs: Vec<String>,
    /// Bare file-name matches (exact stem) to skip.
    files: Vec<String>,
}

impl GitIgnore {
    /// Parse `.gitignore` / `.git/info/exclude` lines into a matcher. Only
    /// non-negated, non-comment lines are kept; anchored `/`-prefixed and
    /// `**`-patterns are treated as literal dir/file prefixes (best effort).
    fn parse(spec: &str) -> Self {
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for raw in spec.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                continue;
            }
            let pat = line.trim_start_matches('/');
            if pat.is_empty() {
                continue;
            }
            // A trailing `/**` means "this dir and everything under it".
            let mut p = pat;
            if let Some(stripped) = p.strip_suffix("/**") {
                dirs.push(stripped.trim_end_matches('/').to_string());
                continue;
            }
            p = p.trim_end_matches('/');
            if p.contains('/') {
                dirs.push(p.to_string());
            } else {
                files.push(p.replace('*', "").to_string());
            }
        }
        GitIgnore { dirs, files }
    }

    /// Whether `rel` (a path relative to the walk root) should be pruned.
    fn prunes(&self, rel: &Path) -> bool {
        let s = rel.to_string_lossy().replace('\\', "/");
        for d in &self.dirs {
            if s == *d || s.starts_with(&format!("{d}/")) {
                return true;
            }
        }
        if let Some(name) = rel.file_name().and_then(|n| n.to_str()) {
            for f in &self.files {
                if name == *f {
                    return true;
                }
            }
        }
        false
    }
}

/// Load the combined ignore spec for a repo root (`.gitignore` +
/// `.git/info/exclude`), or `None` when no spec is readable.
fn load_gitignore(root: &Path) -> Option<GitIgnore> {
    let mut spec = String::new();
    let gi = root.join(".gitignore");
    if let Ok(s) = std::fs::read_to_string(&gi) {
        spec.push_str(&s);
        spec.push('\n');
    }
    let ex = root.join(".git").join("info").join("exclude");
    if let Ok(s) = std::fs::read_to_string(&ex) {
        spec.push_str(&s);
        spec.push('\n');
    }
    if spec.trim().is_empty() {
        None
    } else {
        Some(GitIgnore::parse(&spec))
    }
}

/// Directory names that can NEVER hold a discovery marker (`AGENTS.md` or
/// `.agents/rules`) and are pruned unconditionally on every walk. These are
/// the heavy, infinite-regrowth dirs (`.git` internals, Rust/uv caches) that
/// dominate the ~957K-file workspace. Deliberately NOT a name blocklist for
/// checkout dirs: `.worktrees/` and `skill-lab.gen/` are legitimately scanned
/// — a worktree can be the working repo.
fn is_pruned_dir(name: &str) -> bool {
    matches!(name, ".git" | ".cargo" | ".cache")
}

/// A bounded, prune + ignore-aware AGENTS.md walk. Prunes `.git`/`.cargo`/
/// `.cache` dirs always, honours the root's gitignore, and never follows a
/// symlinked directory (N6).
fn scan_agents_md_with(dir: &Path, filter: &DiscoveryFilter) -> Vec<(PathBuf, bool)> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        if let Ok(canon) = std::fs::canonicalize(&current)
            && !seen.insert(canon)
        {
            continue; // already visited (directly or via a symlink) — skip
        }
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Prune by NAME first (cheap, no ignore eval): `.git`, `.cargo`,
            // `.cache`. Symlinked dirs are only recursed when not pruned.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                if is_pruned_dir(name) {
                    continue;
                }
                if let Some(ig) = &filter.ignores {
                    // rel path = path relative to `dir`, eval'd as the walk root.
                    if let Ok(rel) = path.strip_prefix(dir)
                        && ig.prunes(rel)
                    {
                        continue;
                    }
                }
                // Do not follow symlinked dirs (N6): `path.is_dir()` is true for
                // a symlink-to-dir, so stat to detect + skip symlinks.
                if path.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                    continue;
                }
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some("AGENTS.md") {
                let is_root = path.parent() == Some(dir);
                out.push((path, is_root));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Resolve a repo scope root for a working directory.
///
/// Order: `git rev-parse --show-toplevel`, then the nearest ancestor
/// containing `.agents/rules`; if neither, no repo scope.
fn resolve_repo_root(working_dir: &Path) -> Option<PathBuf> {
    // git rev-parse --show-toplevel (best-effort; a non-git dir returns None).
    if let Ok(output) = std::process::Command::new("git")
        .arg("-C")
        .arg(working_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        && output.status.success()
    {
        let top = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !top.is_empty() {
            return Some(PathBuf::from(top));
        }
    }
    // Nearest ancestor containing `.agents/rules`.
    for ancestor in working_dir.ancestors() {
        if ancestor.join(".agents").join("rules").is_dir() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

// ── Bounded repo-root discovery + manifest cache ─────────────────
//
// The discovery hot path: the per-turn full-tree walk (scan_agents_md over
// the whole workspace + every resolved repo root — ~957K files, 42 .git dirs)
// is replaced by (a) a BOUNDED root finder that locates candidate repos (dirs
// with `.git` or `.agents/rules`) at limited depth, pruning `.git`/`.cargo`/
// `.cache` and honouring gitignore along the way, and (b) an MTIME-CACHED
// manifest keyed per floor. A turn that changes nothing does NO traversal —
// it only stats the small set of marker paths recorded at first discovery.

/// Maximum depth (below the floor) the root finder descends. This bounds the
/// candidate scan: repos nested deeper than this are not discovered by the
/// walker, but are still resolved for in-play paths via
/// [`resolve_repo_root`] (git rev-parse / ancestor `.agents/rules`).
const MAX_ROOT_SCAN_DEPTH: usize = 16;

/// The discovery markers a walk records: a `.git` entry (file or dir), a
/// `.agents/rules` dir, or an `AGENTS.md` file. These are the ONLY paths whose
/// mtime invalidates the cached manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MarkerKind {
    Git,
    AgentsRulesDir,
    AgentsMd,
}

/// A snapshot of one floor's discovery: the marker paths (and their mtimes)
/// plus the discovered repo roots and AGENTS.md files, all built from one
/// bounded walk. On a later turn we stat ONLY the markers; if none changed,
/// the snapshot is reused without a walk.
#[derive(Debug, Clone, Default)]
pub(crate) struct DiscoveryEntry {
    /// The canonical floor this entry describes.
    floor: PathBuf,
    /// Floor mtime (unix secs) at discovery time.
    floor_mtime: u64,
    /// `(path, kind, mtime_secs)` for every marker discovered.
    markers: Vec<(PathBuf, MarkerKind, u64)>,
    /// The discovered repo roots (canonical).
    roots: Vec<PathBuf>,
    /// Every discovered AGENTS.md path (absolute), from the same bounded walk.
    agents_md: Vec<PathBuf>,
    /// When the walk that produced this entry ran. Drives [`DISCOVERY_CACHE_TTL`]
    /// — the backstop for markers that APPEAR without touching a recorded
    /// marker (a new `AGENTS.md`/`.agents/rules`/repo in an existing dir).
    walked_at: Option<std::time::Instant>,
}

/// How long a cached floor is trusted for STRUCTURAL changes (a marker that
/// appeared or disappeared) before it is re-walked regardless of mtimes.
///
/// Existing markers are validated on every turn by mtime, so an edited rule or
/// `AGENTS.md` is picked up immediately. What mtimes cannot see is a marker
/// being ADDED in a directory we already walked past (the parent's mtime does
/// move for a direct child, but not for a grandchild), and a newly cloned repo.
/// Rather than stat every traversed directory (141K of them on the prod
/// workspace — far worse than the walk it would replace), the structural check
/// is bounded by this TTL: five minutes of staleness in exchange for zero
/// traversal on every other turn.
const DISCOVERY_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// The process-global discovery cache, keyed by canonical floor path. Bounded
/// by [`DISCOVERY_CACHE_CAPACITY`] with whole-table flush on overflow (floors
/// are few; a flush is cheap relative to the walk it saves).
static DISCOVERY_CACHE: Mutex<Option<HashMap<PathBuf, DiscoveryEntry>>> = Mutex::new(None);

/// Diagnostic counters for the discovery hot path: how many times a turn did a
/// stat-only cache hit (no walk) vs a re-walk, and the cumulative walk cost.
/// These let the `rules block injected` log line carry the before/after timing
/// evidence the operator asked for (first walk, stat-only turn, changed turn).
#[derive(Debug, Default)]
pub(crate) struct DiscoveryStats {
    /// Stat-only cache hits (no traversal).
    pub stat_hits: u64,
    /// Full walks performed (first turn + any invalidated turn).
    pub walks: u64,
    /// Cumulative millis spent walking.
    pub walk_millis: u128,
    /// Millis spent in the last stat-only revalidation (the cheap path).
    pub last_stat_millis: u128,
}

static DISCOVERY_STATS: Mutex<DiscoveryStats> = Mutex::new(DiscoveryStats::EMPTY);

impl DiscoveryStats {
    const EMPTY: DiscoveryStats = DiscoveryStats {
        stat_hits: 0,
        walks: 0,
        walk_millis: 0,
        last_stat_millis: 0,
    };

    fn snapshot() -> DiscoveryStats {
        let g = DISCOVERY_STATS.lock().unwrap_or_else(|p| p.into_inner());
        DiscoveryStats {
            stat_hits: g.stat_hits,
            walks: g.walks,
            walk_millis: g.walk_millis,
            last_stat_millis: g.last_stat_millis,
        }
    }
}

const DISCOVERY_CACHE_CAPACITY: usize = 64;

fn discovery_cache() -> &'static Mutex<Option<HashMap<PathBuf, DiscoveryEntry>>> {
    &DISCOVERY_CACHE
}

fn mtime_secs(p: &Path) -> Option<u64> {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// A single (bounded) walk of one floor: find every repo root (dir containing
/// `.git` or `.agents/rules`) up to `max_depth`, and every marker path
/// (`AGENTS.md`, `.git`, `.agents/rules`) for the cache. Returns `(roots,
/// markers, agents_md_paths)`.
fn walk_floor(
    floor: &Path,
    max_depth: usize,
) -> (Vec<PathBuf>, Vec<(PathBuf, MarkerKind)>, Vec<PathBuf>) {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut markers: Vec<(PathBuf, MarkerKind)> = Vec::new();
    let mut agents_md: Vec<PathBuf> = Vec::new();
    if !floor.is_dir() {
        return (roots, markers, agents_md);
    }

    // The floor itself is a candidate root if it holds a marker directly.
    let floor_is_root = floor.join(".git").exists()
        || floor.join(".agents").join("rules").is_dir();
    if floor_is_root {
        roots.push(canon(floor));
    }
    if floor.join(".git").exists() {
        markers.push((floor.join(".git"), MarkerKind::Git));
    }
    let agents_rules = floor.join(".agents").join("rules");
    if agents_rules.is_dir() {
        markers.push((agents_rules, MarkerKind::AgentsRulesDir));
    }
    if floor.join("AGENTS.md").is_file() {
        markers.push((floor.join("AGENTS.md"), MarkerKind::AgentsMd));
        agents_md.push(floor.join("AGENTS.md"));
    }

    // The floor's ignore spec (.gitignore + .git/info/exclude).
    let filter = DiscoveryFilter {
        ignores: load_gitignore(floor),
    };

    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(floor.to_path_buf(), 0)];
    while let Some((current, depth)) = stack.pop() {
        if depth >= max_depth {
            continue;
        }
        if let Ok(c) = std::fs::canonicalize(&current)
            && !seen.insert(c)
        {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };

            if name == "AGENTS.md" && ft.is_file() {
                markers.push((path.clone(), MarkerKind::AgentsMd));
                agents_md.push(path.clone());
                continue;
            }
            if !ft.is_dir() {
                if name == ".git" {
                    markers.push((path.clone(), MarkerKind::Git));
                }
                continue;
            }

            // Prune always-pruned dirs, but record `.git` as a marker first.
            if is_pruned_dir(name) {
                if name == ".git" {
                    markers.push((path.clone(), MarkerKind::Git));
                }
                continue;
            }
            // Honour gitignore (evaluated relative to the floor).
            if let Some(ig) = &filter.ignores
                && let Ok(rel) = path.strip_prefix(floor)
                && ig.prunes(rel)
            {
                continue;
            }
            // Do not follow symlinked dirs (N6).
            if ft.is_symlink() {
                continue;
            }
            let child_agents_rules = path.join(".agents").join("rules");
            if path.join(".git").exists() || child_agents_rules.is_dir() {
                roots.push(canon(&path));
            }
            if child_agents_rules.is_dir() {
                markers.push((child_agents_rules, MarkerKind::AgentsRulesDir));
            }
            stack.push((path, depth + 1));
        }
    }
    roots.sort();
    roots.dedup();
    markers.sort();
    markers.dedup();
    agents_md.sort();
    agents_md.dedup();

    (roots, markers, agents_md)
}

/// The cached discovery read: return the floor's repo roots and AGENTS.md
/// paths with a stat-only revalidation. Returns `(roots, agents_md_paths,
/// was_walked)` — `was_walked == false` means a pure stat-only cache hit (no
/// traversal). Diagnostics get the walk duration from the cache entry directly.
fn discover_floor_cached(floor: &Path) -> (Vec<PathBuf>, Vec<PathBuf>, bool) {
    discover_floor_cached_with_ttl(floor, DISCOVERY_CACHE_TTL)
}

/// [`discover_floor_cached`] with an explicit structural TTL (tests drive this
/// with `Duration::ZERO` to force the structural revalidation without sleeping).
fn discover_floor_cached_with_ttl(
    floor: &Path,
    ttl: std::time::Duration,
) -> (Vec<PathBuf>, Vec<PathBuf>, bool) {
    let key = canon(floor);
    let mut guard = discovery_cache().lock().unwrap_or_else(|p| p.into_inner());
    let cache = guard.get_or_insert_with(HashMap::new);

    if let Some(e) = cache.get(&key) {
        let stat_start = std::time::Instant::now();
        // Structural freshness: the TTL backstop for markers that APPEARED
        // (mtimes of recorded markers cannot see a new sibling/deep marker).
        let structure_ok = e
            .walked_at
            .map(|t| stat_start.duration_since(t) < ttl)
            .unwrap_or(false);
        let root_ok = mtime_secs(&e.floor).map(|m| m == e.floor_mtime).unwrap_or(false);
        let all_ok = structure_ok
            && root_ok
            && e.markers
                .iter()
                .all(|(p, _, m)| mtime_secs(p).map(|cur| cur == *m).unwrap_or(false));
        if all_ok {
            // Stat-only cache hit: NO traversal.
            let stat_millis = stat_start.elapsed().as_millis();
            {
                let mut s = DISCOVERY_STATS.lock().unwrap_or_else(|p| p.into_inner());
                s.stat_hits += 1;
                s.last_stat_millis = stat_millis;
            }
            return (e.roots.clone(), e.agents_md.clone(), false);
        }
    }

    // Miss or invalidated: re-walk.
    let start = std::time::Instant::now();
    let (roots, markers, agents_md) = walk_floor(floor, MAX_ROOT_SCAN_DEPTH);
    let walk_millis = start.elapsed().as_millis();
    {
        let mut s = DISCOVERY_STATS.lock().unwrap_or_else(|p| p.into_inner());
        s.walks += 1;
        s.walk_millis += walk_millis;
    }
    let marker_entries: Vec<(PathBuf, MarkerKind, u64)> = markers
        .into_iter()
        .map(|(p, k)| {
            let m = mtime_secs(&p).unwrap_or(0);
            (p, k, m)
        })
        .collect();
    cache.insert(
        key.clone(),
        DiscoveryEntry {
            floor: key.clone(),
            floor_mtime: mtime_secs(&key).unwrap_or(0),
            markers: marker_entries,
            roots: roots.clone(),
            agents_md: agents_md.clone(),
            walked_at: Some(start),
        },
    );
    if cache.len() > DISCOVERY_CACHE_CAPACITY {
        cache.clear();
    }
    (roots, agents_md, true)
}

/// Warm the discovery cache for one workspace floor.
///
/// Called from daemon startup (in a background thread, once per configured
/// agent workspace) so the FIRST turn of a fresh pod finds a populated cache
/// instead of paying the bounded walk. Idempotent and best-effort: a floor
/// that vanished between config load and the warm is simply skipped, and a
/// failing walk leaves the cache empty for the turn to fill.
pub fn warm_discovery(floor: &Path) {
    if !floor.is_dir() {
        return;
    }
    let start = std::time::Instant::now();
    let (roots, agents, walked) = discover_floor_cached(floor);
    if walked {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Load).with_attrs(
                ::serde_json::json!({
                    "hook": "rules",
                    "event": "discovery-warm",
                    "floor": floor.display().to_string(),
                    "roots": roots.len(),
                    "agents_md": agents.len(),
                    "warm_millis": start.elapsed().as_millis(),
                })
            ),
            "rules discovery cache warmed at startup"
        );
    }
}

// ── The in-play registry (process-global, LRU/TTL) ──────────────

const SESSION_REGISTRY_CAPACITY: usize = 256;
const SESSION_TTL_SECS: u64 = 24 * 60 * 60; // 24h

/// A session's in-play set: ordered, deduped, capped at `in_play_paths`.
#[derive(Debug, Clone, Default)]
struct SessionInPlay {
    paths: Vec<String>,
    last_seen_secs: u64,
}

/// Process-global registry keyed `(alias, session_id) → SessionInPlay`, LRU/
/// TTL evicted. Shared across the three `HookRunner::from_root_config` sites
/// so tool events populate the in-play set the prompt build reads regardless
/// of which runner instance dispatches them.
///
/// `delivered_roots` carries the once-per-session repo-root `AGENTS.md`
/// delivery set (§4.10), keyed by the SAME `(alias, session_id)` as the
/// in-play set — the spec's single session registry. Each value holds the
/// canonicalised repo-root `AGENTS.md` paths already delivered this session.
struct Registry {
    sessions: HashMap<(String, String), SessionInPlay>,
    delivered_roots: HashMap<(String, String), HashSet<String>>,
    order: Vec<(String, String)>,
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

fn registry_inner() -> &'static Mutex<Option<Registry>> {
    &REGISTRY
}

fn with_registry<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    let mut guard = registry_inner().lock().unwrap_or_else(|p| p.into_inner());
    let reg = guard.get_or_insert_with(|| Registry {
        sessions: HashMap::new(),
        delivered_roots: HashMap::new(),
        order: Vec::new(),
    });
    f(reg)
}

impl Registry {
    fn record_in_play(&mut self, alias: &str, session_id: &str, path: &str, cap: usize, now: u64) {
        let key = (alias.to_string(), session_id.to_string());
        let entry = self
            .sessions
            .entry(key.clone())
            .or_default();
        entry.last_seen_secs = now;
        if !entry.paths.iter().any(|p| p == path) {
            entry.paths.push(path.to_string());
        }
        if entry.paths.len() > cap {
            let overflow = entry.paths.len() - cap;
            entry.paths.drain(0..overflow);
        }
        self.touch(&key);
    }

    fn in_play(&mut self, alias: &str, session_id: &str, now: u64) -> Vec<String> {
        let key = (alias.to_string(), session_id.to_string());
        // TTL eviction.
        self.sessions.retain(|_, entry| {
            now.saturating_sub(entry.last_seen_secs) <= SESSION_TTL_SECS
        });
        self.order.retain(|k| self.sessions.contains_key(k));
        let found = match self.sessions.get_mut(&key) {
            Some(entry) => {
                entry.last_seen_secs = now;
                Some(entry.paths.clone())
            }
            None => None,
        };
        if found.is_some() {
            self.touch(&key);
        }
        found.unwrap_or_default()
    }

    fn touch(&mut self, key: &(String, String)) {
        self.order.retain(|k| k != key);
        self.order.push(key.clone());
        while self.order.len() > SESSION_REGISTRY_CAPACITY {
            if let Some(evicted) = self.order.first().cloned() {
                self.sessions.remove(&evicted);
                self.delivered_roots.remove(&evicted);
                self.order.remove(0);
            } else {
                break;
            }
        }
    }

    /// Record that a repo-root `AGENTS.md` path was delivered this session.
    fn record_delivered_root(&mut self, alias: &str, session_id: &str, path: &str) {
        let key = (alias.to_string(), session_id.to_string());
        self.delivered_roots
            .entry(key.clone())
            .or_default()
            .insert(path.to_string());
        self.touch(&key);
    }

    /// Whether a repo-root `AGENTS.md` path was already delivered this session.
    fn root_delivered(&mut self, alias: &str, session_id: &str, path: &str) -> bool {
        let key = (alias.to_string(), session_id.to_string());
        self.delivered_roots
            .get(&key)
            .map(|set| set.contains(path))
            .unwrap_or(false)
    }
}

/// Record an attributed path into the in-play set (attach trigger only).
fn record_in_play_path(alias: &str, session_id: &str, path: &str, cap: usize) {
    let now = now_secs();
    with_registry(|reg| reg.record_in_play(alias, session_id, path, cap, now));
}

fn read_in_play_paths(alias: &str, session_id: &str) -> Vec<String> {
    let now = now_secs();
    with_registry(|reg| reg.in_play(alias, session_id, now))
}

/// Mark a repo-root `AGENTS.md` as delivered this session (§4.10 once-per-
/// session delivery, tracked in the same registry key as the in-play set).
fn record_root_agents_delivered(alias: &str, session_id: &str, path: &str) {
    with_registry(|reg| reg.record_delivered_root(alias, session_id, path));
}

fn root_agents_delivered(alias: &str, session_id: &str, path: &str) -> bool {
    with_registry(|reg| reg.root_delivered(alias, session_id, path))
}

// ── Tool-arg path extraction (best-effort) ──────────────────────

const WRITE_TOOLS: &[&str] = &[
    "file_write",
    "file_edit",
    "edit",
    "apply_patch",
    "write",
    "create_file",
    "file_replace",
];
const READ_TOOLS: &[&str] = &["file_read", "read", "read_file", "glob_search"];

fn is_write_tool(tool: &str) -> bool {
    WRITE_TOOLS.iter().any(|t| t.eq_ignore_ascii_case(tool))
}

fn is_read_tool(tool: &str) -> bool {
    READ_TOOLS.iter().any(|t| t.eq_ignore_ascii_case(tool))
}

/// Extract the best candidate path from a tool call's arguments.
///
/// Write/edit tools: the `path` / `file_path` / `file` argument is
/// authoritative (the attach trigger). Read tools: recorded but treated as
/// resolution-only by the caller. `shell`/`bash`: best-effort workspace-
/// relative path tokens (`/path` or `./relative` or `.md`/`.rs`/… suffixes).
fn extract_path(tool: &str, args: &serde_json::Value) -> Option<String> {
    if is_write_tool(tool) || is_read_tool(tool) {
        if let Some(obj) = args.as_object() {
            for key in ["path", "file_path", "file", "filename", "target"] {
                if let Some(v) = obj.get(key).and_then(|v| v.as_str())
                    && !v.trim().is_empty()
                {
                    return Some(v.to_string());
                }
            }
        }
        return None;
    }
    if tool.eq_ignore_ascii_case("shell") || tool.eq_ignore_ascii_case("bash") {
        // Argument keys differ by tool: `command` or `cmd`.
        let obj = match args {
            serde_json::Value::String(s) => return extract_path_from_cmd(s),
            serde_json::Value::Object(obj) => obj,
            _ => return None,
        };
        let cmd = obj
            .get("command")
            .or_else(|| obj.get("cmd"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return extract_path_from_cmd(cmd);
    }
    None
}

/// Extract a plausible path token from a shell command string.
fn extract_path_from_cmd(cmd: &str) -> Option<String> {
    for token in cmd.split_whitespace() {
        let t = token.trim_matches(|c| c == '"' || c == '\'' || c == '`');
        if t.is_empty() {
            continue;
        }
        let looks_like_path = t.starts_with('/')
            || t.starts_with("./")
            || t.starts_with("../")
            || [".md", ".rs", ".toml", ".yml", ".yaml", ".json", ".py", ".ts", ".go", ".sh"]
                .iter()
                .any(|ext| t.contains(ext));
        if looks_like_path {
            return Some(t.to_string());
        }
    }
    None
}

// ── The hook ────────────────────────────────────────────────────

/// The rules builtin hook handler.
///
/// Stateless on the instance by design: the three `HookRunner::from_root_config`
/// construction sites each build one, and all shared state lives in the
/// process-global [`registry`] keyed `(alias, session_id)`.
pub struct RulesHook {
    config: RulesHookConfig,
}

impl RulesHook {
    pub fn new(config: RulesHookConfig) -> Self {
        Self { config }
    }

    /// Register the turn → (alias, session) correlation so later tool events
    /// (keyed only by `turn_id:iteration:call_index`) attribute to this
    /// session's in-play set.
    fn record_turn(&self, ctx: &PromptBuildHookContext) {
        record_turn_session(ctx.turn_id(), ctx.alias(), ctx.session_id());
    }

    /// Discover rules across the active scopes and return them ordered
    /// scopes-first (global → workspace → repo), deduped by `name` later.
    fn discover(&self, ctx: &PromptBuildHookContext) -> Vec<Rule> {
        let mut rules: Vec<Rule> = Vec::new();

        // Global scope (absolute path).
        if !self.config.global_dir.is_empty() {
            let dir = PathBuf::from(&self.config.global_dir);
            if dir.is_dir() {
                for p in scan_rules_dir(&dir) {
                    if let Some(rule) = self.load_rule(&p, "global".to_string(), &dir) {
                        rules.push(rule);
                    }
                }
            } else {
                // Whole dir missing: degrade to no global scope (logged once).
                log_once(&format!("rules-global-dir-missing:{}", self.config.global_dir), "rules hook: global rules dir missing; skipping global scope");
            }
        }

        // Workspace scope (workspace-relative).
        let ws_dir = ctx
            .workspace()
            .join(&self.config.workspace_dir);
        if ws_dir.is_dir() {
            for p in scan_rules_dir(&ws_dir) {
                if let Some(rule) = self.load_rule(&p, "workspace".to_string(), &ws_dir) {
                    rules.push(rule);
                }
            }
        }

        // Repo scope (dynamic, from the working dir of in-play paths first,
        // then the session workspace).
        if self.config.repo_discovery {
            let repo_roots = self.resolve_repo_roots(ctx);
            for root in repo_roots {
                let rules_root = root.join(".agents").join("rules");
                if rules_root.is_dir() {
                    for p in scan_rules_dir(&rules_root) {
                        if let Some(rule) =
                            self.load_rule(&p, format!("repo:{}", root.display()), &rules_root)
                        {
                            rules.push(rule);
                        }
                    }
                }
            }
        }

        rules
    }

    fn resolve_repo_roots(&self, ctx: &PromptBuildHookContext) -> Vec<PathBuf> {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut roots: Vec<PathBuf> = Vec::new();

        // 1. Session workspace / working directory first (the turn-1 corpus).
        let ws = ctx.workspace();
        // Fast path: the bounded, mtime-cached walker finds every candidate
        // repo root below the workspace floor WITHOUT a full tree walk, and
        // stats-only on unchanged turns.
        let (walked_roots, _agents, _walked) = discover_floor_cached(ws);
        for root in walked_roots {
            if seen.insert(root.clone()) {
                roots.push(root);
            }
        }
        // Fallback: git rev-parse for the workspace itself (covers a floor
        // that IS a repo but whose `.git`/`.agents/rules` marker is unusual,
        // and the ancestor `.agents/rules` case).
        if let Some(root) = resolve_repo_root(ws)
            && seen.insert(root.clone())
        {
            roots.push(root);
        }

        // 2. Refine by in-play paths that point into another repo.
        let in_play = read_in_play_paths(ctx.alias(), ctx.session_id());
        for path in in_play {
            let p = Path::new(&path);
            // In-play paths come from tool args and are usually WORKSPACE-
            // RELATIVE (e.g. `demo-repo/HELLO.md`); the daemon's CWD is not the
            // agent workspace, so a bare relative path resolves against the
            // wrong root (or fails) and the nested repo is never found. Anchor
            // relative paths to the session workspace before probing.
            let anchored = if p.is_absolute() {
                p.to_path_buf()
            } else {
                ctx.workspace().join(p)
            };
            let dir = if anchored.is_file() {
                anchored.parent().map(Path::to_path_buf).unwrap_or_default()
            } else {
                anchored
            };
            if let Some(root) = resolve_repo_root(&dir)
                && seen.insert(root.clone())
                && roots.len() < self.config.repo_roots_max as usize
            {
                roots.push(root);
            }
        }

        // 3. Most-recently-modified first; overflow logged.
        roots.sort_by_key(|r| {
            let rules_dir = r.join(".agents").join("rules");
            let mtime = std::fs::metadata(&rules_dir)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            std::cmp::Reverse(mtime)
        });
        if roots.len() > self.config.repo_roots_max as usize {
            log_once(
                "rules-repo-roots-overflow",
                "rules hook: repo roots exceed repo_roots_max; truncating",
            );
            roots.truncate(self.config.repo_roots_max as usize);
        }
        roots
    }

    fn load_rule(&self, path: &Path, scope: String, scope_root: &Path) -> Option<Rule> {
        // Cache is keyed by path+mtime via the frontmatter parse below; we
        // re-read on mtime change. A `cached` map is kept process-global.
        let Ok(raw) = std::fs::read_to_string(path) else {
            // Unreadable file: skip, log once per condition.
            log_once(
                &format!("rules-unreadable:{}", path.display()),
                "rules hook: unreadable rules file skipped",
            );
            return None;
        };
        let (fm_yaml, body) = match split_frontmatter(&raw) {
            Some((fm, body)) => (fm.to_string(), body.to_string()),
            None => {
                // No frontmatter: skip (the corpus requires name/description).
                log_once(
                    &format!("rules-no-frontmatter:{}", path.display()),
                    "rules hook: rules file without frontmatter skipped",
                );
                return None;
            }
        };
        let frontmatter = match parse_frontmatter(&fm_yaml) {
            Ok(fm) => fm,
            Err(e) => {
                log_once(
                    &format!("rules-bad-frontmatter:{}", path.display()),
                    &format!("rules hook: malformed frontmatter skipped: {e}"),
                );
                return None;
            }
        };
        let mtime_secs = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Scope-relative depth (path components) drives deepest-wins dedup.
        let depth = path
            .strip_prefix(scope_root)
            .map(|rel| rel.components().count())
            .unwrap_or(0);
        Some(Rule {
            path: path.to_path_buf(),
            frontmatter,
            scope,
            mtime_secs,
            body,
            depth,
        })
    }

    /// Build the deduped union of rules across scopes, repo instance wins over
    /// shared copy when both are active; within a scope, the deepest (most
    /// nested) scope-relative path wins and the winner is logged (N4/§4.10).
    fn dedup_by_name(&self, rules: Vec<Rule>) -> Vec<Rule> {
        let mut by_name: HashMap<String, Rule> = HashMap::new();
        for rule in rules {
            let name = rule.name().to_string();
            if name.is_empty() {
                continue;
            }
            let replace = match by_name.get(&name) {
                None => true,
                Some(existing) => {
                    let is_repo = rule.scope.starts_with("repo:");
                    let existing_is_repo = existing.scope.starts_with("repo:");
                    if is_repo && !existing_is_repo {
                        // Repo instance beats a global/workspace shared copy.
                        true
                    } else if !is_repo && existing_is_repo {
                        // A shared copy never displaces a repo instance.
                        false
                    } else if rule.scope == existing.scope {
                        // Same scope: deepest scope-relative path wins (N4).
                        let deeper = rule.depth > existing.depth;
                        if deeper {
                            log_once(
                                &format!("rules-deepest-wins:{name}"),
                                &format!(
                                    "rules hook: same name '{name}' at depth {} overrides depth {} in scope '{}'",
                                    rule.depth, existing.depth, rule.scope
                                ),
                            );
                        }
                        deeper
                    } else {
                        // Different non-repo scopes (global vs workspace): the
                        // first discovery order wins (global comes first).
                        false
                    }
                }
            };
            if replace {
                by_name.insert(name, rule.clone());
            }
        }
        let mut out: Vec<Rule> = by_name.into_values().collect();
        out.sort_by(|a, b| {
            a.scope
                .cmp(&b.scope)
                .then(a.name().cmp(b.name()))
        });
        out
    }

    /// Render the manifest + core block, bounded by `manifest_budget`.
    fn render_manifest(&self, rules: &[Rule]) -> String {
        let mut instructions = String::from("<instructions>\n");
        instructions.push_str("Here is a list of instruction files that contain rules for working with this repo.\n");
        instructions.push_str("When an instruction file applies to your task (based on its description or applyTo\n");
        instructions.push_str("pattern), follow it. If the content is not already included, read the file before\n");
        instructions.push_str("proceeding. Only load instruction files relevant to the current task.\n");
        instructions.push_str("When modifying or creating files, check for instructions whose applyTo matches.\n");

        let mut core_lines: Vec<String> = Vec::new();
        for rule in rules {
            let name = rule.name();
            instructions.push_str("<instruction>\n");
            // Pod-form: emit the rule's file name (basename, i.e. its `name`),
            // not the absolute on-disk path. The absolute path is noisy and
            // inflates the manifest (~25 chars × N rules) well past
            // `manifest_budget` for the global scope — the spec's measured
            // 6.8k assumed relative paths. The agent reads the rule by name
            // from its active scopes.
            instructions.push_str(&format!("<file>{name}.md</file>\n"));
            instructions.push_str(&format!("<description>{}</description>\n", rule.frontmatter.description));
            if !rule.frontmatter.apply_to.is_empty() {
                instructions.push_str(&format!(
                    "<applyTo>{}</applyTo>\n",
                    rule.frontmatter.apply_to.join(", ")
                ));
            }
            instructions.push_str("</instruction>\n");
            if rule.frontmatter.core {
                // Port the `rules-bind` toggle extraction (§4.4): only the
                // lines INSIDE the `<!-- core -->` markers are eager-injected
                // as the core block — NOT the rule's full body (the defect: the
                // full body of every `core: true` rule was injected, inflating
                // the core block to ~47k chars and truncating it to ~1%,
                // falsifying the "core never skipped" guarantee). An odd marker
                // count or a `core: true` rule with no markers contributes
                // nothing (fail-open, never capture-to-EOF).
                if let Some(block) = extract_core_block(&rule.body)
                    && !block.is_empty()
                {
                    // Match the plugin's format: a `<!-- from <name> -->`
                    // comment prefix, then the extracted block.
                    core_lines.push(format!("<!-- from {name} -->"));
                    core_lines.push(block);
                }
            }
        }
        instructions.push_str("</instructions>\n");

        let mut out = instructions;
        if !core_lines.is_empty() {
            out.push_str("\n## Core (never skipped)\n");
            for line in &core_lines {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// Deterministically attach rule bodies for the turn, bounded by
    /// `attach_budget`.
    fn build_attach_block(
        &self,
        rules: &[Rule],
        alias: &str,
        session_id: &str,
    ) -> (String, usize, usize) {
        let in_play: Vec<String> = read_in_play_paths(alias, session_id);
        let (in_play_matched, agent_scoped) = partition_attach_rules(rules, alias, &in_play);

        // Ordering: in-play (location-specific) rules FIRST, then agent-scoped;
        // within each, core first, then most-recently-modified.
        //
        // The spec's §4.5 "agent-scoped outranks in-play" orders *precedence for
        // equal-budget readers*, but the G2 guarantee is zero SILENT law loss:
        // an always-on agent-scoped rule (the relocated SOUL appendix, often
        // several KB each) must not crowd the budget so far that the specific
        // law for the file the session just edited is dropped to zero — the
        // edit-triggered rule is the more specific, contextual signal. In-play
        // bodies attach first so they are never starved; agent-scoped bodies
        // fill the remainder and are head-truncated when over budget.
        let order_key = |r: &&Rule| {
            (
                if r.frontmatter.core { 0u8 } else { 1u8 },
                std::cmp::Reverse(r.mtime_secs),
            )
        };
        let mut in_play_matched = in_play_matched;
        let mut agent_scoped = agent_scoped;
        in_play_matched.sort_by_key(order_key);
        agent_scoped.sort_by_key(order_key);

        let mut out = String::new();
        let mut attached_chars = 0usize;
        let mut attached_count = 0usize;

        let budget = self.config.attach_budget;
        for rule in in_play_matched.into_iter().chain(agent_scoped) {
            let block = format!(
                "\n### {} ({})\n{}",
                rule.name(),
                rule.scope,
                rule.body
            );
            if out.is_empty() {
                out.push_str("\n## Attached — files in play this session\n");
            }
            if attached_chars + block.len() > budget {
                // Head-fit: if the whole block exceeds the remaining budget,
                // append a head-truncated slice with an explicit note.
                let remaining = budget.saturating_sub(attached_chars);
                if remaining < 64 {
                    break;
                }
                let head = head_truncate(&block, remaining);
                out.push_str(head);
                out.push_str("\n[... attached rule body truncated]\n");
                attached_chars = budget;
                break;
            }
            out.push_str(&block);
            attached_chars += block.len();
            attached_count += 1;
        }

        (out, attached_count, attached_chars)
    }

    /// Discover `AGENTS.md` delivery items across the active scopes (§4.10).
    ///
    /// Repo scopes contribute their ROOT `AGENTS.md` (deliver once per session)
    /// plus every NESTED `AGENTS.md` (implicit `applyTo: <dir>/**` attach). The
    /// workspace contributes only its NESTED `AGENTS.md` files: the workspace
    /// ROOT `AGENTS.md` is the engine bootstrap file and is never re-listed.
    /// Discovered files are deduped by canonicalised path so a repo root that
    /// coincides with the workspace floor is not listed twice.
    fn discover_agents_md(&self, ctx: &PromptBuildHookContext) -> Vec<AgentsMd> {
        let roots = if self.config.repo_discovery {
            self.resolve_repo_roots(ctx)
        } else {
            Vec::new()
        };
        self.discover_agents_md_from_roots(roots, ctx.workspace())
    }

    /// The deterministic part of AGENTS.md discovery: given the resolved repo
    /// roots + the workspace, classify root vs nested and dedupe. Separated
    /// from [`Self::discover_agents_md`] so the classification law (repo-first,
    /// deepest-first, canonical dedup) is unit-testable without the
    /// process-global in-play registry.
    fn discover_agents_md_from_roots(&self, roots: Vec<PathBuf>, ws: &Path) -> Vec<AgentsMd> {
        let mut out: Vec<AgentsMd> = Vec::new();
        // Dedupe by CANONICALISED path: a repo root from `git rev-parse` is
        // canonical (macOS `/var` → `/private/var`), while the workspace path
        // may not be — the same on-disk file must not be listed twice.
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());

        // The workspace floor's AGENTS.md set comes from the SAME bounded,
        // mtime-cached walk the root finder uses — no second full scan here.
        // `is_root` is recomputed per path relative to a resolved repo root or
        // the workspace floor below.
        let (_floor_roots, floor_agents, _walked) = discover_floor_cached(ws);
        let ws_canon = canon(ws);
        // Canonicalise each discovered AGENTS.md ONCE (not per repo root): the
        // filter below then does pure path-prefix comparisons, so a workspace
        // with many nested `AGENTS.md` files × several repo roots costs a few
        // hundred `canonicalize` calls instead of `agents × roots`.
        let floor_agents_canon: Vec<(PathBuf, PathBuf)> = floor_agents
            .iter()
            .map(|p| (canon(p), p.clone()))
            .collect();

        // Repo scopes FIRST — root + nested AGENTS.md. A repo root that is also
        // a workspace subdirectory must keep its repo-root classification
        // (deliver-once-per-session), so the repo scan claims the path before
        // the workspace nested scan can mis-classify it. Deeper roots claim
        // first (an inner repo's own root AGENTS.md is root, not parent-nested).
        let mut roots = roots;
        roots.sort_by_key(|r| std::cmp::Reverse(r.components().count()));
        for root in roots {
            if !root.is_dir() {
                continue;
            }
            let root_canon = canon(&root);
            // For a root under the workspace floor, its AGENTS.md files are
            // already in `floor_agents` (the floor walk covers the subtree).
            // For a root OUTSIDE the floor (an in-play absolute path into
            // another checkout), scan it directly.
            let candidates: Vec<(PathBuf, bool)> = if root_canon.starts_with(&ws_canon) {
                floor_agents_canon
                    .iter()
                    .filter(|(canon_p, _)| canon_p.starts_with(&root_canon))
                    .map(|(canon_p, p)| {
                        let is_root = canon_p
                            .parent()
                            .map(|d| d == root_canon.as_path())
                            .unwrap_or(false);
                        (p.clone(), is_root)
                    })
                    .collect()
            } else {
                scan_agents_md(&root)
            };
            for (path, is_root) in candidates {
                if let Some(md) = self.load_agents_md(
                    &path,
                    format!("repo:{}", root.display()),
                    is_root,
                ) && seen.insert(canon(&path))
                {
                    out.push(md);
                }
            }
        }

        // Workspace scope — nested AGENTS.md only (root = engine bootstrap),
        // and only when the repo scope above has not already claimed the path.
        if ws.is_dir() {
            for (canon_p, path) in &floor_agents_canon {
                let is_root = canon_p
                    .parent()
                    .map(|d| d == ws_canon.as_path())
                    .unwrap_or(false);
                if is_root {
                    continue; // engine bootstrap, never re-listed
                }
                if let Some(md) = self.load_agents_md(path, "workspace".to_string(), false)
                    && seen.insert(canon_p.clone())
                {
                    out.push(md);
                }
            }
        }

        out.sort_by(|a, b| a.scope.cmp(&b.scope).then(a.path.cmp(&b.path)));
        out
    }

    fn load_agents_md(&self, path: &Path, scope: String, is_root: bool) -> Option<AgentsMd> {
        let Ok(body) = std::fs::read_to_string(path) else {
            log_once(
                &format!("agents-md-unreadable:{}", path.display()),
                "rules hook: unreadable AGENTS.md skipped",
            );
            return None;
        };
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Some(AgentsMd {
            path: path.to_path_buf(),
            dir,
            scope,
            is_root,
            body,
        })
    }

    /// Build the AGENTS.md delivery block (§4.10).
    ///
    /// - Repo-root `AGENTS.md`: the first repo-scope turn delivers the body
    ///   (bounded digest), later turns emit the one-line pointer. Delivery is
    ///   tracked in the session registry so the body is injected exactly once
    ///   per session per repo root.
    /// - Nested `AGENTS.md`: when an in-play (modified) path is under its
    ///   directory, the body attaches (bounded by `attach_budget`); otherwise
    ///   it contributes an index entry with its folder description.
    ///
    /// Returns the block text and the count of root bodies delivered.
    fn build_agents_md_block(&self, ctx: &PromptBuildHookContext) -> (String, usize) {
        let agents = self.discover_agents_md(ctx);
        if agents.is_empty() {
            return (String::new(), 0);
        }

        let alias = ctx.alias();
        let session_id = ctx.session_id();
        let in_play: Vec<PathBuf> = read_in_play_paths(alias, session_id)
            .into_iter()
            .map(|p| {
                let path = Path::new(&p);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    ctx.workspace().join(path)
                }
            })
            .collect();

        let mut root_bodies: Vec<&AgentsMd> = Vec::new();
        let mut root_pointers: Vec<&AgentsMd> = Vec::new();
        let mut nested_attached: Vec<&AgentsMd> = Vec::new();
        let mut nested_indexed: Vec<&AgentsMd> = Vec::new();

        for md in &agents {
            if md.is_root {
                let key = md
                    .path
                    .canonicalize()
                    .unwrap_or_else(|_| md.path.clone());
                let key = key.to_string_lossy();
                if root_agents_delivered(alias, session_id, &key) {
                    root_pointers.push(md);
                } else {
                    root_bodies.push(md);
                    record_root_agents_delivered(alias, session_id, &key);
                }
            } else {
                let under_dir = in_play.iter().any(|p| p.starts_with(&md.dir));
                if under_dir {
                    nested_attached.push(md);
                } else {
                    nested_indexed.push(md);
                }
            }
        }

        let mut out = String::new();
        let mut delivered = 0usize;
        // The block budget is shared by every section below: root bodies first
        // (the law), then pointers (cheap), then in-play attached bodies, then
        // the index (discoverability, capped hardest).
        let mut remaining = AGENTS_MD_BLOCK_BUDGET;

        // 1. Root bodies (once per session).
        for md in root_bodies {
            if out.is_empty() {
                out.push_str("\n## Repo instructions (AGENTS.md)\n");
            }
            delivered += 1;
            let heading = format!("### {} {}\n", md.scope, md.path.display());
            let body_cap = AGENTS_MD_ROOT_BODY_MAX_CHARS
                .min(remaining.saturating_sub(heading.chars().count()));
            let (head, truncated) = bounded_digest(&md.body, body_cap);
            out.push_str(&heading);
            out.push_str(head);
            if truncated {
                out.push_str("\n[... AGENTS.md truncated — read the file for full content]\n");
            }
            out.push('\n');
            remaining = remaining.saturating_sub(heading.chars().count() + head.chars().count());
        }

        // 2. Root pointers (already delivered this session).
        for md in root_pointers {
            if out.is_empty() {
                out.push_str("\n## Repo instructions (AGENTS.md)\n");
            }
            let line = format!(
                "{} — repo instructions loaded (AGENTS.md, {} chars) — re-read with file_read if needed\n",
                md.scope,
                md.body.chars().count()
            );
            if line.chars().count() > remaining {
                break;
            }
            remaining = remaining.saturating_sub(line.chars().count());
            out.push_str(&line);
        }

        // 3. Nested attached bodies (in-play dir match).
        let mut nested_budget = self.config.attach_budget.min(remaining);
        for md in nested_attached {
            let block =
                format!("### {} {}\n{}", md.scope, md.path.display(), md.body);
            if nested_budget == 0 {
                break;
            }
            if block.len() > nested_budget {
                let head = head_truncate(&block, nested_budget);
                out.push_str("\n## Attached AGENTS.md — in-play directory\n");
                out.push_str(head);
                out.push_str("\n[... attached AGENTS.md truncated]\n");
                remaining = remaining.saturating_sub(head.chars().count());
                break;
            }
            out.push_str("\n## Attached AGENTS.md — in-play directory\n");
            out.push_str(&block);
            nested_budget -= block.len();
            remaining = remaining.saturating_sub(block.chars().count());
        }

        // 4. Nested indexed-only entries (out of play): a bounded listing, most
        // general first (shallow paths), with an explicit count when truncated.
        // The index is discoverability, not law — 261 entries × ~200 chars was
        // the dominant cost of the measured 67,592-char block.
        let mut index_budget = AGENTS_MD_INDEX_BUDGET.min(remaining);
        let mut indexed = nested_indexed;
        indexed.sort_by_key(|md| (md.dir.components().count(), md.dir.clone()));
        let total_indexed = indexed.len();
        let mut listed = 0usize;
        let mut skipped = 0usize;
        for md in indexed {
            let entry = format!(
                "<instruction>\n<file>{}</file>\n<folder>{}</folder>\n</instruction>\n",
                md.scope,
                md.dir.display()
            );
            if entry.chars().count() > index_budget {
                skipped += 1;
                continue; // keep trying shorter entries that still fit
            }
            if listed == 0 {
                out.push_str("\n## AGENTS.md index\n");
            }
            index_budget -= entry.chars().count();
            out.push_str(&entry);
            listed += 1;
        }
        if skipped > 0 {
            out.push_str(&format!(
                "[... {} more AGENTS.md files not listed ({} of {}) — locate with a file search when working in that directory]\n",
                skipped, listed, total_indexed
            ));
        }

        (out, delivered)
    }

    /// Compose the full rules block appended to the prompt.
    ///
    /// Returns ``(block, manifest_count, attached_count, attached_chars,
    /// manifest_chars, agents_md_chars, manifest_truncated)`` — the three
    /// component sizes in addition to the combined block so the caller (and the
    /// ``rules block injected`` log line) can assert each budget independently:
    /// ``manifest_chars ≤ manifest_budget`` (index + core), ``attached_chars ≤
    /// attach_budget`` (attached rule bodies), and ``agents_md_chars`` (the
    /// once-per-session AGENTS.md delivery, budgeted SEPARATELY by
    /// ``AGENTS_MD_ROOT_BODY_MAX_CHARS`` — NOT counted against
    /// ``manifest_budget``). ``injected_chars`` (computed by the caller from
    /// ``block.len()``) remains the truthful TOTAL of all three.
    fn build_block(
        &self,
        ctx: &PromptBuildHookContext,
    ) -> (String, usize, usize, usize, usize, usize, bool) {
        let rules = self.dedup_by_name(self.discover(ctx));
        let manifest = self.render_manifest(&rules);
        let (attach, attached_count, attached_chars) =
            self.build_attach_block(&rules, ctx.alias(), ctx.session_id());
        let (agents_md_block, _agents_delivered) = self.build_agents_md_block(ctx);

        // Budget the manifest + core block (head-truncate with a note). The
        // `<instructions>` block must STAY well-formed even when truncated: cut
        // the inner entries and re-append the closing tag, so a downstream
        // consumer (or the inventory parser) always sees a closed block. Only
        // the instruction index itself is truncated; the core block (already
        // counted in the manifest) is head-truncated too when it is what
        // overflows.
        let mut manifest_truncated = false;
        let manifest = if manifest.len() > self.config.manifest_budget {
            manifest_truncated = true;
            let mut budget = self.config.manifest_budget;
            let mut out = String::new();
            if let Some((head, tail)) = split_manifest_tail(&manifest) {
                // head = everything through the closing ``</instructions>`` (or
                // the whole manifest when unclosed); tail = the core block.
                if head.len() <= budget {
                    // The instruction index fits; truncate the core tail only.
                    out.push_str(&head);
                    budget -= head.len();
                    if !tail.is_empty() && budget > 0 {
                        out.push_str(&head_truncate(&tail, budget));
                    }
                    out.push_str("\n[... instruction index truncated]\n");
                } else {
                    // The instruction index itself overflows: keep the opening
                    // tag + as many entries as fit + a re-closed tag.
                    let open_len = "<instructions>\n".len();
                    let close_len = "</instructions>\n".len();
                    let inner_budget = budget.saturating_sub(open_len + close_len);
                    out.push_str("<instructions>\n");
                    let body = &manifest[open_len..manifest.rfind("</instructions>").unwrap_or(manifest.len())];
                    out.push_str(&head_truncate(body, inner_budget));
                    out.push_str("\n[... instruction index truncated]\n");
                    out.push_str("</instructions>\n");
                }
            } else {
                out.push_str(&head_truncate(&manifest, budget));
                out.push_str("\n[... instruction index truncated]\n");
            }
            out
        } else {
            manifest
        };

        let mut block = manifest.clone();
        block.push_str(&attach);
        block.push_str(&agents_md_block);
        (
            block,
            rules.len(),
            attached_count,
            attached_chars,
            manifest.len(),
            agents_md_block.len(),
            manifest_truncated,
        )
    }
}

fn head_truncate(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars || max_chars == 0 {
        return &s[..std::cmp::min(s.len(), max_chars)];
    }
    let mut end = max_chars;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Head-truncate a body to `max_chars`, returning `(head, truncated)`.
fn bounded_digest(body: &str, max_chars: usize) -> (&str, bool) {
    if body.len() <= max_chars {
        (body, false)
    } else {
        (head_truncate(body, max_chars), true)
    }
}

/// Apply the rules block to the prompt idempotently (I1).
///
/// The block (if non-empty) is emitted after the `<!-- rules-hook:v1 -->`
/// marker at the block start. If the marker is already present in the prompt,
/// everything from the marker to the end is replaced rather than appended, so
/// running the hook twice on one prompt yields exactly one block. When the
/// block is empty and a prior block is present, the prior block is stripped.
fn apply_rules_hook(prompt: String, block: &str) -> String {
    let block = block.trim_matches('\n');
    if let Some(idx) = prompt.find(RULES_HOOK_MARKER) {
        // Replace from the marker to the end.
        let mut out = prompt[..idx].to_string();
        if !block.is_empty() {
            let _ = out.pop(); // drop the trailing newline before the marker
            out.push('\n');
            out.push_str(RULES_HOOK_MARKER);
            out.push('\n');
            out.push_str(block);
            out.push('\n');
        }
        out
    } else if block.is_empty() {
        prompt
    } else {
        let mut out = prompt;
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(RULES_HOOK_MARKER);
        out.push('\n');
        out.push_str(block);
        out.push('\n');
        out
    }
}

/// Split the rendered manifest into `(instruction_index_through_closing_tag,
/// trailing_core_block)`. The instruction index is everything through the
/// closing ``</instructions>``; the core block (``## Core``) trails it. When
/// the manifest is not as expected, returns the whole thing with an empty tail
/// so the caller can safely head-truncate.
fn split_manifest_tail(manifest: &str) -> Option<(String, String)> {
    let close_marker = "</instructions>\n";
    match manifest.find(close_marker) {
        Some(idx) => {
            let end = idx + close_marker.len();
            Some((
                manifest[..end].to_string(),
                manifest[end..].to_string(),
            ))
        }
        None => Some((manifest.to_string(), String::new())),
    }
}

#[async_trait]
impl HookHandler for RulesHook {
    fn name(&self) -> &str {
        "rules"
    }

    fn priority(&self) -> i32 {
        -100
    }

    async fn before_prompt_build_with_context(
        &self,
        ctx: &PromptBuildHookContext,
        prompt: String,
    ) -> HookResult<String> {
        self.record_turn(ctx);
        let (
            block,
            manifest_count,
            attached_count,
            attached_chars,
            manifest_chars,
            agents_md_chars,
            manifest_truncated,
        ) = self.build_block(ctx);
        let injected_chars = block.len();
        let stats = DiscoveryStats::snapshot();
        if !block.is_empty() {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Load)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_attrs(::serde_json::json!({
                        "hook": "rules",
                        "alias": ctx.alias(),
                        "turn_id": ctx.turn_id(),
                        "injected_chars": injected_chars,
                        "manifest_chars": manifest_chars,
                        "agents_md_chars": agents_md_chars,
                        "manifest_count": manifest_count,
                        "attached_count": attached_count,
                        "attached_chars": attached_chars,
                        "manifest_truncated": manifest_truncated,
                        "discovery_stat_hits": stats.stat_hits,
                        "discovery_walks": stats.walks,
                        "discovery_walk_millis": stats.walk_millis,
                        "discovery_last_stat_millis": stats.last_stat_millis,
                    })),
                "rules block injected into system prompt"
            );
        }
        // I1 idempotency: emit the marker at the block start; if it is already
        // present in the prompt, replace from the marker rather than append.
        HookResult::Continue(apply_rules_hook(prompt, &block))
    }

    async fn on_after_tool_call_with_context_and_args(
        &self,
        context: &ToolCallHookContext,
        tool: &str,
        args: &serde_json::Value,
        _result: &ToolResult,
        _duration: Duration,
    ) {
        // Only correlate when the context carries a stable turn identity, and
        // attribute to the session via the turn_id prefix.
        if !context.is_correlated() {
            return;
        }
        let invocation_id = context.invocation_id();
        let turn_id = invocation_id
            .split(':')
            .next()
            .unwrap_or(invocation_id);
        // turn_id → (alias, session_id) is the correlation key: the prompt
        // build records the turn's session, and tool events are attributed to
        // that same session. We look it up through the turn→session map.
        let Some((alias, session_id)) = resolve_turn_session(turn_id) else {
            return;
        };
        let Some(path) = extract_path(tool, args) else {
            return;
        };
        if is_write_tool(tool) {
            // Attach trigger.
            record_in_play_path(&alias, &session_id, &path, self.config.in_play_paths);
        } else if is_read_tool(tool) {
            // Resolution only: recorded for repo-scope refinement but
            // not as an attach trigger.
            record_read_path(&alias, &session_id, &path);
        }
        // shell: best-effort; a write-looking token is an attach trigger
        // only when the command appears to modify a file.
        if tool.eq_ignore_ascii_case("shell") || tool.eq_ignore_ascii_case("bash") {
            record_in_play_path(&alias, &session_id, &path, self.config.in_play_paths);
        }
    }
}

/// Turn id → (alias, session_id) lookup, populated at prompt build.
///
/// The prompt-build hook records the `(alias, session_id)` for the turn so
/// later tool events (which only carry `turn_id:iteration:call_index`) can be
/// attributed to the right session's in-play set.
type TurnSessionMap = HashMap<String, (String, String)>;

static TURN_SESSIONS: Mutex<Option<TurnSessionMap>> = Mutex::new(None);

fn resolve_turn_session(turn_id: &str) -> Option<(String, String)> {
    let guard = TURN_SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    guard.as_ref().and_then(|m| m.get(turn_id).cloned())
}

/// Record the `(alias, session_id)` for a turn at prompt-build time.
pub(crate) fn record_turn_session(turn_id: &str, alias: &str, session_id: &str) {
    let mut guard = TURN_SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() >= SESSION_REGISTRY_CAPACITY * 4 {
        // Bounded: clear the oldest half.
        let keys: Vec<String> = map.keys().cloned().collect();
        let half = keys.len() / 2;
        for k in keys.into_iter().take(half) {
            map.remove(&k);
        }
    }
    map.insert(turn_id.to_string(), (alias.to_string(), session_id.to_string()));
}

/// Read paths (resolution-only), keyed separately so repo-scope refinement can
/// use them without triggering attach.
type ReadPathMap = HashMap<(String, String), Vec<String>>;

static READ_PATHS: Mutex<Option<ReadPathMap>> = Mutex::new(None);

fn record_read_path(alias: &str, session_id: &str, path: &str) {
    let mut guard = READ_PATHS.lock().unwrap_or_else(|p| p.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let key = (alias.to_string(), session_id.to_string());
    let entry = map.entry(key).or_default();
    if !entry.iter().any(|p| p == path) {
        entry.push(path.to_string());
    }
    if entry.len() > 256 {
        let overflow = entry.len() - 256;
        entry.drain(0..overflow);
    }
}

// ── Fail-open logging (once per condition) ──────────────────────

static LOGGED_CONDITIONS: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn log_once(condition: &str, message: &str) {
    let mut guard = LOGGED_CONDITIONS.lock().unwrap_or_else(|p| p.into_inner());
    let set = guard.get_or_insert_with(HashSet::new);
    if set.insert(condition.to_string()) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"hook": "rules", "condition": condition})),
            message
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_rules_dir_does_not_follow_symlink_dirs() {
        // N6: a symlinked directory inside the corpus must not be followed —
        // following a self-referential symlink recurses forever (or, for a
        // non-cyclic but duplicating symlink, double-scans the same files).
        let dir = std::env::temp_dir().join(format!("zc-rules-symlink-{}", std::process::id()));
        let rules = dir.join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(
            rules.join("real-rule.md"),
            "---\nname: real-rule\ndescription: when\n---\nBody.\n",
        )
        .unwrap();
        // A self-referential symlink (would loop forever if followed) AND a
        // symlink back up to the parent (would re-scan real-rule.md twice).
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(".", rules.join("self-loop"));
            let _ = std::os::unix::fs::symlink("..", rules.join("up"));
        }

        let found = scan_rules_dir(&rules);
        // Only the single real rule, once — no cycle, no duplicates.
        let real: Vec<_> = found
            .iter()
            .filter(|p| p.file_name().map(|n| n == "real-rule.md").unwrap_or(false))
            .collect();
        assert_eq!(
            real.len(),
            1,
            "real-rule.md must be discovered exactly once (no symlink cycle/dup); got {found:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_agents_md_does_not_follow_symlink_dirs() {
        // N6 (AGENTS.md twin): the same symlink-cycle hazard in scan_agents_md.
        let dir = std::env::temp_dir().join(format!("zc-agents-symlink-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "root\n").unwrap();
        std::fs::write(dir.join("sub").join("AGENTS.md"), "nested\n").unwrap();
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(".", dir.join("self-loop"));
        }

        let found = scan_agents_md(&dir);
        let real: Vec<_> = found
            .iter()
            .filter(|(p, _)| p.file_name().map(|n| n == "AGENTS.md").unwrap_or(false))
            .collect();
        assert_eq!(real.len(), 2, "root + nested only, no symlink re-scan; got {found:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_to_matches_always_match_patterns() {
        assert!(apply_to_matches("**", "any/path.rs"));
        assert!(apply_to_matches("**/*", "src/main.rs"));
        assert!(apply_to_matches("*", "whatever"));
    }

    #[test]
    fn apply_to_matches_bare_pattern_is_prefixed() {
        // Bare ".rs" → "**/*.rs" matches any .rs path.
        assert!(apply_to_matches("*.rs", "src/lib.rs"));
        assert!(apply_to_matches("*.rs", "lib.rs"));
        assert!(!apply_to_matches("*.rs", "src/lib.go"));
    }

    #[test]
    fn apply_to_matches_is_case_insensitive() {
        assert!(apply_to_matches("**/*.TOML", "a/b/config.toml"));
    }

    #[test]
    fn apply_to_matches_comma_separated() {
        assert!(apply_to_matches("*.rs, *.go", "main.go"));
        assert!(apply_to_matches("*.rs, *.go", "main.rs"));
        assert!(!apply_to_matches("*.rs, *.go", "main.py"));
    }

    #[test]
    fn apply_to_matches_negation() {
        assert!(!apply_to_matches("*.rs, !vendor/*.rs", "vendor/lib.rs"));
        assert!(apply_to_matches("*.rs, !vendor/*.rs", "src/lib.rs"));
    }

    #[test]
    fn empty_apply_to_matches_nothing() {
        assert!(!apply_to_matches("", "src/lib.rs"));
    }

    #[test]
    fn split_frontmatter_detects_and_splits() {
        let raw = "---\nname: foo\ndescription: when triggered\n---\nbody line\n";
        let (fm, body) = split_frontmatter(raw).expect("has frontmatter");
        assert!(fm.contains("name: foo"));
        assert!(body.contains("body line"));
    }

    #[test]
    fn split_frontmatter_none_without_delimiter() {
        let raw = "no frontmatter here\njust body\n";
        assert!(split_frontmatter(raw).is_none());
    }

    #[test]
    fn parse_frontmatter_scalar_and_list_apply_to() {
        let fm = parse_frontmatter("name: foo\ndescription: when\napplyTo: \"*.rs, *.go\"\ncore: true\n").unwrap();
        assert_eq!(fm.name, "foo");
        assert_eq!(fm.apply_to, vec!["*.rs".to_string(), "*.go".to_string()]);
        assert!(fm.core);

        let fm = parse_frontmatter("applyTo:\n  - '*.rs'\n  - '*.go'\nagents:\n  - director\n").unwrap();
        assert_eq!(fm.apply_to.len(), 2);
        assert_eq!(fm.agents, vec!["director".to_string()]);
    }

    #[test]
    fn parse_frontmatter_defaults() {
        let fm = parse_frontmatter("name: foo\ndescription: when\n").unwrap();
        assert!(fm.apply_to.is_empty());
        assert!(fm.agents.is_empty());
        assert!(!fm.core);
    }

    // ── The `<!-- core -->` toggle extraction (§4.4) ──────────────────────
    //
    // The defect: `render_manifest` injected the FULL BODY of every
    // `core: true` rule into the core block (47k chars for 5 rules), instead
    // of extracting only the `<!-- core -->`-marked lines (the rules-bind
    // plugin's `extractCoreBlock` toggle semantics + spec §4.4). These tests
    // pin the toggle state machine at the `extract_core_block` seam.

    #[test]
    fn extract_core_block_yields_only_marked_lines() {
        let body = "leading prose\n<!-- core -->\nALWAYS LAW ONE\nALWAYS LAW TWO\n<!-- core -->\ntrailing prose\n";
        let out = extract_core_block(body).expect("even markers extract");
        assert_eq!(out, "ALWAYS LAW ONE\nALWAYS LAW TWO", "only the marked lines are emitted");
        assert!(!out.contains("leading prose"), "text before the open marker is dropped");
        assert!(!out.contains("trailing prose"), "text after the close marker is dropped");
    }

    #[test]
    fn extract_core_block_handles_multiple_pairs() {
        // The identical marker opens AND closes; multiples are handled (the
        // rules-bind plugin verified canary-main-reconciliation.md carries two
        // pairs: l.21/23 and l.133/135).
        let body = "<!-- core -->\nPAIR ONE\n<!-- core -->\nbetween\n<!-- core -->\nPAIR TWO\n<!-- core -->\n";
        let out = extract_core_block(body).expect("two even pairs extract");
        assert_eq!(out, "PAIR ONE\nPAIR TWO", "both pairs' interiors are emitted");
        assert!(!out.contains("between"), "text between the pairs is dropped");
    }

    #[test]
    fn extract_core_block_odd_markers_yield_none() {
        // An odd marker count (unclosed pair) yields NO extraction — never
        // capture-to-EOF (the odd-marker parity guarantee).
        let body = "<!-- core -->\nALWAYS LAW\n(unclosed)";
        assert!(extract_core_block(body).is_none(), "odd markers → no extraction");
    }

    #[test]
    fn extract_core_block_no_markers_yields_empty() {
        assert_eq!(extract_core_block("no markers here\n").unwrap(), "", "no markers → empty block");
    }

    #[test]
    fn render_manifest_core_true_without_markers_contributes_nothing() {
        // §4.4 + the litmus: a `core: true` rule with NO `<!-- core -->`
        // markers (or an odd count) contributes NOTHING to the core block —
        // the fix's fail-open contract (never capture the full body, never
        // capture-to-EOF).
        let hook = RulesHook::new(RulesHookConfig::default());
        let rule = Rule {
            path: PathBuf::from("/global/no-marker-core.md"),
            frontmatter: RuleFrontmatter {
                name: "no-marker-core".into(),
                description: "when".into(),
                core: true,
                ..Default::default()
            },
            scope: "global".into(),
            mtime_secs: 0,
            body: "The FULL body must NOT be injected.\n".into(),
            depth: 0,
        };
        let manifest = hook.render_manifest(&[rule]);
        assert!(
            !manifest.contains("## Core (never skipped)"),
            "a core rule with no markers emits no core block"
        );
        assert!(
            !manifest.contains("The FULL body"),
            "the full body is never injected as core"
        );
    }

    #[test]
    fn render_manifest_core_char_count_drops_to_marked_size() {
        // The manifest char count must drop to the ~index + marked-core size —
        // NOT the index + full-body size the defect produced. A large
        // `core: true` body of 40,000 chars with a tiny marked interior yields
        // a manifest far below the body size (and below the 11,000 budget the
        // post-fix spec cleanly fits).
        let hook = RulesHook::new(RulesHookConfig::default());
        let raw = format!(
            "---\nname: big-core\ndescription: marks a tiny core over a huge body\ncore: true\n---\n{}\n<!-- core -->\nONLY THIS IS CORE\nAND THIS\n<!-- core -->\n{}\n",
            "X".repeat(40_000),
            "Y".repeat(20_000),
        );
        let (_, body) = split_frontmatter(&raw).unwrap();
        let rule = Rule {
            path: PathBuf::from("/global/big-core.md"),
            frontmatter: RuleFrontmatter {
                name: "big-core".into(),
                description: "marks a tiny core over a huge body".into(),
                core: true,
                ..Default::default()
            },
            scope: "global".into(),
            mtime_secs: 0,
            body,
            depth: 0,
        };
        let manifest = hook.render_manifest(&[rule]);
        assert!(
            manifest.contains("ONLY THIS IS CORE"),
            "the marked interior is emitted"
        );
        assert!(
            manifest.contains("AND THIS"),
            "the second marked line is emitted"
        );
        assert!(
            manifest.len() < 11_000,
            "the manifest (index + marked core) must fit under the post-fix 11,000 budget — got {}",
            manifest.len()
        );
        assert!(
            !manifest.contains(&"X".repeat(40_000)),
            "the 40k-char bulk section is not in the manifest"
        );
        assert!(
            !manifest.contains(&"Y".repeat(20_000)),
            "the 20k-char trailing section is not in the manifest"
        );
    }

    #[test]
    fn extract_path_write_read_shell() {
        assert_eq!(
            extract_path("file_write", &serde_json::json!({"path": "a/b.md"})),
            Some("a/b.md".to_string())
        );
        assert_eq!(
            extract_path("file_read", &serde_json::json!({"file_path": "c/d.rs"})),
            Some("c/d.rs".to_string())
        );
        assert_eq!(
            extract_path("shell", &serde_json::json!({"command": "vim src/main.rs"})),
            Some("src/main.rs".to_string())
        );
        assert_eq!(
            extract_path("shell", &serde_json::json!({"command": "echo hello"})),
            None
        );
    }

    #[test]
    fn registry_records_and_reads_in_play_with_cap() {
        // Isolate: clear the registry first.
        {
            let mut guard = registry_inner().lock().unwrap();
            *guard = None;
        }
        record_in_play_path("director", "sess", "a.md", 50);
        record_in_play_path("director", "sess", "b.rs", 50);
        let paths = read_in_play_paths("director", "sess");
        assert_eq!(paths, vec!["a.md".to_string(), "b.rs".to_string()]);
        // Different session is isolated.
        assert!(read_in_play_paths("director", "other").is_empty());
        // Different alias is isolated.
        assert!(read_in_play_paths("worker", "sess").is_empty());
    }

    #[test]
    fn dedup_by_name_repo_wins_over_global() {
        let hook = RulesHook::new(RulesHookConfig::default());
        let mk = |name: &str, scope: &str, depth: usize| Rule {
            path: PathBuf::from(format!("/{scope}/{name}.md")),
            frontmatter: RuleFrontmatter {
                name: name.to_string(),
                description: "when".into(),
                ..Default::default()
            },
            scope: scope.to_string(),
            mtime_secs: 0,
            body: String::new(),
            depth,
        };
        let rules = vec![
            mk("event-driven-swarm", "global", 0),
            mk("event-driven-swarm", "repo:/repo", 0),
            mk("quorum-review", "global", 0),
        ];
        let deduped = hook.dedup_by_name(rules);
        assert_eq!(deduped.len(), 2);
        let ed = deduped.iter().find(|r| r.name() == "event-driven-swarm").unwrap();
        assert_eq!(ed.scope, "repo:/repo");
    }

    #[test]
    fn dedup_by_name_deepest_wins_within_scope() {
        // N4/§4.10: the same rule name within a scope resolves to the deepest
        // (most nested) scope-relative path; repo still beats global across
        // scopes regardless of depth.
        let hook = RulesHook::new(RulesHookConfig::default());
        let mk = |name: &str, scope: &str, depth: usize| Rule {
            path: PathBuf::from(format!("/{scope}/{name}.md")),
            frontmatter: RuleFrontmatter {
                name: name.to_string(),
                description: "when".into(),
                ..Default::default()
            },
            scope: scope.to_string(),
            mtime_secs: 0,
            body: String::new(),
            depth,
        };
        // Same global scope, two depths → the deeper (depth 2) wins.
        let global_shallow = mk("shared-rule", "global", 0);
        let global_deep = mk("shared-rule", "global", 2);
        let deduped = hook.dedup_by_name(vec![global_shallow.clone(), global_deep.clone()]);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].depth, 2, "deepest within a scope wins");

        // Repo still beats global even when the repo instance is shallower.
        let repo_shallow = mk("shared-rule", "repo:/repo", 0);
        let global_deep2 = mk("shared-rule", "global", 5);
        let deduped2 = hook.dedup_by_name(vec![global_deep2, repo_shallow]);
        assert_eq!(deduped2.len(), 1);
        assert_eq!(
            deduped2[0].scope, "repo:/repo",
            "repo still beats global across scopes, depth notwithstanding"
        );
    }

    #[tokio::test]
    async fn prompt_build_manifest_truncation_preserves_closing_tag() {
        // When the manifest is over `manifest_budget`, the `<instructions>`
        // block must STAY well-formed: the closing `</instructions>` tag must
        // survive so downstream parsers (the eval inventory) see a closed block
        // — the P1 AFTER run caught the old head-truncate cutting the close tag
        // and leaving a dangling block.
        let dir = std::env::temp_dir().join(format!("zc-rules-trunc-{}", std::process::id()));
        let rules_dir = dir.join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        // A corpus large enough to overflow a deliberately tiny budget.
        for i in 0..40 {
            std::fs::write(
                rules_dir.join(format!("rule-{i:02}.md")),
                format!(
                    "---\nname: rule-{i:02}\ndescription: binds when doing rule-{i:02} work with a long enough description to fill the manifest quickly\n---\nBody {i}.\n"
                ),
            )
            .unwrap();
        }

        let hook = RulesHook::new(RulesHookConfig {
            enabled: true,
            global_dir: rules_dir.to_string_lossy().into_owned(),
            workspace_dir: ".agents/rules".to_string(),
            repo_discovery: false,
            manifest_budget: 1024, // force truncation
            attach_budget: 8000,
            ..RulesHookConfig::default()
        });
        let ctx = PromptBuildHookContext::new("director", dir.clone(), "sess-1", "turn-1");
        match hook
            .before_prompt_build_with_context(&ctx, "SYSTEM".to_string())
            .await
        {
            HookResult::Continue(prompt) => {
                assert!(prompt.contains("<instructions>"), "opening tag present");
                assert!(
                    prompt.contains("</instructions>"),
                    "closing tag must survive truncation — got a dangling block"
                );
                assert!(
                    prompt.contains("instruction index truncated"),
                    "the truncation note is emitted"
                );
            }
            HookResult::Cancel(_) => panic!("rules hook must continue"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prompt_build_appends_manifest_and_core_for_workspace_scope() {
        let dir = std::env::temp_dir().join(format!("zc-rules-test-{}", std::process::id()));
        let ws = dir.join("workspace");
        let rules_dir = ws.join(".agents").join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("event-driven-swarm.md"),
            "---\nname: event-driven-swarm\ndescription: binds when doing async work\ncore: true\n---\nPreamble prose (NOT core — outside the markers).\n<!-- core -->\nNever sleep-poll; watch.\nSecond core line.\n<!-- core -->\nTrailing prose (NOT core).\n",
        )
        .unwrap();
        std::fs::write(
            rules_dir.join("quorum-review.md"),
            "---\nname: quorum-review\ndescription: binds when running a review\napplyTo: \"*.rs\"\n---\nBody here.\n",
        )
        .unwrap();

        let hook = RulesHook::new(RulesHookConfig {
            enabled: true,
            global_dir: String::new(),
            workspace_dir: ".agents/rules".to_string(),
            repo_discovery: false,
            ..RulesHookConfig::default()
        });
        // Write hook needs the config passed; it's already set. Use the hook's
        // block builder directly by invoking the prompt hook.
        let ctx = PromptBuildHookContext::new(
            "director",
            ws.clone(),
            "sess-1",
            "turn-9",
        );
        match hook
            .before_prompt_build_with_context(&ctx, "SYSTEM".to_string())
            .await
        {
            HookResult::Continue(prompt) => {
                assert!(prompt.contains("SYSTEM"), "original prompt preserved");
                assert!(prompt.contains("<instructions>"), "manifest present");
                assert!(prompt.contains("event-driven-swarm"), "rule indexed");
                assert!(prompt.contains("quorum-review"), "rule indexed");
                assert!(prompt.contains("## Core (never skipped)"), "core block present");
                assert!(prompt.contains("Never sleep-poll"), "marked core line injected");
                assert!(prompt.contains("Second core line."), "second marked line injected");
                assert!(
                    prompt.contains("<!-- from event-driven-swarm -->"),
                    "the core block carries the source rule name as a comment prefix"
                );
                assert!(
                    !prompt.contains("Preamble prose"),
                    "text OUTSIDE the markers is NOT in the core block"
                );
                assert!(
                    !prompt.contains("Trailing prose"),
                    "trailing text after the closing marker is NOT in the core block"
                );
                assert!(
                    !prompt.contains("Body here."),
                    "no in-play paths → no attach body"
                );
            }
            HookResult::Cancel(_) => panic!("rules hook must continue"),
        }

        // After a write-tool event touches a `.rs` path, the matching rule's
        // body attaches on the next prompt build.
        let turn_key = format!("turn-9:0:0");
        let tc = ToolCallHookContext::new(&turn_key);
        hook.on_after_tool_call_with_context_and_args(
            &tc,
            "file_write",
            &serde_json::json!({"path": "src/lib.rs"}),
            &ToolResult {
                success: true,
                output: "ok".into(),
                error: None,
            },
            Duration::ZERO,
        )
        .await;

        let ctx2 = PromptBuildHookContext::new("director", ws.clone(), "sess-1", "turn-9");
        match hook
            .before_prompt_build_with_context(&ctx2, "SYSTEM".to_string())
            .await
        {
            HookResult::Continue(prompt) => {
                assert!(prompt.contains("## Attached — files in play this session"));
                assert!(prompt.contains("Body here."), "in-play `.rs` attaches body");
            }
            HookResult::Cancel(_) => panic!("rules hook must continue"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn repo_scope_resolves_from_workspace_relative_in_play_path() {
        // CONTEXT-LAYERS-REPO-SCOPE-ANCHOR: an in-play path recorded from a
        // tool arg is workspace-RELATIVE (`demo-repo/HELLO.md`), but the
        // daemon's CWD is not the agent workspace — the bare relative path
        // resolved against `git -C`/ancestor-walk from the wrong root, so a
        // nested repo's rules were never discovered. The path must be anchored
        // to the session workspace before repo-root resolution.
        let dir = std::env::temp_dir().join(format!("zc-rules-repo-{}", std::process::id()));
        let ws = dir.join("workspace");
        let repo = ws.join("demo-repo");
        let rules_dir = repo.join(".agents").join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("demo-scope.md"),
            "---\nname: demo-scope\ndescription: binds when working inside demo-repo.\n---\nBody.\n",
        )
        .unwrap();
        // A real git repo so `git rev-parse --show-toplevel` resolves, and a
        // touch file so the in-play path's parent is a filename's parent.
        std::fs::write(repo.join("HELLO.md"), "# hi\n").unwrap();
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .arg("-q")
            .status();
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.email=t@t", "-c", "user.name=t", "add", "-A"])
            .status();
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "seed"])
            .status();

        let hook = RulesHook::new(RulesHookConfig {
            enabled: true,
            global_dir: String::new(),
            workspace_dir: ".agents/rules".to_string(),
            repo_discovery: true,
            repo_roots_max: 4,
            manifest_budget: 8192,
            attach_budget: 8000,
            ..RulesHookConfig::default()
        });

        // Record a workspace-relative in-play path (the tool-arg shape).
        {
            let mut guard = registry_inner().lock().unwrap_or_else(|p| p.into_inner());
            *guard = None;
        }
        record_in_play_path("director", "sess", "demo-repo/HELLO.md", 50);

        // The `director` replica of the workspace model the prompt build uses:
        // the workspace is `ws`, the in-play path is relative.
        let ctx = PromptBuildHookContext::new("director", ws.clone(), "sess", "turn-1");
        let roots = hook.resolve_repo_roots(&ctx);
        // git resolves the repo root canonically (macOS `/var` → `/private/var`),
        // so compare canonicalized.
        let expected = repo.canonicalize().unwrap_or(repo.clone());
        assert!(
            roots.iter().any(|r| r.canonicalize().map(|c| c == expected).unwrap_or(false)),
            "a workspace-relative in-play path must resolve its nested repo root; got {roots:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_agents_md_lists_root_and_nested() {
        let dir = std::env::temp_dir().join(format!("zc-agents-scan-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "root\n").unwrap();
        std::fs::write(dir.join("sub").join("AGENTS.md"), "nested\n").unwrap();

        let found = scan_agents_md(&dir);
        assert_eq!(found.len(), 2);
        let by_name: Vec<(bool, String)> = found
            .iter()
            .map(|(p, is_root)| (*is_root, p.to_string_lossy().into_owned()))
            .collect();
        assert!(by_name.iter().any(|(r, p)| *r && p.ends_with("AGENTS.md") && !p.contains("/sub/")));
        assert!(by_name.iter().any(|(r, p)| !*r && p.contains("/sub/AGENTS.md")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Bounded discovery + mtime cache (discovery-fix) ──────────

    #[test]
    fn walk_floor_finds_nested_repo_and_prunes_git() {
        // A nested repo (dir with `.git`) under a non-repo floor is discovered;
        // `.git/` INTERNALS (a file inside `.git/`) are never marked/indexed.
        let dir = std::env::temp_dir().join(format!("zc-bounded-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("nested").join(".git")).unwrap();
        std::fs::write(dir.join("nested").join(".git").join("index"), "x").unwrap();
        std::fs::write(dir.join("nested").join("AGENTS.md"), "law\n").unwrap();

        let (roots, markers, agents) = walk_floor(&dir, 16);

        // The nested repo is a discovered root.
        let canon_nested = canon(&dir.join("nested"));
        assert!(
            roots.iter().any(|r| *r == canon_nested),
            "nested repo root discovered; got {roots:?}"
        );
        // The nested AGENTS.md is indexed.
        assert!(
            agents.iter().any(|p| p.ends_with("AGENTS.md")),
            "nested AGENTS.md indexed; got {agents:?}"
        );
        // No marker descends INTO `.git/` internals.
        assert!(
            !markers.iter().any(|(p, _)| p.to_string_lossy().contains("/.git/index")),
            "`.git/` internals are pruned from markers; got {markers:?}"
        );
        // The `.git` dir itself IS a marker (signals the repo root).
        assert!(
            markers.iter().any(|(p, k)| *k == MarkerKind::Git && p.ends_with(".git")),
            "the `.git` dir itself is recorded as a marker"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_floor_prunes_cargo_cache_and_honours_gitignore() {
        let dir = std::env::temp_dir().join(format!("zc-prune-{}", std::process::id()));
        // `.cargo` and `.cache` always pruned.
        std::fs::create_dir_all(dir.join(".cargo").join("registry")).unwrap();
        std::fs::create_dir_all(dir.join(".cache").join("uv")).unwrap();
        std::fs::write(dir.join(".cache").join("uv").join("blob"), "x").unwrap();
        // A gitignored dir (`target/`) pruned via .gitignore.
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target").join("AGENTS.md"), "ignored\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        // A legitimate nested repo that must STILL be found.
        std::fs::create_dir_all(dir.join("skill-lab.gen")).unwrap();
        std::fs::write(dir.join("skill-lab.gen").join(".git"), "gitdir: fake\n").unwrap();
        std::fs::write(dir.join("skill-lab.gen").join("AGENTS.md"), "lab\n").unwrap();

        let (roots, markers, agents) = walk_floor(&dir, 16);

        // No marker/index descends into `.cargo`, `.cache`, or `target`.
        let s = |p: &Path| p.to_string_lossy().into_owned();
        assert!(!markers.iter().any(|(p, _)| s(p).contains("/.cargo/")), ".cargo pruned");
        assert!(!markers.iter().any(|(p, _)| s(p).contains("/.cache/")), ".cache pruned");
        assert!(
            !agents.iter().any(|p| s(p).contains("/target/AGENTS.md")),
            "gitignored target/ AGENTS.md is not indexed"
        );
        // `skill-lab.gen` is NOT pruned (a worktree-like dir can be a repo).
        let mut found_lab = roots.iter().any(|r| s(r).ends_with("skill-lab.gen"));
        found_lab = found_lab || agents.iter().any(|p| s(p).ends_with("/skill-lab.gen/AGENTS.md"));
        assert!(found_lab, "skill-lab.gen (a possible checkout) is still scanned");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_hit_is_stat_only_no_second_walk() {
        // A second discover_floor_cached on an unchanged tree must reuse the
        // cached manifest (was_walked == false): no re-traversal.
        let dir = std::env::temp_dir().join(format!("zc-cache-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("repo").join(".agents").join("rules")).unwrap();
        std::fs::write(dir.join("repo").join("AGENTS.md"), "law\n").unwrap();

        let (roots1, agents1, walked1) = discover_floor_cached(&dir);
        assert!(walked1, "first call walks");
        assert!(
            roots1.iter().any(|r| r.ends_with("repo")),
            "first walk finds the nested repo; got {roots1:?}"
        );

        // Second call: stat-only cache hit.
        let (roots2, agents2, walked2) = discover_floor_cached(&dir);
        assert!(!walked2, "unchanged tree → stat-only cache hit, no re-walk");
        assert_eq!(roots1, roots2, "cached roots match the first walk");
        assert_eq!(agents1, agents2, "cached AGENTS.md set matches");

        // Changing an EXISTING marker's mtime invalidates the cache → re-walk.
        // Set the marker to a deterministically older mtime (avoids second-
        // granularity flakiness of a same-second content write).
        std::fs::write(dir.join("repo").join("AGENTS.md"), "law v2\n").unwrap();
        {
            let f = std::fs::File::options()
                .write(true)
                .open(dir.join("repo").join("AGENTS.md"))
                .unwrap();
            let older = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
            f.set_modified(older).unwrap();
        }
        let (_roots3, _agents3, walked3) = discover_floor_cached(&dir);
        assert!(walked3, "a changed marker (AGENTS.md mtime) invalidates → re-walk");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_cache_ttl_picks_up_markers_that_appear() {
        // The mtime check only sees markers it RECORDED. A marker that appears
        // later (a new AGENTS.md, a newly cloned repo) must be found by the
        // structural TTL re-walk — that is the bounded-invalidation contract:
        // never re-walk every turn, but never go permanently blind either.
        let dir = std::env::temp_dir().join(format!("zc-ttl-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested").join("keep"), "x").unwrap();

        let (roots1, agents1, walked1) = discover_floor_cached_with_ttl(&dir, DISCOVERY_CACHE_TTL);
        assert!(walked1, "first call walks");
        assert!(roots1.is_empty(), "no repo yet: {roots1:?}");
        assert!(agents1.is_empty(), "no AGENTS.md yet: {agents1:?}");

        // A NEW marker appears one level down (the floor's own mtime does not
        // move for a grandchild, and no recorded marker changed).
        std::fs::write(dir.join("nested").join("AGENTS.md"), "new law\n").unwrap();

        // Within the TTL the cached view is served (documented bounded staleness).
        let (_r, agents_within_ttl, walked_within_ttl) =
            discover_floor_cached_with_ttl(&dir, DISCOVERY_CACHE_TTL);
        assert!(!walked_within_ttl, "within the TTL: stat-only, no re-walk");
        assert!(
            agents_within_ttl.is_empty(),
            "bounded staleness: the new marker is not seen inside the TTL window"
        );

        // Once the TTL elapses the structural re-walk finds it.
        let (_r2, agents_after_ttl, walked_after_ttl) =
            discover_floor_cached_with_ttl(&dir, std::time::Duration::ZERO);
        assert!(walked_after_ttl, "TTL elapsed → structural re-walk");
        assert!(
            agents_after_ttl.iter().any(|p| p.ends_with("nested/AGENTS.md")),
            "the marker that appeared is discovered after the TTL; got {agents_after_ttl:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn warm_discovery_populates_the_cache_for_the_first_turn() {
        // Startup warm-up: after `warm_discovery`, the first turn's read is a
        // stat-only cache hit — the pod never pays the walk on a live turn.
        let dir = std::env::temp_dir().join(format!("zc-warm-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("repo").join(".agents").join("rules")).unwrap();
        std::fs::write(dir.join("repo").join("AGENTS.md"), "law\n").unwrap();

        warm_discovery(&dir);

        let (roots, agents, walked) = discover_floor_cached(&dir);
        assert!(!walked, "after warm-up the turn is a stat-only hit (no walk)");
        assert!(roots.iter().any(|r| r.ends_with("repo")), "warm found the repo");
        assert!(agents.iter().any(|p| p.ends_with("AGENTS.md")), "warm indexed AGENTS.md");

        // A missing floor is a no-op, never a panic.
        warm_discovery(&dir.join("does-not-exist"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agents_md_block_respects_the_total_budget_with_many_nested_files() {
        // The per-body cap does not bound the BLOCK: a workspace with many
        // nested AGENTS.md files used to inject ~54K chars of index alone. The
        // block budget caps the whole delivery and the index gets the smallest
        // share, with an explicit count of what was dropped.
        let dir = std::env::temp_dir().join(format!("zc-blockbudget-{}", std::process::id()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join("AGENTS.md"),
            format!("ROOT LAW: kept.\n{}", "X".repeat(60_000)),
        )
        .unwrap();
        let _ = std::process::Command::new("git")
            .args(["-C", repo.to_str().unwrap(), "init", "-q"])
            .status();
        for i in 0..200 {
            let sub = dir.join(format!("area{i:03}"));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("AGENTS.md"), format!("nested {i}\n")).unwrap();
        }

        {
            let mut guard = registry_inner().lock().unwrap_or_else(|p| p.into_inner());
            *guard = None;
        }
        let hook = RulesHook::new(RulesHookConfig {
            enabled: true,
            global_dir: String::new(),
            workspace_dir: ".agents/rules".to_string(),
            repo_discovery: true,
            ..RulesHookConfig::default()
        });
        let ctx = PromptBuildHookContext::new("director", dir.clone(), "sess-budget", "turn-1");
        let (block, _count, _ac, _ac_chars, _manifest_chars, agents_md_chars, _truncated) =
            hook.build_block(&ctx);

        assert!(
            agents_md_chars <= AGENTS_MD_BLOCK_BUDGET + 512,
            "the WHOLE AGENTS.md block stays within the total budget; agents_md_chars={agents_md_chars}"
        );
        assert!(
            block.contains("ROOT LAW: kept"),
            "the root body's head survives (law before index)"
        );
        assert!(
            block.contains("more AGENTS.md files not listed"),
            "the dropped index entries are declared, not silently lost"
        );
        assert!(
            block.chars().count() < 20_000,
            "200 nested files + a 60k root body do not inflate the block; got {}",
            block.chars().count()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // Timing evidence for the discovery-fix report. `#[ignore]` by default:
    // run explicitly with
    //   cargo test -p zeroclaw-runtime --lib discovery_timing_report -- --ignored --nocapture
    // to print first-walk / stat-only / mtime-changed timings on a realistic
    // fixture (many files, nested repos, .git internals, .cargo/.cache, and a
    // gitignored dir).
    #[test]
    #[ignore]
    fn discovery_timing_report() {
        let dir = std::env::temp_dir().join(format!("zc-timing-{}", std::process::id()));
        // A floor that resembles a workspace: a nested repo with .git internals,
        // a heavy .cargo + .cache, a gitignored target/, and many leaf files.
        std::fs::create_dir_all(dir.join("repo").join(".git").join("objects")).unwrap();
        std::fs::write(dir.join("repo").join(".git").join("objects").join("b1"), "x").unwrap();
        std::fs::write(
            dir.join("repo").join("AGENTS.md"),
            &"X".repeat(67_592),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join(".cargo").join("registry").join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".cache").join("uv")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\nnode_modules/\n").unwrap();
        std::fs::create_dir_all(dir.join("target").join("debug")).unwrap();
        std::fs::write(dir.join("target").join("debug").join("blob"), "x").unwrap();
        // A spread of leaf files to give the walker real work.
        for i in 0..20_000 {
            let sub = dir.join(format!("d{}", i % 50));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join(format!("f{i}.rs")), "x").unwrap();
        }

        // First walk (cold) — the discovery-fix must NOT scan .cargo/.cache/
        // .git internals, but DOES build the manifest in one bounded pass.
        let t0 = std::time::Instant::now();
        let (roots1, agents1, walked1) = discover_floor_cached(&dir);
        let first_walk = t0.elapsed();
        assert!(walked1, "first call walks");
        assert!(roots1.iter().any(|r| r.ends_with("repo")), "nested repo found");

        // Stat-only turn (unchanged).
        let t0 = std::time::Instant::now();
        let (_r2, _a2, walked2) = discover_floor_cached(&dir);
        let stat_only = t0.elapsed();
        assert!(!walked2, "unchanged → stat-only");

        // mtime-changed turn (a marker changed) → re-walk.
        let f = std::fs::File::options()
            .write(true)
            .open(dir.join("repo").join("AGENTS.md"))
            .unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .unwrap();
        let t0 = std::time::Instant::now();
        let (_r3, _a3, walked3) = discover_floor_cached(&dir);
        let changed_turn = t0.elapsed();
        assert!(walked3, "changed marker → re-walk");

        let stats = DiscoveryStats::snapshot();
        eprintln!("\n=== discovery timing report ===");
        eprintln!("first walk:        {:?}  (agents_md files: {})", first_walk, agents1.len());
        eprintln!("stat-only turn:    {:?}  (cache hit, no traversal)", stat_only);
        eprintln!("mtime-changed turn:{:?}  (re-walk)", changed_turn);
        eprintln!(
            "cumulative: walks={} stat_hits={} walk_millis={} last_stat_millis={}",
            stats.walks, stats.stat_hits, stats.walk_millis, stats.last_stat_millis
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repo_root_agents_md_body_is_capped_at_12k() {
        // A >12k repo-root AGENTS.md must be head-truncated to ~12k chars in
        // the delivered block (canary parity), never injected whole.
        let dir = std::env::temp_dir().join(format!("zc-cap-{}", std::process::id()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        // A 60k body: well over the 12k cap. The narrative spine is at the HEAD so
        // head-truncation keeps it (the tail X's are dropped).
        let body = format!("PROJECT LAW: the spine survives.\n{}", "X".repeat(60_000));
        std::fs::write(repo.join("AGENTS.md"), &body).unwrap();
        let _ = std::process::Command::new("git")
            .args(["-C", repo.to_str().unwrap(), "init", "-q"])
            .status();

        {
            let mut guard = registry_inner().lock().unwrap_or_else(|p| p.into_inner());
            *guard = None;
        }

        let hook = RulesHook::new(RulesHookConfig {
            enabled: true,
            global_dir: String::new(),
            workspace_dir: ".agents/rules".to_string(),
            repo_discovery: true,
            ..RulesHookConfig::default()
        });

        let ctx = PromptBuildHookContext::new("director", repo.clone(), "sess-cap", "turn-1");
        let (block, _count, _ac, _ac_chars, _manifest_chars, agents_md_chars, _truncated) =
            hook.build_block(&ctx);

        // The delivered AGENTS.md block is bounded by the 12k cap (plus heading
        // + truncation note overhead).
        assert!(
            agents_md_chars <= AGENTS_MD_ROOT_BODY_MAX_CHARS + 512,
            "repo AGENTS.md body is capped (~12k) not injected whole; agents_md_chars={agents_md_chars}"
        );
        assert!(
            block.contains("PROJECT LAW: the spine survives"),
            "the head of the body (where the narrative spine lives) survives truncation"
        );
        assert!(
            block.chars().count() < 20_000,
            "the 60k body is NOT injected whole (block is bounded)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn repo_root_agents_md_delivers_once_then_pointer() {
        // §4.10: the repo-root AGENTS.md body is delivered exactly once per
        // session; later turns emit the one-line pointer instead of the body.
        let dir = std::env::temp_dir().join(format!("zc-agents-root-{}", std::process::id()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let body = "PROJECT LAW: never sleep-poll.\n";
        std::fs::write(repo.join("AGENTS.md"), body).unwrap();
        // Make it a git repo so resolve_repo_root returns the repo root.
        let _ = std::process::Command::new("git")
            .args(["-C", repo.to_str().unwrap(), "init", "-q"])
            .status();

        // Isolate the registry.
        {
            let mut guard = registry_inner().lock().unwrap_or_else(|p| p.into_inner());
            *guard = None;
        }

        let hook = RulesHook::new(RulesHookConfig {
            enabled: true,
            global_dir: String::new(),
            workspace_dir: ".agents/rules".to_string(),
            repo_discovery: true,
            ..RulesHookConfig::default()
        });

        let ctx = PromptBuildHookContext::new("director", repo.clone(), "sess-root", "turn-1");
        match hook
            .before_prompt_build_with_context(&ctx, "SYSTEM".to_string())
            .await
        {
            HookResult::Continue(p) => {
                assert!(p.contains("PROJECT LAW: never sleep-poll"), "first turn delivers body");
            }
            HookResult::Cancel(_) => panic!("must continue"),
        }

        // Second turn: pointer, not body.
        let ctx2 = PromptBuildHookContext::new("director", repo.clone(), "sess-root", "turn-2");
        match hook
            .before_prompt_build_with_context(&ctx2, "SYSTEM".to_string())
            .await
        {
            HookResult::Continue(p) => {
                assert!(
                    p.contains("repo instructions loaded (AGENTS.md"),
                    "later turns emit the pointer"
                );
                assert!(
                    !p.contains("PROJECT LAW: never sleep-poll"),
                    "body is not re-delivered"
                );
            }
            HookResult::Cancel(_) => panic!("must continue"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_block_accounts_agents_md_separately_from_manifest_budget() {
        // The manifest budget scopes index + core ONLY. The once-per-session
        // repo-root AGENTS.md body is budgeted by AGENTS_MD_ROOT_BODY_MAX_CHARS
        // and must NOT count against manifest_budget — otherwise a long
        // AGENTS.md body (≈6,000 chars) would trip manifest_truncated or push
        // `manifest_chars` past budget. The combined `block.len()` (what the
        // log reports as `injected_chars`) legitimately EXCEEDS manifest_budget
        // when an AGENTS.md body is delivered; the accounting must keep the
        // three components separable so a consumer can assert the manifest
        // budget independently.
        let dir = std::env::temp_dir().join(format!("zc-agents-acct-{}", std::process::id()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".agents").join("rules")).unwrap();
        // One rule so the manifest (index) is small; a body under the default
        // cap so nothing is digested away, but large enough to blow a small
        // manifest_budget if it were (wrongly) counted against it.
        std::fs::write(
            repo.join(".agents").join("rules").join("demo-rule.md"),
            "---\nname: demo-rule\ndescription: binds on demo work\n---\nBody.\n",
        )
        .unwrap();
        let agents_body = format!("{}LAW: prove-the-budget.\n", "X".repeat(4000));
        std::fs::write(repo.join("AGENTS.md"), &agents_body).unwrap();
        let _ = std::process::Command::new("git")
            .args(["-C", repo.to_str().unwrap(), "init", "-q"])
            .status();

        // Isolate the registry (the build path reads in-play + delivered sets).
        {
            let mut guard = registry_inner().lock().unwrap_or_else(|p| p.into_inner());
            *guard = None;
        }

        let hook = RulesHook::new(RulesHookConfig {
            enabled: true,
            global_dir: String::new(),
            workspace_dir: ".agents/rules".to_string(),
            repo_discovery: true,
            // A manifest budget generous enough to hold the tiny index but far
            // smaller than the 4,000-char AGENTS.md body.
            manifest_budget: 1024,
            attach_budget: 8000,
            ..RulesHookConfig::default()
        });

        let ctx = PromptBuildHookContext::new("director", repo.clone(), "sess-acct", "turn-1");
        let (block, _count, _ac, _ac_chars, manifest_chars, agents_md_chars, manifest_truncated) =
            hook.build_block(&ctx);

        // The AGENTS.md body WAS delivered (once-per-session root) …
        assert!(
            agents_md_chars > 0,
            "a repo-root AGENTS.md body must be delivered on turn 1 (agents_md_chars={agents_md_chars})"
        );
        assert!(
            block.contains("LAW: prove-the-budget"),
            "the AGENTS.md body text is present in the delivered block"
        );
        // … and it is tracked SEPARATELY: the manifest (index + core) must not
        // be inflated by it. The index itself is tiny, so a correct accounting
        // keeps manifest_chars ≤ budget even though the combined block exceeds
        // it.
        assert!(
            manifest_chars <= 1024,
            "manifest_cars must stay within manifest_budget=1024 (got {manifest_chars}) — the AGENTS.md body is a separate budget"
        );
        // The combined block (what injected_chars reports) DOES exceed the
        // manifest budget — that is expected and correct, but only because the
        // AGENTS.md body is inside it.
        assert!(
            block.len() > 1024,
            "combined block (injected_chars) exceeds manifest_budget when an AGENTS.md body is delivered — expected"
        );
        assert!(
            !manifest_truncated,
            "the small index must NOT be truncated (manifest_truncated=false)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn workspace_root_agents_md_is_excluded() {
        // §4.10: the agent-workspace ROOT AGENTS.md is the engine bootstrap
        // file and never appears in the hook's index.
        let dir = std::env::temp_dir().join(format!("zc-agents-ws-excl-{}", std::process::id()));
        let ws = dir.join("workspace");
        std::fs::create_dir_all(ws.join("pkg")).unwrap();
        std::fs::write(ws.join("AGENTS.md"), "bootstrap root\n").unwrap();
        std::fs::write(ws.join("pkg").join("AGENTS.md"), "nested pkg\n").unwrap();

        let hook = RulesHook::new(RulesHookConfig {
            enabled: true,
            global_dir: String::new(),
            workspace_dir: ".agents/rules".to_string(),
            repo_discovery: false,
            ..RulesHookConfig::default()
        });
        let ctx = PromptBuildHookContext::new("director", ws.clone(), "sess", "turn-1");
        let agents = hook.discover_agents_md(&ctx);
        assert_eq!(
            agents.len(),
            1,
            "only the nested AGENTS.md is discovered; the root is excluded"
        );
        assert!(!agents[0].is_root);
        assert!(agents[0].path.ends_with("pkg/AGENTS.md"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn repo_root_agents_md_in_workspace_subdir_wins_root_classification() {
        // N3b: a git repo cloned INSIDE the agent workspace has its own root
        // AGENTS.md. When both the workspace floor and the inner git repo are
        // resolved repo roots, the repo's root AGENTS.md must be classified as
        // a REPO ROOT (delivered once per session), NOT as a workspace-NESTED
        // AGENTS.md (implicit applyTo attach). The workspace-first nested scan
        // previously poisoned the seen-set so a repo root that is also a
        // workspace subdirectory was mis-classified as nested and never got
        // once-per-session delivery. Tested at the `_from_roots` seam (no
        // process-global in-play registry) so the classification law is
        // deterministic.
        let dir = std::env::temp_dir().join(format!(
            "zc-agents-repo-sub-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let ws = dir.join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        // The workspace has the (empty) workspace scope, mimicking the shipped
        // deployment whose workspace always carries `.agents/rules`.
        std::fs::create_dir_all(ws.join(".agents").join("rules")).unwrap();
        // A nested git repo with its own root AGENTS.md + a nested one.
        let repo = ws.join("checkout");
        std::fs::create_dir_all(repo.join("pkg")).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "REPO ROOT LAW.\n").unwrap();
        std::fs::write(repo.join("pkg").join("AGENTS.md"), "nested pkg law.\n").unwrap();

        let hook = RulesHook::new(RulesHookConfig {
            enabled: true,
            global_dir: String::new(),
            workspace_dir: ".agents/rules".to_string(),
            repo_discovery: true,
            repo_roots_max: 4,
            ..RulesHookConfig::default()
        });

        // Both roots resolved (workspace floor + the inner git repo).
        let agents = hook.discover_agents_md_from_roots(vec![ws.clone(), repo.clone()], &ws);
        // The repo-root AGENTS.md must be classified as a repo root.
        let root = agents
            .iter()
            .find(|a| a.path.to_string_lossy().ends_with("/checkout/AGENTS.md"));
        assert!(root.is_some(), "the checkout's root AGENTS.md must be discovered; got {agents:?}");
        let root = root.unwrap();
        assert!(root.is_root, "repo-root AGENTS.md must be is_root=true; got {agents:?}");
        assert!(
            root.scope.starts_with("repo:"),
            "repo-root AGENTS.md must carry a repo: scope, got {}",
            root.scope
        );
        // The nested pkg AGENTS.md is still discovered as nested.
        assert!(
            agents.iter().any(|a| {
                a.path.to_string_lossy().ends_with("/checkout/pkg/AGENTS.md") && !a.is_root
            }),
            "nested AGENTS.md must remain nested; got {agents:?}"
        );
        // No duplicate: each file once.
        let checkout_count = agents
            .iter()
            .filter(|a| a.path.to_string_lossy().ends_with("/checkout/AGENTS.md"))
            .count();
        assert_eq!(checkout_count, 1, "no duplicate listing; got {agents:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn nested_agents_md_attaches_on_in_play_dir_match() {
        // §4.10: a nested AGENTS.md attaches its body when a modified in-play
        // path is under its directory (implicit applyTo: <dir>/**).
        let dir = std::env::temp_dir().join(format!("zc-agents-nested-{}", std::process::id()));
        let ws = dir.join("workspace");
        let sub = ws.join("pkg");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("AGENTS.md"), "PKG RULE: only touch pkg files.\n").unwrap();

        let hook = RulesHook::new(RulesHookConfig {
            enabled: true,
            global_dir: String::new(),
            workspace_dir: ".agents/rules".to_string(),
            repo_discovery: false,
            ..RulesHookConfig::default()
        });

        {
            let mut guard = registry_inner().lock().unwrap_or_else(|p| p.into_inner());
            *guard = None;
        }
        // An in-play path under the nested AGENTS.md's dir.
        record_in_play_path("director", "sess-nested", "pkg/main.rs", 50);

        let ctx = PromptBuildHookContext::new("director", ws.clone(), "sess-nested", "turn-1");
        match hook
            .before_prompt_build_with_context(&ctx, "SYSTEM".to_string())
            .await
        {
            HookResult::Continue(p) => {
                assert!(
                    p.contains("PKG RULE: only touch pkg files"),
                    "nested AGENTS.md body attaches on in-play dir match"
                );
            }
            HookResult::Cancel(_) => panic!("must continue"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn idempotent_injection_yields_one_block() {
        // I1: running the hook twice on one prompt yields exactly one
        // `<!-- rules-hook:v1 -->` marker and one block (no double injection).
        let dir = std::env::temp_dir().join(format!("zc-agents-idem-{}", std::process::id()));
        let ws = dir.join("workspace");
        let rules_dir = ws.join(".agents").join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("idem-rule.md"),
            "---\nname: idem-rule\ndescription: when idempotent work is done\n---\nBody.\n",
        )
        .unwrap();

        let hook = RulesHook::new(RulesHookConfig {
            enabled: true,
            global_dir: String::new(),
            workspace_dir: ".agents/rules".to_string(),
            repo_discovery: false,
            ..RulesHookConfig::default()
        });

        let ctx = PromptBuildHookContext::new("director", ws.clone(), "sess-idem", "turn-1");
        let first = match hook
            .before_prompt_build_with_context(&ctx, "SYSTEM".to_string())
            .await
        {
            HookResult::Continue(p) => p,
            HookResult::Cancel(_) => panic!("must continue"),
        };
        let second = match hook
            .before_prompt_build_with_context(&ctx, first)
            .await
        {
            HookResult::Continue(p) => p,
            HookResult::Cancel(_) => panic!("must continue"),
        };

        assert_eq!(
            second.matches(RULES_HOOK_MARKER).count(),
            1,
            "exactly one marker after two builds"
        );
        assert_eq!(
            second.matches("<instructions>").count(),
            1,
            "exactly one instruction block"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Scenario reproductions (U1 / N2 / N4) ──────────────────────────────
    //
    // These pin the three defects the context-layers scenario suite caught —
    // all at deterministic seams (no process-global registry, no git
    // subprocess) so they are parallel-safe.

    fn mk_rule(name: &str, scope: &str, depth: usize, desc: &str, body: &str,
               apply_to: Option<&str>, agents: Option<&str>) -> Rule {
        Rule {
            path: PathBuf::from(format!("/{scope}/{name}.md")),
            frontmatter: RuleFrontmatter {
                name: name.to_string(),
                description: desc.to_string(),
                apply_to: apply_to.map(|s| s.to_string()).into_iter().collect(),
                agents: agents.map(|s| vec![s.to_string()]).unwrap_or_default(),
                core: false,
            },
            scope: scope.to_string(),
            mtime_secs: 0,
            body: body.to_string(),
            depth,
        }
    }

    #[test]
    fn repro_n4_deepest_wins_depth_and_body() {
        // N4: the same rule name at two depths in ONE scope. Dedup keeps the
        // deepest (highest depth) only; its description AND body win; the
        // shallow body is deduped out. This is the load→dedup→attach law.
        let hook = RulesHook::new(RulesHookConfig::default());
        let shallow = mk_rule("shared", "repo:/r", 1, "N4-SHALLOW-DESC shallow shared rule", "N4-SHALLOW-BODY.\n", Some("**/*.n4"), None);
        let deep = mk_rule("shared", "repo:/r", 3, "N4-DEEP-DESC deep shared rule", "N4-DEEP-BODY.\n", Some("**/*.n4"), None);

        let deduped = hook.dedup_by_name(vec![shallow.clone(), deep.clone()]);
        assert_eq!(deduped.len(), 1, "one 'shared' entry survives");
        let winner = &deduped[0];
        assert_eq!(winner.depth, 3, "deepest wins");
        assert!(winner.frontmatter.description.contains("N4-DEEP-DESC"), "deepest description");
        assert!(winner.body.contains("N4-DEEP-BODY"), "deepest body");

        // And that same winner is the ONLY rule that attaches for an in-play `.n4`.
        let (in_play, _agent) = partition_attach_rules(&deduped, "researcher", &["n4-repo/f.n4".to_string()]);
        assert_eq!(in_play.len(), 1, "the surviving rule attaches in-play");
        assert!(in_play[0].body.contains("N4-DEEP-BODY"), "the DEEP body attaches, never shallow");
    }

    #[test]
    fn repro_n4_depth_is_scope_root_relative() {
        // The depth driving deepest-wins must be measured against the SCOPE
        // root (the `.agents/rules` dir), not the process CWD or an absolute
        // prefix — a scope-relative path resolved against the wrong root yields
        // equal (or inverted) depths and breaks dedup. Pin it at the load seam.
        let hook = RulesHook::new(RulesHookConfig::default());
        let dir = std::env::temp_dir().join(format!("zc-n4-depth-{}", std::process::id()));
        let scope_root = dir.join("rules");
        let shallow_p = scope_root.join("shared.md");
        let deep_p = scope_root.join("sub").join("deep").join("shared.md");
        std::fs::create_dir_all(deep_p.parent().unwrap()).unwrap();
        let fm = |desc: &str| format!("---\nname: shared\ndescription: {desc}\n---\nbody\n");
        std::fs::write(&shallow_p, fm("shallow")).unwrap();
        std::fs::write(&deep_p, fm("deep")).unwrap();

        let s = hook.load_rule(&shallow_p, "repo:/r".into(), &scope_root).unwrap();
        let d = hook.load_rule(&deep_p, "repo:/r".into(), &scope_root).unwrap();
        assert_eq!(s.depth, 1, "shared.md at scope root is depth 1");
        assert_eq!(d.depth, 3, "sub/deep/shared.md is depth 3");
        assert!(d.depth > s.depth, "nested path is deeper");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repro_n2_scope_root_relative_apply_to_attaches() {
        // N2: a nested repo rule with `applyTo: **/*.gradle` matches a path
        // that includes the repo dirname (`n2-repo/app/build.gradle`) because
        // the bare pattern is `**/`-prefixed — the nested rule must land in the
        // in-play partition, not be missed.
        let rule = mk_rule("n2-anchored", "repo:/r", 1, "binds on nested path", "N2-ANCHORED-BODY marker.\n", Some("**/*.gradle"), None);
        let rules = vec![rule];
        let (in_play, _agent) = partition_attach_rules(&rules, "gandalf", &["n2-repo/app/build.gradle".to_string()]);
        assert_eq!(in_play.len(), 1, "nested applyTo matches scope-root-relative path");
        assert!(in_play[0].body.contains("N2-ANCHORED-BODY"));
    }

    #[test]
    fn repro_u1_in_play_partitions_before_agent_scoped() {
        // U1: the in-play rule (declarative-pod-content, matched by an edit to
        // a chart file) must survive even when an always-on agent-scoped rule
        // is large. The ordering law is that in-play rules attach FIRST, so
        // agent-scoped bulk cannot starve the specific edit-triggered law to
        // zero — the regression the scenario caught.
        let chart_rule = mk_rule("declarative-pod-content", "repo:/r", 1, "binds when touching pod content", "U1-CHART-BODY declarative pod content law.\n", Some("ansible/roles/cheknet.zeroclaw/files/charts/zeroclaw/**"), None);
        let coder_rule = mk_rule("coder-discipline-core", "repo:/r", 1, "binds for the coder agent", &format!("{}\n", "X".repeat(9000)), None, Some("coder"));

        let rules = vec![chart_rule.clone(), coder_rule.clone()];
        let (in_play, agent) = partition_attach_rules(&rules, "coder", &["debops-checknet/ansible/roles/cheknet.zeroclaw/files/charts/zeroclaw/templates/foo.j2".to_string()]);
        assert_eq!(agent.len(), 1, "coder rule is agent-scoped");
        assert_eq!(in_play.len(), 1, "chart rule is in-play matched");
        assert!(in_play[0].body.contains("U1-CHART-BODY"), "the in-play body is preserved in its partition");

        // The hook's attach ordering concatenates in-play FIRST, so the chart
        // body precedes the (oversized) agent-scoped body — the exact law that
        // prevents in-play starvation. Assert the concatenated order.
        let mut ordered: Vec<&Rule> = in_play.clone();
        ordered.extend(agent.iter().copied());
        assert_eq!(ordered.len(), 2);
        assert!(
            ordered[0].body.contains("U1-CHART-BODY"),
            "in-play rule is FIRST in attachment order, never starved"
        );
        assert_eq!(ordered[1].name(), "coder-discipline-core", "agent-scoped rule follows");
    }
}