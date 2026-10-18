//! C2 — the recursive-grep steering guard (spec:
//! `docs/superpowers/specs/2026-10-18-c2-unscoped-grep-steering-design.md`,
//! quorum-approved R3-clean). One boolean rule: a `grep`-family invocation
//! carrying a recursive flag (`-r`/`-R`, incl. combos and `--recursive`) →
//! refuse + redirect to the native `content_search` (rg-backed, 30s cap,
//! 1000 results), with the extracted pattern as a ready-to-call suggestion.
//!
//! The parse is quote-aware at DETECTION (quoted segments are data, not
//! command positions; a quoted `|` does not segment stages; `-e`/`-f`/
//! `--regexp` operands are values, not flags) and scans ALL tokens of the
//! grep stage (`grep foo -r .` refuses — the round-1 silent-false-negative
//! class). It REUSES `policy.rs`'s segmentation helpers via the public
//! `SecurityPolicy` API — never a hand-rolled parser (the quorum's DRY
//! ruling; the `split_unquoted_segments` + `command_basename` helpers are
//! already exported through `zeroclaw_config::policy`).

use serde_json::json;

/// The redirect advice extracted from a refused recursive-grep invocation:
/// the ready-to-call `content_search` suggestion with the pattern and flag
/// parity (`-i` → `case_sensitive:false`, `-l`/`-rl` → `output_mode:
/// "files_with_matches"` — the round-1 semantic-degradation catch).
#[derive(Debug, Clone, PartialEq)]
pub struct SteerAdvice {
    /// The extracted search pattern (best-effort — degrades on
    /// `-e`/`--regexp` values, post-`--include` patterns, and past `--`;
    /// degrades the suggestion, never the refusal).
    pub pattern: String,
    /// The full `content_search` args JSON (the ready-to-call suggestion).
    pub suggestion: serde_json::Value,
}

impl SteerAdvice {
    /// The refusal message the shell tool returns (the same UX channel as
    /// a policy denial — "a denial is a rephrase request, not a breakage").
    pub fn refusal_message(&self) -> String {
        format!(
            "Refused: recursive grep — use the native bounded search instead: \
             content_search with {} (rg-backed, 30s cap, 1000 results max; \
             `path` defaults to the workspace root). Shell grep is fine for \
             single files and pipe filters; recursive file search belongs to \
             content_search.",
             self.suggestion
        )
    }
}

/// Extract the first non-flag positional argument from a grep invocation's
/// token list (the search pattern; best-effort per the spec — the `-e`/
/// `--regexp` operand case is handled at the flag-scan level, so a
/// flag-operand is never mistaken for the pattern here).
fn extract_pattern(tokens: &[String]) -> Option<String> {
    for tok in tokens {
        if tok.starts_with('-') {
            continue;
        }
        // Strip wrapping quotes (single or double) — the QuoteState-aware
        // segmentation already separated stages; this is the per-token trim.
        let trimmed = tok.trim_matches(|c| c == '\'' || c == '"');
        return Some(trimmed.to_string());
    }
    None
}

/// Check whether a flag token carries the recursive flag: standalone
/// `-r`/`-R`, a combo embedding `r` (`-ril`, `-rn`, `-inr`), or the long
/// form `--recursive`. The `-e "-r"` case is an OPERAND (handled by the
/// caller's operand-awareness, never reaching here as a flag).
fn is_recursive_flag(token: &str) -> bool {
    if token == "--recursive" || token == "-r" || token == "-R" {
        return true;
    }
    // Short combos: a token starting with a single `-` (not `--`) that
    // contains an `r` among its short flags (e.g. `-ril`, `-rn`, `-inr`).
    if token.len() > 2 && token.starts_with('-') && !token.starts_with("--") {
        return token.chars().any(|c| c == 'r' || c == 'R');
    }
    false
}

