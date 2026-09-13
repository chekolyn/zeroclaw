use anyhow::{Context, Result, bail};
use directories::UserDirs;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use zeroclaw_config::schema::Config;
use zeroclaw_memory::{self, Memory, MemoryCategory};

#[derive(Debug, Clone)]
struct SourceEntry {
    key: String,
    content: String,
    category: MemoryCategory,
    /// V3 schema fields preserved when the source sqlite has them.
    /// The postgres backend stores `agent_id` + `session_id` (its INSERT
    /// sets those columns); `namespace` + `importance` are accepted by the
    /// `store_with_agent` signature and preserved for backends that store
    /// them (the postgres INSERT currently omits those two columns — a
    /// separate backend limitation, not migration-specific).
    agent_id: Option<String>,
    session_id: Option<String>,
    namespace: Option<String>,
    importance: Option<f64>,
}

#[derive(Debug, Default)]
struct MigrationStats {
    from_sqlite: usize,
    from_markdown: usize,
    imported: usize,
    skipped_unchanged: usize,
    renamed_conflicts: usize,
}

pub async fn migrate_openclaw_memory(
    config: &Config,
    source_workspace: Option<PathBuf>,
    dry_run: bool,
    reindex: bool,
) -> Result<()> {
    let source_workspace = resolve_openclaw_workspace(source_workspace)?;
    if !source_workspace.exists() {
        bail!(
            "OpenClaw workspace not found at {}. Pass --source <path> if needed.",
            source_workspace.display()
        );
    }

    if paths_equal(&source_workspace, &config.data_dir) {
        bail!("Source workspace matches current ZeroClaw workspace; refusing self-migration");
    }

    let mut stats = MigrationStats::default();
    let entries = collect_source_entries(&source_workspace, &mut stats)?;

    if entries.is_empty() {
        println!(
            "No importable memory found in {}",
            source_workspace.display()
        );
        println!("Checked for: memory/brain.db, MEMORY.md, memory/*.md");
        return Ok(());
    }

    if dry_run {
        println!("🔎 Dry run: OpenClaw migration preview");
        println!("  Source: {}", source_workspace.display().to_string());
        println!("  Target: {}", config.data_dir.display().to_string());
        println!("  Candidates: {}", entries.len());
        println!("    - from sqlite:   {}", stats.from_sqlite);
        println!("    - from markdown: {}", stats.from_markdown);
        println!();
        if reindex {
            println!("Reindex requested: indexes would be rebuilt after import.");
        }
        println!("Run without --dry-run to import these entries.");
        return Ok(());
    }

    if let Some(backup_dir) = backup_target_memory(&config.data_dir)? {
        println!("🛟 Backup created: {}", backup_dir.display().to_string());
    }

    let memory = target_memory_backend(config)?;

    // Map source sqlite agent_ids → live target agent_ids by alias. The source
    // memories table stores the SOURCE agent UUID; the target backend assigns
    // its OWN agent id (e.g. postgres `ensure_agent_uuid` mints a UUID keyed by
    // alias). Without this map, imported memories would reference the source
    // UUID — violating the target's `agents` FK and/or orphaning the memories
    // from the live agent's recall. Source agents without an aliases-table
    // entry (or whose ensure fails) fall back to the target's default agent
    // (agent_id = None in the store call below).
    let source_db = source_workspace.join("memory").join("brain.db");
    let agent_map = build_agent_alias_map(memory.as_ref(), &source_db).await?;

    for (idx, entry) in entries.into_iter().enumerate() {
        let mut key = entry.key.trim().to_string();
        if key.is_empty() {
            key = format!("openclaw_{idx}");
        }

        if let Some(existing) = memory.get(&key).await? {
            if existing.content.trim() == entry.content.trim() {
                stats.skipped_unchanged += 1;
                continue;
            }

            let renamed = next_available_key(memory.as_ref(), &key).await?;
            key = renamed;
            stats.renamed_conflicts += 1;
        }

        // Resolve the source agent_id → live target agent_id via the alias map.
        // A miss (legacy source with agent_id but no agents table, or an ensure
        // failure) yields None → the target's default agent. The memory is still
        // imported (never dropped), only its per-agent attribution is softened.
        let live_agent_id = entry
            .agent_id
            .as_deref()
            .and_then(|src| agent_map.get(src).map(|s| s.as_str()));

        memory
            .store_with_agent(
                &key,
                &entry.content,
                entry.category,
                entry.session_id.as_deref(),
                entry.namespace.as_deref(),
                entry.importance,
                live_agent_id,
            )
            .await?;
        stats.imported += 1;
    }

    println!("✅ OpenClaw memory migration complete");
    println!("  Source: {}", source_workspace.display().to_string());
    println!("  Target: {}", config.data_dir.display().to_string());
    println!("  Imported:         {}", stats.imported);
    println!("  Skipped unchanged:{}", stats.skipped_unchanged);
    println!("  Renamed conflicts:{}", stats.renamed_conflicts);
    println!("  Source sqlite rows:{}", stats.from_sqlite);
    println!("  Source markdown:   {}", stats.from_markdown);
    if reindex {
        // The import above deliberately goes through a NoopEmbedding-backed
        // handle for speed, so reindex through a second handle with the
        // configured embedder wired in - the same construction `zeroclaw
        // memory reindex` uses - otherwise the backfill could never compute
        // an embedding regardless of the operator's embedding config.
        drop(memory);
        let reindex_memory = reindex_memory_backend(config)?;
        let reembedded = reindex_memory.reindex().await?;
        println!("  Reindexed:         yes ({reembedded} embeddings backfilled; FTS rebuilt)");
    }

    Ok(())
}

