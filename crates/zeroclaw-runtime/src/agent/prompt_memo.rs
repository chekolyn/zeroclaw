//! Cross-child system-prompt memoization keyed on the child's OWN render
//! inputs.
//!
//! Delegate children and cross-agent SOP steps each render a fresh system
//! prompt from the TARGET agent's own identity — never from the parent's
//! (parent-prompt reuse would lie about the child's tool catalog, identity
//! mechanism, and approval policy). Two children of the same identity then
//! render byte-identical system rows, which is exactly what a provider's
//! prefix cache needs: the memo freezes those bytes so repeated children
//! (same alias, same render inputs) hit the provider's prompt cache instead
//! of re-prefilling tens of thousands of tokens per dispatch.
//!
//! The memo NEVER serves one identity's bytes to another: the key carries the
//! full identity key of the render — alias, model, provider, the effective
//! tool surface (including the step's excluded-tool set, the documented
//! per-step pitfall), skills, autonomy, shell profile, workspace, the
//! content fingerprint of every workspace file the render reads, and the
//! per-shape prompt knobs. Any member that changes produces a different key
//! and therefore a fresh render.
//!
//! Storage is the process-bounded [`PROMPT_MEMO`] map (whole-table flush on
//! overflow, the rules-hook `DISCOVERY_CACHE` pattern): per-process,
//! memory-bounded, and cheap to refill (a miss re-renders).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Which render pipeline produced the prompt. Two shapes build structurally
/// different prompts from the same agent config (section sets and ordering
/// differ), so the shape is part of the key: a delegate child and an owned
/// SOP step of the same alias must never cross-serve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PromptRenderShape {
    /// The delegate tool's enriched child system prompt
    /// (`DelegateTool::build_enriched_system_prompt_from_config`).
    DelegateChild,
    /// The cross-agent SOP step's own system prompt
    /// (`build_owned_step_system_prompt` → `build_system_prompt_for_turn`).
    OwnedStep,
}

/// The full identity key of one child system-prompt render. Every member is
/// load-bearing: omitting one turns the memo into a stale-prompt or
/// cross-identity bug (the exact failure class parent-prompt reuse has).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PromptMemoKey {
    pub(crate) shape: PromptRenderShape,
    pub(crate) agent_alias: String,
    pub(crate) model: String,
    /// The provider identity the render derives its native-tool posture from.
    pub(crate) provider_alias: String,
    /// Hash of the effective tool surface: tool names + descriptions, the
    /// excluded-tool set, the activated-tool state (hidden builtins + deferred
    /// section), and the MCP/deferred prompt section.
    pub(crate) tool_surface_hash: u64,
    pub(crate) skills_hash: u64,
    /// Autonomy policy the render narrates (risk profile / autonomy level +
    /// always-ask list).
    pub(crate) autonomy_hash: u64,
    pub(crate) shell_hash: u64,
    /// The workspace whose files the render reads (personality + skills +
    /// workspace section).
    pub(crate) workspace_dir: PathBuf,
    /// The workspace whose appended identity files (AGENTS.md/SOUL.md/…)
    /// the delegate path reads; `None` when it equals `workspace_dir`.
    pub(crate) identity_workspace_dir: Option<PathBuf>,
    /// Content fingerprint of every workspace file the render reads from disk.
    pub(crate) bootstrap_hash: u64,
    /// Per-shape prompt knobs that change bytes (parsing/injection modes,
    /// compact flags, char caps, tool-call visibility, native-spec posture).
    pub(crate) knobs_hash: u64,
    /// Day-granularity render stamp for shapes whose system row still embeds
    /// the current date (the owned-step path's `## Current Date` section).
    /// `None` for date-free shapes (the delegate child, whose date moved to
    /// the child's user row).
    pub(crate) render_day: Option<String>,
}

/// Stable-within-process hasher for key members. `std::hash::DefaultHasher`
/// keys are randomized per process but constant within one, which is all an
/// in-process memo needs.
#[derive(Default)]
pub(crate) struct KeyHasher(std::hash::DefaultHasher);

impl KeyHasher {
    pub(crate) fn new() -> Self {
        Self(std::hash::DefaultHasher::new())
    }

    pub(crate) fn str(&mut self, value: &str) {
        std::hash::Hash::hash(&value, &mut self.0);
    }

    pub(crate) fn usize(&mut self, value: usize) {
        std::hash::Hash::hash(&value, &mut self.0);
    }

    pub(crate) fn bool(&mut self, value: bool) {
        std::hash::Hash::hash(&value, &mut self.0);
    }

    pub(crate) fn finish(self) -> u64 {
        std::hash::Hasher::finish(&self.0)
    }
}

/// Hash an ordered string list (order matters: the render emits in order).
pub(crate) fn hash_string_list(values: impl IntoIterator<Item = impl AsRef<str>>) -> u64 {
    let mut hasher = KeyHasher::new();
    for value in values {
        hasher.str(value.as_ref());
    }
    hasher.finish()
}

