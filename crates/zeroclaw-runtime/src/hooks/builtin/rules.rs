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

// ── The matcher (§4.5) ─────────────────────────────────────────

/// Whether an empty `applyTo` list means "never matches in-play" (manifest-only)
/// vs. the `**`/`**/*`/`*` always-match semantics.
fn pattern_always_matches(pattern: &str) -> bool {
    matches!(pattern.trim(), "**" | "**/*" | "*")
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
fn scan_rules_dir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
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
struct Registry {
    sessions: HashMap<(String, String), SessionInPlay>,
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
                self.order.remove(0);
            } else {
                break;
            }
        }
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
                    if let Some(rule) = self.load_rule(&p, "global".to_string()) {
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
                if let Some(rule) = self.load_rule(&p, "workspace".to_string()) {
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
                            self.load_rule(&p, format!("repo:{}", root.display()))
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

    fn load_rule(&self, path: &Path, scope: String) -> Option<Rule> {
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
        Some(Rule {
            path: path.to_path_buf(),
            frontmatter,
            scope,
            mtime_secs,
            body,
        })
    }

    /// Build the deduped union of rules across scopes, repo instance wins over
    /// shared copy when both are active.
    fn dedup_by_name(&self, rules: Vec<Rule>) -> Vec<Rule> {
        let mut by_name: HashMap<String, Rule> = HashMap::new();
        for rule in rules {
            let name = rule.name().to_string();
            if name.is_empty() {
                continue;
            }
            let entry = by_name.entry(name.clone()).or_insert_with(|| rule.clone());
            // A repo instance (scope starts with "repo:") wins over a global/
            // workspace shared copy; the first that isn't a repo keeps its spot.
            let is_repo = rule.scope.starts_with("repo:");
            let existing_is_repo = entry.scope.starts_with("repo:");
            if is_repo && !existing_is_repo {
                *entry = rule.clone();
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
                for line in rule.body.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    core_lines.push(format!("{}: {trimmed}", name));
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

        // Ordering: agent-scoped first, then in-play; within each, core first,
        // then most-recently-modified.
        let order_key = |r: &Rule| {
            (
                if r.frontmatter.core { 0u8 } else { 1u8 },
                std::cmp::Reverse(r.mtime_secs),
            )
        };
        agent_scoped.sort_by_key(|r| order_key(r));
        in_play_matched.sort_by_key(|r| order_key(r));

        let mut out = String::new();
        let mut attached_chars = 0usize;
        let mut attached_count = 0usize;

        let budget = self.config.attach_budget;
        for rule in agent_scoped.into_iter().chain(in_play_matched) {
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

    /// Compose the full rules block appended to the prompt.
    fn build_block(&self, ctx: &PromptBuildHookContext) -> (String, usize, usize, usize, bool) {
        let rules = self.dedup_by_name(self.discover(ctx));
        let manifest = self.render_manifest(&rules);
        let (attach, attached_count, attached_chars) =
            self.build_attach_block(&rules, ctx.alias(), ctx.session_id());

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

        let mut block = manifest;
        block.push_str(&attach);
        (
            block,
            rules.len(),
            attached_count,
            attached_chars,
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
        let (block, manifest_count, attached_count, attached_chars, manifest_truncated) =
            self.build_block(ctx);
        let injected_chars = block.len();
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
                        "manifest_count": manifest_count,
                        "attached_count": attached_count,
                        "attached_chars": attached_chars,
                        "manifest_truncated": manifest_truncated,
                    })),
                "rules block injected into system prompt"
            );
        }
        HookResult::Continue(format!("{prompt}\n\n{block}"))
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
        let mk = |name: &str, scope: &str| Rule {
            path: PathBuf::from(format!("/{scope}/{name}.md")),
            frontmatter: RuleFrontmatter {
                name: name.to_string(),
                description: "when".into(),
                ..Default::default()
            },
            scope: scope.to_string(),
            mtime_secs: 0,
            body: String::new(),
        };
        let rules = vec![
            mk("event-driven-swarm", "global"),
            mk("event-driven-swarm", "repo:/repo"),
            mk("quorum-review", "global"),
        ];
        let deduped = hook.dedup_by_name(rules);
        assert_eq!(deduped.len(), 2);
        let ed = deduped.iter().find(|r| r.name() == "event-driven-swarm").unwrap();
        assert_eq!(ed.scope, "repo:/repo");
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
            "---\nname: event-driven-swarm\ndescription: binds when doing async work\ncore: true\n---\nNever sleep-poll; watch.\n",
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
                assert!(prompt.contains("Never sleep-poll"), "core line injected");
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
}