fn target_memory_backend(config: &Config) -> Result<Box<dyn Memory>> {
    let backend = zeroclaw_memory::backend_kind_from_dotted(&config.memory.backend);
    if zeroclaw_memory::classify_memory_backend(&backend)
        == zeroclaw_memory::MemoryBackendKind::Qdrant
    {
        bail!(crate::i18n::get_required_cli_string(
            "cli-migrate-openclaw-qdrant-unsupported"
        ));
    }
    zeroclaw_memory::create_memory_for_migration(config)
}

/// Memory handle for the post-import `--reindex` pass, with the configured
/// embedder resolved and wired in. Mirrors `zeroclaw memory reindex`
/// (`create_memory_with_embedder` in the CLI): same storage resolution, same
/// embedding-route handling, so `migrate openclaw --reindex` is equivalent to
/// running the standalone reindex command right after the import.
fn reindex_memory_backend(config: &Config) -> Result<Box<dyn Memory>> {
    zeroclaw_memory::create_memory_with_storage_and_routes(
        &config.memory,
        &config.embedding_routes,
        config.resolve_active_storage(),
        &config.data_dir,
        None,
        Some(&config.providers.models),
    )
}

fn collect_source_entries(
    source_workspace: &Path,
    stats: &mut MigrationStats,
) -> Result<Vec<SourceEntry>> {
    let mut entries = Vec::new();

    let sqlite_path = source_workspace.join("memory").join("brain.db");
    let sqlite_entries = read_openclaw_sqlite_entries(&sqlite_path)?;
    stats.from_sqlite = sqlite_entries.len();
    entries.extend(sqlite_entries);

    let markdown_entries = read_openclaw_markdown_entries(source_workspace)?;
    stats.from_markdown = markdown_entries.len();
    entries.extend(markdown_entries);

    // De-dup exact duplicates to make re-runs deterministic.
    let mut seen = HashSet::new();
    entries.retain(|entry| {
        let sig = format!("{}\u{0}{}\u{0}{}", entry.key, entry.content, entry.category);
        seen.insert(sig)
    });

    Ok(entries)
}