/// Quote-aware tokenizer (the spec's §3.1 detection-stage rule): tokens
/// are split on whitespace ONLY outside single/double quotes — a quoted
/// segment is ONE token, its contents never parsed as command positions
/// (`"see grep -ril usage"` is a pattern, not flags). This is the
/// detection-stage complement to `split_unquoted_segments`' stage-level
/// quote handling; the quote state machine mirrors the same
/// single/double/escape rules.
fn tokenize_quoted_aware(stage: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for ch in stage.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(ch);
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Scan ONE shell pipeline stage (already unquoted-segmented and
/// pipe-split by the caller) for the recursive-grep shape. Returns the
/// steer advice when the stage is a grep-family invocation with a
/// recursive flag; `None` for every legitimate shape.
fn detect_in_stage(stage: &str) -> Option<SteerAdvice> {
    let tokens: Vec<String> = tokenize_quoted_aware(stage);
    if tokens.is_empty() {
        return None;
    }
    // The command position: the first token (basename matching covers
    // path prefixes — /bin/grep, /usr/bin/ggrep — per the round-2 ruling).
    let first = tokens[0].as_str();
    let basename = first.rsplit('/').next().unwrap_or(first);
    let basename = basename.strip_suffix(".exe").unwrap_or(basename);
    let is_grep = basename == "grep" || basename == "ggrep" || basename == "egrep"
        || basename == "fgrep";
    if !is_grep {
        return None;
    }
    // All-token flag scan (the round-1 flags-after-positional class:
    // `grep foo -r .`). Operand-awareness: after `-e`/`-f`/`--regexp`/
    // `--include=`/`--exclude=`, the NEXT token is a value, not a flag.
    let mut recursive = false;
    let mut case_insensitive = false;
    let mut files_with_matches = false;
    let mut i = 1;
    while i < tokens.len() {
        let tok = tokens[i].as_str();
        if tok == "-e" || tok == "-f" || tok == "--regexp" || tok == "--file" {
            i += 2; // skip the operand — a value, not a flag
            continue;
        }
        if tok.starts_with("--include=") || tok.starts_with("--exclude=")
            || tok.starts_with("--include-dir=")
        {
            i += 1; // the = form carries its own value
            continue;
        }
        if is_recursive_flag(tok) {
            recursive = true;
        }
        if tok.contains('i') && tok.starts_with('-') && !tok.starts_with("--") && tok.len() > 2 {
            case_insensitive = true; // a combo carrying -i (e.g. -ril)
        }
        if tok == "-l" || tok == "--files-with-matches"
            || (tok.starts_with('-') && !tok.starts_with("--") && tok.contains('l') && tok.len() > 2)
        {
            files_with_matches = true;
        }
        i += 1;
    }
    if !recursive {
        return None;
    }
    // The flag-parity suggestion: the extracted pattern + the semantics
    // the refused grep carried (case-insensitive → case_sensitive:false;
    // -l → files_with_matches — content_search.rs:89/79-82's real keys).
    let pattern = extract_pattern(&tokens[1..]).unwrap_or_default();
    let mut args = json!({ "pattern": pattern });
    if case_insensitive {
        args["case_sensitive"] = json!(false);
    }
    if files_with_matches {
        args["output_mode"] = json!("files_with_matches");
    }
    Some(SteerAdvice { pattern, suggestion: args })
}

/// The entry point (the rules registry's first rule — the spec's §3.0
/// extension contract): detect a recursive grep anywhere in the command's
/// unquoted pipeline stages. Quoted segments are DATA (the
/// `split_unquoted_segments` helper — quote-aware at detection), so
/// `echo "run grep -rn foo ."` passes and a pattern containing a flag
/// cluster (`grep -n "see grep -ril usage" notes.md`) passes its
/// single-file shape because its stage carries no recursive flag at a
/// command position.
pub fn detect_recursive_grep(command: &str) -> Option<SteerAdvice> {
    let segments = zeroclaw_config::policy::split_unquoted_segments_public(command);
    for segment in &segments {
        // Pipe-split each segment into stages; a quoted | was already
        // kept inside its segment by the quote-aware splitting.
        for stage in segment.split('|') {
            if let Some(advice) = detect_in_stage(stage.trim()) {
                return Some(advice);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Refuse: the recursive shapes (the spec's §4 matrix) ──────────

    #[test]
    fn refuses_basic_recursive() {
        let a = detect_recursive_grep("grep -r x .").unwrap();
        assert_eq!(a.pattern, "x");
    }

    #[test]
    fn refuses_uppercase_r() {
        assert!(detect_recursive_grep("grep -R x dir").is_some());
    }

    #[test]
    fn refuses_combos() {
        for cmd in ["grep -ril x .", "grep -rn x .", "grep -rl x .", "grep -inr x ."] {
            assert!(detect_recursive_grep(cmd).is_some(), "{cmd}");
        }
    }

    #[test]
    fn refuses_long_form() {
        assert!(detect_recursive_grep("grep --recursive x .").is_some());
    }

    #[test]
    fn refuses_flags_after_positional() {
        // The round-1 silent-false-negative class
        let a = detect_recursive_grep("grep foo -r .").unwrap();
        assert_eq!(a.pattern, "foo");
        assert!(detect_recursive_grep("grep --include='*.rs' foo -R dir").is_some());
    }

    #[test]
    fn refuses_pipeline_recursive_stage() {
        // The whole command is refused pre-spawn — the recursive stage is
        // detected anywhere in the pipeline
        assert!(detect_recursive_grep("grep -ril x . | grep -v y").is_some());
    }

    #[test]
    fn refuses_path_prefixed() {
        // Basename matching (the round-2 ruling)
        assert!(detect_recursive_grep("/bin/grep -r x .").is_some());
        assert!(detect_recursive_grep("/usr/bin/ggrep -r x .").is_some());
    }

    // ── Pass: the legitimate shapes ──────────────────────────────────

    #[test]
    fn passes_single_file() {
        assert!(detect_recursive_grep("grep -n x file.md").is_none());
    }

    #[test]
    fn passes_pipe_filter() {
        assert!(detect_recursive_grep("ps aux | grep nginx").is_none());
        assert!(detect_recursive_grep("cat out | grep -v /git/").is_none());
    }

    #[test]
    fn passes_quoted_non_command_position() {
        // Quoted segments are data, not command positions
        assert!(detect_recursive_grep("echo \"run grep -rn foo .\"").is_none());
    }

    #[test]
    fn passes_pattern_containing_flag_cluster() {
        // A single-file grep whose pattern MENTIONS -ril: no recursive
        // flag at a command position (the -e/-n flag scan sees the
        // quoted pattern as a positional, not a flag)
        assert!(detect_recursive_grep("grep -n \"see grep -ril usage\" notes.md").is_none());
    }

    #[test]
    fn passes_e_operand() {
        // -e "-r" is an OPERAND, not a flag
        assert!(detect_recursive_grep("grep -e \"-r\" file").is_none());
    }

    #[test]
    fn passes_regexp_operand() {
        assert!(detect_recursive_grep("grep --regexp \"-r\" file").is_none());
    }

    // ── Flag parity + extraction (the spec's §4) ──────────────────────

    #[test]
    fn flag_parity_case_insensitive() {
        let a = detect_recursive_grep("grep -ril x .").unwrap();
        assert_eq!(a.suggestion["case_sensitive"], json!(false));
        assert_eq!(a.suggestion["output_mode"], json!("files_with_matches"));
        assert_eq!(a.suggestion["pattern"], json!("x"));
    }

    #[test]
    fn flag_parity_files_with_matches() {
        let a = detect_recursive_grep("grep -rl x .").unwrap();
        assert_eq!(a.suggestion["output_mode"], json!("files_with_matches"));
        assert!(a.suggestion.get("case_sensitive").is_none());
    }

    #[test]
    fn refusal_message_names_content_search() {
        let a = detect_recursive_grep("grep -r pattern .").unwrap();
        let msg = a.refusal_message();
        assert!(msg.contains("content_search"));
        assert!(msg.contains("pattern"));
    }
}