/// Hash a set where iteration order is unstable (sort before hashing so two
/// equal sets always agree).
pub(crate) fn hash_string_set(values: impl IntoIterator<Item = impl AsRef<str>>) -> u64 {
    let mut sorted: Vec<String> = values.into_iter().map(|v| v.as_ref().to_string()).collect();
    sorted.sort();
    hash_string_list(sorted)
}

/// Content fingerprint of `filenames` under `dir`, in the given order. A
/// file's absence is part of the fingerprint (removals matter as much as
/// edits). Content, not mtime: an editor that rewrites without touching
/// mtime still invalidates, and a same-mtime different-content write is the
/// stale-serving failure class this fingerprint exists to prevent.
pub(crate) fn files_fingerprint(dir: &Path, filenames: &[&std::path::Path]) -> u64 {
    let mut hasher = KeyHasher::new();
    for filename in filenames {
        hasher.str(&filename.to_string_lossy());
        match std::fs::read_to_string(dir.join(filename)) {
            Ok(content) => hasher.str(&content),
            Err(_) => hasher.str("\u{0}absent"),
        }
    }
    hasher.finish()
}

const PROMPT_MEMO_CAPACITY: usize = 64;

/// The process-global prompt memo, keyed by [`PromptMemoKey`]. Bounded by
/// [`PROMPT_MEMO_CAPACITY`] with whole-table flush on overflow — identities
/// are few, a flush costs one re-render per identity, and the render is far
/// cheaper than the re-prefill it prevents.
static PROMPT_MEMO: Mutex<Option<HashMap<PromptMemoKey, Arc<String>>>> = Mutex::new(None);

// Hit/miss telemetry: production counters (the A1 memo was previously
// observable only under `cfg(test)`; the 2026-10-28 tuning cycle exposed them
// on the rules row so prod memo behavior is readable from traces).
static MEMO_HITS: AtomicU64 = AtomicU64::new(0);
static MEMO_MISSES: AtomicU64 = AtomicU64::new(0);

/// Process-global `(hits, misses)` snapshot for the prompt memo. Emitted on
/// the rules hook's injection row alongside the body-cache counters.
pub(crate) fn prompt_memo_stats() -> (u64, u64) {
    (MEMO_HITS.load(Ordering::SeqCst), MEMO_MISSES.load(Ordering::SeqCst))
}

#[cfg(test)]
pub(crate) static MEMO_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn reset_prompt_memo() {
    MEMO_HITS.store(0, Ordering::SeqCst);
    MEMO_MISSES.store(0, Ordering::SeqCst);
    PROMPT_MEMO
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
}

/// Serve the memoized prompt for `key`, if one is stored. The hit/miss
/// counters are PRODUCTION counters (the rules hook's injection row reads
/// [`prompt_memo_stats`]): they increment unconditionally — a
/// `#[cfg(test)]`-gated increment compiles away in production builds and
/// left the rules row reading 0/0 forever (measured live 2026-10-28 on the
/// canary battery across three byte-identical coder children).
pub(crate) fn memoized_system_prompt(key: &PromptMemoKey) -> Option<Arc<String>> {
    let hit = PROMPT_MEMO
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .and_then(|map| map.get(key).cloned());
    if hit.is_some() {
        MEMO_HITS.fetch_add(1, Ordering::SeqCst);
    } else {
        MEMO_MISSES.fetch_add(1, Ordering::SeqCst);
    }
    hit
}