fn read_openclaw_sqlite_entries(db_path: &Path) -> Result<Vec<SourceEntry>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Failed to open source db {}", db_path.display().to_string()))?;

    let table_exists: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='memories' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;

    if table_exists.is_none() {
        return Ok(Vec::new());
    }

    let columns = table_columns(&conn, "memories")?;
    let key_expr = pick_column_expr(&columns, &["key", "id", "name"], "CAST(rowid AS TEXT)");
    let Some(content_expr) =
        pick_optional_column_expr(&columns, &["content", "value", "text", "memory"])
    else {
        bail!("OpenClaw memories table found but no content-like column was detected");
    };
    let category_expr = pick_column_expr(&columns, &["category", "kind", "type"], "'core'");
    // V3 schema fields (optional — old sources may lack them). When present,
    // preserve them so a sqlite→postgres migration keeps per-agent / per-session
    // context instead of flattening every row to the `default` agent.
    let agent_id_expr = pick_optional_column_expr(&columns, &["agent_id"]).unwrap_or("NULL".to_string());
    let session_id_expr = pick_optional_column_expr(&columns, &["session_id", "session"]).unwrap_or("NULL".to_string());
    let namespace_expr = pick_optional_column_expr(&columns, &["namespace"]).unwrap_or("NULL".to_string());
    let importance_expr = pick_optional_column_expr(&columns, &["importance"]).unwrap_or("NULL".to_string());

    let sql = format!(
        "SELECT {key_expr} AS key, {content_expr} AS content, {category_expr} AS category, \
         {agent_id_expr} AS agent_id, {session_id_expr} AS session_id, \
         {namespace_expr} AS namespace, {importance_expr} AS importance FROM memories"
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;

    let mut entries = Vec::new();
    let mut idx = 0_usize;

    while let Some(row) = rows.next()? {
        let key: String = row
            .get(0)
            .unwrap_or_else(|_| format!("openclaw_sqlite_{idx}"));
        let content: String = row.get(1).unwrap_or_default();
        let category_raw: String = row.get(2).unwrap_or_else(|_| "core".to_string());
        let agent_id: Option<String> = row.get(3).ok().flatten();
        let session_id: Option<String> = row.get(4).ok().flatten();
        let namespace: Option<String> = row.get(5).ok().flatten();
        let importance: Option<f64> = row.get(6).ok().flatten();

        if content.trim().is_empty() {
            continue;
        }

        entries.push(SourceEntry {
            key: normalize_key(&key, idx),
            content: content.trim().to_string(),
            category: parse_category(&category_raw),
            agent_id,
            session_id,
            namespace,
            importance,
        });

        idx += 1;
    }

    Ok(entries)
}

/// Read the source sqlite `agents` table (id, alias) so the migration can map
/// source agent_ids → live target agent_ids by alias (see `build_agent_alias_map`).
/// Returns an empty vec when the db or agents table is absent (legacy sources).
fn read_openclaw_sqlite_agents(db_path: &Path) -> Result<Vec<(String, String)>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Failed to open source db {}", db_path.display()))?;
    let table_exists: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='agents' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if table_exists.is_none() {
        return Ok(Vec::new());
    }
    let columns = table_columns(&conn, "agents")?;
    let id_expr = pick_column_expr(&columns, &["id"], "''");
    let alias_expr = pick_column_expr(&columns, &["alias", "name"], "''");
    let sql = format!("SELECT {id_expr} AS id, {alias_expr} AS alias FROM agents");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0).unwrap_or_default();
        let alias: String = row.get(1).unwrap_or_default();
        if !id.is_empty() && !alias.is_empty() {
            out.push((id, alias));
        }
    }
    Ok(out)
}

/// Build a source_agent_id → live_agent_id map by ensuring each source agent
/// (by alias) exists in the target backend. For postgres this mints/looks up
/// the target UUID keyed by alias, so imported memories reference the LIVE
/// agent and remain recallable by it. Best-effort: agents whose ensure fails
/// are skipped (their memories fall back to the default agent).
async fn build_agent_alias_map(
    memory: &dyn Memory,
    source_db: &Path,
) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let agents = read_openclaw_sqlite_agents(source_db)?;
    for (src_id, alias) in agents {
        match memory.ensure_agent_uuid(&alias).await {
            Ok(live_id) => {
                map.insert(src_id, live_id);
            }
            Err(e) => {
                eprintln!(
                    "warning: could not ensure agent '{alias}' in target; its memories will use the default agent: {e}"
                );
            }
        }
    }
    Ok(map)
}

fn read_openclaw_markdown_entries(source_workspace: &Path) -> Result<Vec<SourceEntry>> {
    let mut all = Vec::new();

    let core_path = source_workspace.join("MEMORY.md");
    if core_path.exists() {
        let content = fs::read_to_string(&core_path)?;
        all.extend(parse_markdown_file(
            &core_path,
            &content,
            MemoryCategory::Core,
            "openclaw_core",
        ));
    }

    let daily_dir = source_workspace.join("memory");
    if daily_dir.exists() {
        for file in fs::read_dir(&daily_dir)? {
            let file = file?;
            let path = file.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let content = fs::read_to_string(&path)?;
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("openclaw_daily");
            all.extend(parse_markdown_file(
                &path,
                &content,
                MemoryCategory::Daily,
                stem,
            ));
        }
    }

    Ok(all)
}

#[allow(clippy::needless_pass_by_value)]
fn parse_markdown_file(
    _path: &Path,
    content: &str,
    default_category: MemoryCategory,
    stem: &str,
) -> Vec<SourceEntry> {
    let mut entries = Vec::new();

    for (idx, raw_line) in content.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let line = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        let (key, text) = match parse_structured_memory_line(line) {
            Some((k, v)) => (normalize_key(k, idx), v.trim().to_string()),
            None => (
                format!("openclaw_{stem}_{}", idx + 1),
                line.trim().to_string(),
            ),
        };

        if text.is_empty() {
            continue;
        }

        entries.push(SourceEntry {
            key,
            content: text,
            category: default_category.clone(),
            agent_id: None,
            session_id: None,
            namespace: None,
            importance: None,
        });
    }

    entries
}

fn parse_structured_memory_line(line: &str) -> Option<(&str, &str)> {
    if !line.starts_with("**") {
        return None;
    }

    let rest = line.strip_prefix("**")?;
    let key_end = rest.find("**:")?;
    let key = rest.get(..key_end)?.trim();
    let value = rest.get(key_end + 3..)?.trim();

    if key.is_empty() || value.is_empty() {
        return None;
    }

    Some((key, value))
}

fn parse_category(raw: &str) -> MemoryCategory {
    match raw.trim().to_ascii_lowercase().as_str() {
        "core" | "" => MemoryCategory::Core,
        "daily" => MemoryCategory::Daily,
        "conversation" => MemoryCategory::Conversation,
        other => MemoryCategory::Custom(other.to_string()),
    }
}

fn normalize_key(key: &str, fallback_idx: usize) -> String {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return format!("openclaw_{fallback_idx}");
    }
    trimmed.to_string()
}

async fn next_available_key(memory: &dyn Memory, base: &str) -> Result<String> {
    for i in 1..=10_000 {
        let candidate = format!("{base}__openclaw_{i}");
        if memory.get(&candidate).await?.is_none() {
            return Ok(candidate);
        }
    }

    bail!("Unable to allocate non-conflicting key for '{base}'")
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;

    let mut cols = Vec::new();
    for col in rows {
        cols.push(col?.to_ascii_lowercase());
    }

    Ok(cols)
}

fn pick_optional_column_expr(columns: &[String], candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .find(|candidate| columns.iter().any(|c| c == *candidate))
        .map(std::string::ToString::to_string)
}

fn pick_column_expr(columns: &[String], candidates: &[&str], fallback: &str) -> String {
    pick_optional_column_expr(columns, candidates).unwrap_or_else(|| fallback.to_string())
}