/// Store a freshly rendered prompt under `key` (flushing the table first when
/// at capacity) and return it shared. The store is counted as a miss: the
/// render path's observable shape is lookup-miss + store for the first child
/// of an identity, and a bare lookup hit for every repeated child.
pub(crate) fn store_system_prompt(key: PromptMemoKey, prompt: String) -> Arc<String> {
    let shared = Arc::new(prompt);
    let mut guard = PROMPT_MEMO
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() >= PROMPT_MEMO_CAPACITY {
        *map = HashMap::new();
    }
    map.insert(key, Arc::clone(&shared));
    MEMO_MISSES.fetch_add(1, Ordering::SeqCst);
    drop(guard);
    shared
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(shape: PromptRenderShape, alias: &str, workspace: &Path) -> PromptMemoKey {
        PromptMemoKey {
            shape,
            agent_alias: alias.to_string(),
            model: "m".into(),
            provider_alias: "p".into(),
            tool_surface_hash: 1,
            skills_hash: 1,
            autonomy_hash: 1,
            shell_hash: 1,
            workspace_dir: workspace.to_path_buf(),
            identity_workspace_dir: None,
            bootstrap_hash: 1,
            knobs_hash: 1,
            render_day: None,
        }
    }

    #[test]
    fn store_then_hit_serves_identical_bytes_and_counts() {
        let _guard = MEMO_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_prompt_memo();
        let workspace = std::env::temp_dir();
        let k = key(PromptRenderShape::DelegateChild, "alpha", &workspace);
        assert!(memoized_system_prompt(&k).is_none(), "empty memo misses");
        store_system_prompt(k.clone(), "prompt-bytes".to_string());
        let hit = memoized_system_prompt(&k).expect("stored key must hit");
        assert_eq!(&*hit, "prompt-bytes");
        let (hits, _) = prompt_memo_stats();
        assert_eq!(hits, 1);
    }

    /// The counter CONTRACT the production rules row reads: the first
    /// render of an identity costs exactly two misses (the lookup miss plus
    /// the store recorded as a miss), and a second render of the SAME key
    /// is exactly one hit with no further misses. The production counters
    /// must move through the same unconditional code in every build — a
    /// `#[cfg(test)]`-gated increment compiles away in production and leaves
    /// the rules row reading 0/0 forever (measured live on the 2026-10-28
    /// canary battery across three byte-identical coder children).
    #[test]
    fn counters_track_first_render_as_miss_and_second_as_hit() {
        let _guard = MEMO_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_prompt_memo();
        let workspace = std::env::temp_dir();
        let k = key(PromptRenderShape::DelegateChild, "gamma", &workspace);

        // First render of the identity: lookup miss + store-as-miss.
        assert!(memoized_system_prompt(&k).is_none());
        store_system_prompt(k.clone(), "bytes-v1".to_string());
        let (hits_after_first, misses_after_first) = prompt_memo_stats();
        assert_eq!(hits_after_first, 0, "a first render cannot hit");
        assert_eq!(
            misses_after_first, 2,
            "the first render must record exactly the lookup miss and the store miss"
        );

        // Second render of the SAME key: exactly one hit, no new misses.
        assert!(memoized_system_prompt(&k).is_some());
        let (hits_after_second, misses_after_second) = prompt_memo_stats();
        assert_eq!(
            hits_after_second, 1,
            "the repeated same-key render must be exactly one hit"
        );
        assert_eq!(
            misses_after_second, misses_after_first,
            "a hit must not record a miss"
        );
    }

    #[test]
    fn different_shape_or_alias_never_cross_serves() {
        let _guard = MEMO_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_prompt_memo();
        let workspace = std::env::temp_dir();
        let delegate = key(PromptRenderShape::DelegateChild, "alpha", &workspace);
        let owned = key(PromptRenderShape::OwnedStep, "alpha", &workspace);
        store_system_prompt(delegate, "delegate-bytes".to_string());
        assert!(
            memoized_system_prompt(&owned).is_none(),
            "a different render shape must never serve the other shape's bytes"
        );
        let other_alias = key(PromptRenderShape::DelegateChild, "beta", &workspace);
        assert!(
            memoized_system_prompt(&other_alias).is_none(),
            "a different alias must never cross-serve"
        );
    }

    #[test]
    fn capacity_overflow_flushes_whole_table() {
        let _guard = MEMO_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_prompt_memo();
        let workspace = std::env::temp_dir();
        for i in 0..PROMPT_MEMO_CAPACITY + 8 {
            let mut k = key(PromptRenderShape::DelegateChild, "alpha", &workspace);
            k.knobs_hash = i as u64;
            store_system_prompt(k, format!("bytes-{i}"));
        }
        // The table flushed: the first entry is gone, and the map is bounded.
        let mut first = key(PromptRenderShape::DelegateChild, "alpha", &workspace);
        first.knobs_hash = 0;
        assert!(memoized_system_prompt(&first).is_none());
        let guard = PROMPT_MEMO.lock().unwrap_or_else(|p| p.into_inner());
        let len = guard.as_ref().map_or(0, |m| m.len());
        assert!(len <= PROMPT_MEMO_CAPACITY, "memo must stay bounded ({len})");
    }

    #[test]
    fn files_fingerprint_tracks_content_and_absence() {
        let dir = std::env::temp_dir().join(format!("zc_memo_fp_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let names = [std::path::Path::new("AGENTS.md")];
        let absent = files_fingerprint(&dir, &names);
        std::fs::write(dir.join("AGENTS.md"), "identity v1").unwrap();
        let v1 = files_fingerprint(&dir, &names);
        assert_ne!(absent, v1, "appearance must change the fingerprint");
        std::fs::write(dir.join("AGENTS.md"), "identity v2").unwrap();
        let v2 = files_fingerprint(&dir, &names);
        assert_ne!(v1, v2, "content edits must change the fingerprint");
        assert_eq!(v2, files_fingerprint(&dir, &names), "unchanged content is stable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hash_string_set_is_order_insensitive() {
        assert_eq!(
            hash_string_set(["a", "b"]),
            hash_string_set(["b", "a"]),
            "a set hashed for a key must not depend on iteration order"
        );
        assert_ne!(hash_string_set(["a", "b"]), hash_string_set(["a"]));
    }
}