fn resolve_openclaw_workspace(source: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(src) = source {
        return Ok(src);
    }

    let home = UserDirs::new()
        .map(|u| u.home_dir().to_path_buf())
        .context("Could not find home directory")?;

    Ok(home.join(".openclaw").join("workspace"))
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn backup_target_memory(workspace_dir: &Path) -> Result<Option<PathBuf>> {
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let backup_root = workspace_dir
        .join("memory")
        .join("migrations")
        .join(format!("openclaw-{timestamp}"));

    let mut copied_any = false;
    fs::create_dir_all(&backup_root)?;

    let files_to_copy = [
        workspace_dir.join("memory").join("brain.db"),
        workspace_dir.join("MEMORY.md"),
    ];

    for source in files_to_copy {
        if source.exists() {
            let Some(name) = source.file_name() else {
                continue;
            };
            fs::copy(&source, backup_root.join(name))?;
            copied_any = true;
        }
    }

    let daily_dir = workspace_dir.join("memory");
    if daily_dir.exists() {
        let daily_backup = backup_root.join("daily");
        for file in fs::read_dir(&daily_dir)? {
            let file = file?;
            let path = file.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            fs::create_dir_all(&daily_backup)?;
            let Some(name) = path.file_name() else {
                continue;
            };
            fs::copy(&path, daily_backup.join(name))?;
            copied_any = true;
        }
    }

    if copied_any {
        Ok(Some(backup_root))
    } else {
        let _ = fs::remove_dir_all(&backup_root);
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::TempDir;
    use zeroclaw_config::schema::{Config, MemoryConfig};
    use zeroclaw_memory::SqliteMemory;

    fn test_config(workspace: &Path) -> Config {
        Config {
            data_dir: workspace.to_path_buf(),
            config_path: workspace.join("config.toml"),
            memory: MemoryConfig {
                backend: "sqlite".to_string(),
                ..MemoryConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn parse_structured_markdown_line() {
        let line = "**user_pref**: likes Rust";
        let parsed = parse_structured_memory_line(line).unwrap();
        assert_eq!(parsed.0, "user_pref");
        assert_eq!(parsed.1, "likes Rust");
    }

    #[test]
    fn parse_unstructured_markdown_generates_key() {
        let entries = parse_markdown_file(
            Path::new("/tmp/MEMORY.md"),
            "- plain note",
            MemoryCategory::Core,
            "core",
        );
        assert_eq!(entries.len(), 1);
        assert!(entries[0].key.starts_with("openclaw_core_"));
        assert_eq!(entries[0].content, "plain note");
    }

    #[test]
    fn sqlite_reader_supports_legacy_value_column() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("brain.db");
        let conn = Connection::open(&db_path).unwrap();

        conn.execute_batch("CREATE TABLE memories (key TEXT, value TEXT, type TEXT);")
            .unwrap();
        conn.execute(
            "INSERT INTO memories (key, value, type) VALUES (?1, ?2, ?3)",
            params!["legacy_key", "legacy_value", "daily"],
        )
        .unwrap();

        let rows = read_openclaw_sqlite_entries(&db_path).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "legacy_key");
        assert_eq!(rows[0].content, "legacy_value");
        assert_eq!(rows[0].category, MemoryCategory::Daily);
    }

    #[tokio::test]
    async fn migration_renames_conflicting_key() {
        let source = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();

        // Existing target memory
        let target_mem = SqliteMemory::new("test", target.path()).unwrap();
        target_mem
            .store("k", "new value", MemoryCategory::Core, None)
            .await
            .unwrap();

        // Source sqlite with conflicting key + different content
        let source_db_dir = source.path().join("memory");
        fs::create_dir_all(&source_db_dir).unwrap();
        let source_db = source_db_dir.join("brain.db");
        let conn = Connection::open(&source_db).unwrap();
        conn.execute_batch("CREATE TABLE memories (key TEXT, content TEXT, category TEXT);")
            .unwrap();
        conn.execute(
            "INSERT INTO memories (key, content, category) VALUES (?1, ?2, ?3)",
            params!["k", "old value", "core"],
        )
        .unwrap();

        let config = test_config(target.path());
        migrate_openclaw_memory(&config, Some(source.path().to_path_buf()), false, false)
            .await
            .unwrap();

        let all = target_mem.list(None, None).await.unwrap();
        assert!(all.iter().any(|e| e.key == "k" && e.content == "new value"));
        assert!(
            all.iter()
                .any(|e| e.key.starts_with("k__openclaw_") && e.content == "old value")
        );
    }

    #[tokio::test]
    async fn dry_run_does_not_write() {
        let source = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        let source_db_dir = source.path().join("memory");
        fs::create_dir_all(&source_db_dir).unwrap();

        let source_db = source_db_dir.join("brain.db");
        let conn = Connection::open(&source_db).unwrap();
        conn.execute_batch("CREATE TABLE memories (key TEXT, content TEXT, category TEXT);")
            .unwrap();
        conn.execute(
            "INSERT INTO memories (key, content, category) VALUES (?1, ?2, ?3)",
            params!["dry", "run", "core"],
        )
        .unwrap();

        let config = test_config(target.path());
        migrate_openclaw_memory(&config, Some(source.path().to_path_buf()), true, false)
            .await
            .unwrap();

        let target_mem = SqliteMemory::new("test", target.path()).unwrap();
        assert_eq!(target_mem.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn migration_reindex_uses_same_dotted_sqlite_backend_as_import() {
        let source = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        let source_db_dir = source.path().join("memory");
        fs::create_dir_all(&source_db_dir).unwrap();

        let source_db = source_db_dir.join("brain.db");
        let conn = Connection::open(&source_db).unwrap();
        conn.execute_batch("CREATE TABLE memories (key TEXT, content TEXT, category TEXT);")
            .unwrap();
        conn.execute(
            "INSERT INTO memories (key, content, category) VALUES (?1, ?2, ?3)",
            params!["reindex_key", "reindex searchable content", "core"],
        )
        .unwrap();

        let mut config = test_config(target.path());
        config.memory.backend = "sqlite.default".to_string();
        migrate_openclaw_memory(&config, Some(source.path().to_path_buf()), false, true)
            .await
            .unwrap();

        let target_mem = SqliteMemory::new("test", target.path()).unwrap();
        let results = target_mem
            .recall("searchable", 10, None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "reindex_key");
    }

    #[test]
    fn migration_target_rejects_none_backend() {
        let target = TempDir::new().unwrap();
        let mut config = test_config(target.path());
        config.memory.backend = "none".to_string();

        let err = target_memory_backend(&config)
            .err()
            .expect("backend=none should be rejected for migration target");
        assert!(err.to_string().contains("disables persistence"));
    }

    #[test]
    fn migration_target_rejects_qdrant_with_localized_operator_guidance() {
        let target = TempDir::new().unwrap();
        let mut config = test_config(target.path());
        config.memory.backend = "qdrant.default".to_string();

        let err = target_memory_backend(&config)
            .err()
            .expect("Qdrant is not a supported OpenClaw migration target");
        let expected =
            crate::i18n::get_required_cli_string("cli-migrate-openclaw-qdrant-unsupported");
        assert_eq!(err.to_string(), expected);
        assert!(!err.to_string().contains("create_memory_"));
    }

    // ── §7.1 / §7.2 Config backward compatibility & migration tests ──

    #[test]
    fn parse_category_handles_all_variants() {
        assert_eq!(parse_category("core"), MemoryCategory::Core);
        assert_eq!(parse_category("daily"), MemoryCategory::Daily);
        assert_eq!(parse_category("conversation"), MemoryCategory::Conversation);
        assert_eq!(parse_category(""), MemoryCategory::Core);
        assert_eq!(
            parse_category("custom_type"),
            MemoryCategory::Custom("custom_type".to_string())
        );
    }

    #[test]
    fn parse_category_case_insensitive() {
        assert_eq!(parse_category("CORE"), MemoryCategory::Core);
        assert_eq!(parse_category("Daily"), MemoryCategory::Daily);
        assert_eq!(parse_category("CONVERSATION"), MemoryCategory::Conversation);
    }

    #[test]
    fn normalize_key_handles_empty_string() {
        let key = normalize_key("", 42);
        assert_eq!(key, "openclaw_42");
    }

    #[test]
    fn normalize_key_trims_whitespace() {
        let key = normalize_key("  my_key  ", 0);
        assert_eq!(key, "my_key");
    }

    #[test]
    fn parse_structured_markdown_rejects_empty_key() {
        assert!(parse_structured_memory_line("****:value").is_none());
    }

    #[test]
    fn parse_structured_markdown_rejects_empty_value() {
        assert!(parse_structured_memory_line("**key**:").is_none());
    }

    #[test]
    fn parse_structured_markdown_rejects_no_stars() {
        assert!(parse_structured_memory_line("key: value").is_none());
    }

    #[tokio::test]
    async fn migration_skips_empty_content() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("brain.db");
        let conn = Connection::open(&db_path).unwrap();

        conn.execute_batch("CREATE TABLE memories (key TEXT, content TEXT, category TEXT);")
            .unwrap();
        conn.execute(
            "INSERT INTO memories (key, content, category) VALUES (?1, ?2, ?3)",
            params!["empty_key", "   ", "core"],
        )
        .unwrap();

        let rows = read_openclaw_sqlite_entries(&db_path).unwrap();
        assert_eq!(
            rows.len(),
            0,
            "entries with empty/whitespace content must be skipped"
        );
    }

    #[test]
    fn backup_creates_timestamped_directory() {
        let tmp = TempDir::new().unwrap();
        let mem_dir = tmp.path().join("memory");
        std::fs::create_dir_all(&mem_dir).unwrap();

        // Create a brain.db to back up
        let db_path = mem_dir.join("brain.db");
        std::fs::write(&db_path, "fake db content").unwrap();

        let result = backup_target_memory(tmp.path()).unwrap();
        assert!(
            result.is_some(),
            "backup should be created when files exist"
        );

        let backup_dir = result.unwrap();
        assert!(backup_dir.exists());
        assert!(
            backup_dir.to_string_lossy().contains("openclaw-"),
            "backup dir must contain openclaw- prefix"
        );
    }

    #[test]
    fn backup_returns_none_when_no_files() {
        let tmp = TempDir::new().unwrap();
        let result = backup_target_memory(tmp.path()).unwrap();
        assert!(
            result.is_none(),
            "backup should return None when no files to backup"
        );
    }

    // ── migrate-to-postgres: V3 metadata preservation + postgres routing ──

    /// A V3-schema source (agent_id, session_id, namespace, importance columns
    /// present) must be read with those fields preserved on `SourceEntry`, so a
    /// sqlite→postgres migration keeps per-agent / per-session context instead
    /// of flattening every row to the `default` agent. Regression for the fix
    /// that extended `read_openclaw_sqlite_entries` beyond key/content/category.
    #[test]
    fn read_openclaw_sqlite_entries_preserves_v3_metadata() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("brain.db");
        let conn = Connection::open(&db_path).unwrap();

        conn.execute_batch(
            "CREATE TABLE memories (\n\
             key TEXT, content TEXT, category TEXT,\n\
             agent_id TEXT, session_id TEXT, namespace TEXT, importance REAL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memories (key, content, category, agent_id, session_id, namespace, importance)\n\
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params!["k1", "content one", "daily", "agent-rocky", "sess-7", "ns-rocky", 0.8],
        )
        .unwrap();
        // A row with NULL metadata must also be read (None preserved).
        conn.execute(
            "INSERT INTO memories (key, content, category, agent_id, session_id, namespace, importance)\n\
             VALUES (?1, ?2, ?3, NULL, NULL, NULL, NULL)",
            params!["k2", "content two", "core"],
        )
        .unwrap();

        let rows = read_openclaw_sqlite_entries(&db_path).unwrap();
        assert_eq!(rows.len(), 2, "both V3 rows must be read");

        let r1 = rows.iter().find(|r| r.key == "k1").expect("k1 present");
        assert_eq!(r1.content, "content one");
        assert_eq!(r1.category, MemoryCategory::Daily);
        assert_eq!(r1.agent_id.as_deref(), Some("agent-rocky"));
        assert_eq!(r1.session_id.as_deref(), Some("sess-7"));
        assert_eq!(r1.namespace.as_deref(), Some("ns-rocky"));
        assert_eq!(r1.importance, Some(0.8));

        let r2 = rows.iter().find(|r| r.key == "k2").expect("k2 present");
        assert_eq!(r2.agent_id, None);
        assert_eq!(r2.session_id, None);
        assert_eq!(r2.namespace, None);
        assert_eq!(r2.importance, None);
    }

    /// A legacy-schema source (only key/content/category) must still be read
    /// without error, with the V3 fields all `None`. Regression: the V3 column
    /// picks must not break imports of old OpenClaw workspaces.
    #[test]
    fn read_openclaw_sqlite_entries_legacy_schema_yields_none_metadata() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("brain.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE memories (key TEXT, content TEXT, category TEXT);")
            .unwrap();
        conn.execute(
            "INSERT INTO memories (key, content, category) VALUES (?1, ?2, ?3)",
            params!["legacy", "value", "core"],
        )
        .unwrap();

        let rows = read_openclaw_sqlite_entries(&db_path).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "legacy");
        assert_eq!(rows[0].agent_id, None);
        assert_eq!(rows[0].session_id, None);
        assert_eq!(rows[0].namespace, None);
        assert_eq!(rows[0].importance, None);
    }

    /// `target_memory_backend` (the migrate import path) must route a postgres
    /// backend through `create_memory_with_storage_and_routes`, NOT the builder
    /// path that bails with "postgres backend requires storage config; call
    /// create_memory_with_storage_and_routes instead of create_memory_with_builders".
    /// With no `[storage.postgres.<alias>]` configured, the fixed path reaches
    /// storage resolution and errors there; the pre-fix path bailed earlier at
    /// the builder. This test fails on the broken builder path (the error
    /// contains the builder bail message) and passes once routed correctly.
    #[test]
    fn migration_target_routes_postgres_via_storage_and_routes() {
        let target = TempDir::new().unwrap();
        let mut config = test_config(target.path());
        config.memory.backend = "postgres.test".to_string();
        // No [storage.postgres.test] configured → storage resolution fails, but
        // the failure must NOT be the builder-path bail.
        let err = target_memory_backend(&config)
            .err()
            .expect("postgres backend should error at storage resolution, not succeed with no storage");
        let msg = err.to_string();
        assert!(
            !msg.contains(
                "create_memory_with_storage_and_routes instead of create_memory_with_builders"
            ),
            "migrate must route postgres via storage_and_routes, not the builder path. Got: {msg}"
        );
    }

    /// `read_openclaw_sqlite_agents` reads the source `agents` table (id, alias)
    /// so the migration can map source agent_ids → live target agent_ids by alias.
    /// Without this, imported memories would reference the source UUID and be
    /// orphaned from the live agent's recall (or violate the target's agents FK).
    #[test]
    fn read_openclaw_sqlite_agents_reads_id_and_alias() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("brain.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE agents (id TEXT PRIMARY KEY, alias TEXT NOT NULL UNIQUE, created_at TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agents (id, alias, created_at) VALUES (?1, ?2, 'now')",
            params!["src-rocky-id", "rocky"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agents (id, alias, created_at) VALUES (?1, ?2, 'now')",
            params!["src-coder-id", "codereviewer"],
        )
        .unwrap();

        let agents = read_openclaw_sqlite_agents(&db_path).unwrap();
        assert_eq!(agents.len(), 2);
        // Order is unspecified; check both pairs are present.
        assert!(agents.iter().any(|(id, alias)| id == "src-rocky-id" && alias == "rocky"));
        assert!(agents.iter().any(|(id, alias)| id == "src-coder-id" && alias == "codereviewer"));
    }

    /// A source with no agents table (legacy) yields an empty agent list, so the
    /// migration falls back to the default agent for all memories (no crash).
    #[test]
    fn read_openclaw_sqlite_agents_returns_empty_when_no_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("brain.db");
        // Create a db with a memories table but no agents table.
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE memories (key TEXT, content TEXT, category TEXT);")
            .unwrap();

        let agents = read_openclaw_sqlite_agents(&db_path).unwrap();
        assert!(agents.is_empty(), "no agents table → empty agent list");
    }
}
