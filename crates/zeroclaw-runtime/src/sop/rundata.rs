use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::types::{SopStepResult, SopStepStatus};

/// Accumulated step outputs, shared by schema validation and routing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunData {
    pub outputs: BTreeMap<u32, Value>,
    /// The decision model's p(yes) per conditional-part step, exposed to
    /// `when:` conditions as `$.decisions.<step>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub decisions: BTreeMap<u32, f64>,
}

impl RunData {
    pub fn from_step_results(results: &[SopStepResult]) -> Self {
        let mut data = Self::default();
        for result in results {
            if result.status == SopStepStatus::Completed {
                data.insert_output_str(result.step_number, &result.output);
            }
        }
        data
    }

    pub fn with_decisions(mut self, decisions: &BTreeMap<u32, f64>) -> Self {
        self.decisions.clone_from(decisions);
        self
    }

    pub fn insert_output(&mut self, step_number: u32, output: Value) {
        self.outputs.insert(step_number, output);
    }

    pub fn insert_output_str(&mut self, step_number: u32, output: &str) {
        let value = serde_json::from_str(output).unwrap_or_else(|_| Value::String(output.into()));
        self.insert_output(step_number, value);
    }

    pub fn merge(&mut self, other: RunData) {
        self.outputs.extend(other.outputs);
        self.decisions.extend(other.decisions);
    }

    pub fn to_payload(&self) -> Value {
        let steps = self
            .outputs
            .iter()
            .map(|(step, value)| (step.to_string(), value.clone()))
            .collect();
        if self.decisions.is_empty() {
            return json!({ "steps": Value::Object(steps) });
        }
        let decisions = self
            .decisions
            .iter()
            .map(|(step, p)| (step.to_string(), json!(p)))
            .collect();
        json!({ "steps": Value::Object(steps), "decisions": Value::Object(decisions) })
    }

    pub fn get_path(&self, path: &str) -> Option<Value> {
        if path.is_empty() {
            return None;
        }
        let payload = self.to_payload();
        let pointer = path
            .strip_prefix("$.")
            .map(|rest| format!("/{}", rest.replace('.', "/")))
            .unwrap_or_else(|| path.to_string());
        payload.pointer(&pointer).cloned()
    }
}

/// A parsed step output plus, when the enum-tiny prose recovery fired, the
/// vocabulary token the object was reconstructed from — the observability
/// handle the engine turns into a `step_output_recovered` audit row.
pub(crate) struct ParsedStepOutput {
    pub value: Value,
    pub recovered_enum_token: Option<String>,
}

/// Parse a model step's raw output text into a JSON value, reporting the
/// enum-tiny prose recovery when it fired.
///
/// Exact JSON remains the first and schema-independent path. Fuzzy recovery is
/// deliberately limited to outputs with a declared schema: trigger payloads and
/// other raw strings must not lose their surrounding instructions just because
/// they contain a JSON example. A recovered candidate must be the only complete
/// object/array in the text and satisfy the declared output schema.
///
/// The third rung is the enum-tiny prose recovery: when the declared schema is
/// exactly one required string-enum property and the raw text (which carried no
/// JSON container at all) contains exactly one DISTINCT vocabulary token, the
/// object is reconstructed from that token — see [`enum_tiny_prose_recovery`].
/// Every other shape fails closed to the raw string, byte-for-byte.
pub(crate) fn parse_step_output_value_detailed(
    raw: &str,
    output_schema: Option<&Value>,
) -> ParsedStepOutput {
    let fallback = || ParsedStepOutput {
        value: Value::String(raw.into()),
        recovered_enum_token: None,
    };
    let trimmed = raw.trim();

    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        // A double-encoded payload parses cleanly as a top-level JSON string,
        // so the embedded-container recovery below never gets a chance at it.
        // One schema-gated unwrap of the inner text is a recovery, not a
        // reinterpretation: the inner value is accepted only when the declared
        // schema validates it; anything else keeps the string untouched.
        if let (Some(schema), Value::String(inner)) = (output_schema, &value)
            && let Ok(unwrapped) = serde_json::from_str::<Value>(inner.trim())
            && super::schema::validate_value(schema, &unwrapped).is_ok()
        {
            return ParsedStepOutput {
                value: unwrapped,
                recovered_enum_token: None,
            };
        }
        return ParsedStepOutput {
            value,
            recovered_enum_token: None,
        };
    }

    let Some(schema) = output_schema else {
        return fallback();
    };
    let Some(candidate) = unique_embedded_container(trimmed) else {
        return match enum_tiny_prose_recovery(schema, trimmed) {
            Some((value, token)) => ParsedStepOutput {
                value,
                recovered_enum_token: Some(token),
            },
            None => fallback(),
        };
    };
    if super::schema::validate_value(schema, &candidate).is_ok() {
        ParsedStepOutput {
            value: candidate,
            recovered_enum_token: None,
        }
    } else {
        fallback()
    }
}

/// The enum-tiny prose recovery: the observed `step_schema_reject` class is a
/// step whose declared output schema is exactly one required string-enum
/// property (the deploy-executor's entry-class and verdict steps) answered in
/// prose that names the chosen value but never wraps it in JSON. When the raw
/// text carries exactly one DISTINCT vocabulary token, the declared object is
/// reconstructed from that token.
///
/// Fail-closed on everything else: zero vocabulary tokens, two or more
/// DISTINCT tokens (an ambiguous answer), a schema that is not enum-tiny, or
/// any extractable JSON container (the container rungs above own those
/// outputs) — all keep the raw string byte-for-byte. The matching rule is
/// whole-token over the ASCII vocabulary charset `[A-Za-z0-9_]`
/// (see [`vocabulary_tokens`]), case-insensitive against the declared enum
/// values, and the reconstructed value is spelled exactly as the schema's
/// enum declares it.
fn enum_tiny_prose_recovery(schema: &Value, text: &str) -> Option<(Value, String)> {
    let (prop, variants) = enum_tiny_property(schema)?;
    let mut matched: Option<&str> = None;
    for token in vocabulary_tokens(text) {
        let lowered = token.to_ascii_lowercase();
        let Some(entry) = variants
            .iter()
            .filter_map(Value::as_str)
            .find(|variant| variant.to_ascii_lowercase() == lowered)
        else {
            continue;
        };
        if let Some(previous) = matched {
            if previous != entry {
                // Two distinct vocabulary values in one answer is ambiguous.
                return None;
            }
        } else {
            matched = Some(entry);
        }
    }
    let token = matched?;
    let value = serde_json::json!({ prop: token });
    super::schema::validate_value(schema, &value).ok()?;
    Some((value, token.to_string()))
}

/// The enum-tiny output shape: `type: "object"` with exactly one property
/// that is both required and a string enum. Computed from the schema's
/// serde value — never by string-matching the schema text. Anything else
/// (extra properties, a non-enum property type, several required keys, a
/// bare string-enum without the object wrapper) is not enum-tiny.
fn enum_tiny_property(schema: &Value) -> Option<(&str, &[Value])> {
    let object = schema.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return None;
    }
    let required = object.get("required")?.as_array()?;
    if required.len() != 1 {
        return None;
    }
    let required_key = required[0].as_str()?;
    let properties = object.get("properties")?.as_object()?;
    if properties.len() != 1 {
        return None;
    }
    let (name, property) = properties.iter().next()?;
    if name != required_key {
        return None;
    }
    let property = property.as_object()?;
    if property.get("type").and_then(Value::as_str) != Some("string") {
        return None;
    }
    let variants = property.get("enum")?.as_array()?;
    if variants.is_empty() || variants.iter().any(|variant| !variant.is_string()) {
        return None;
    }
    Some((name.as_str(), variants))
}

/// Maximal runs of the ASCII vocabulary charset `[A-Za-z0-9_]`. Underscore
/// joins stay one token (`not_ahead`); every other byte — space, punctuation,
/// quotes, backticks, hyphens, non-ASCII letters — delimits. Whole-token
/// equality is the match rule: `own` must not match inside `shown` or
/// `owner`, and `rejected` is not `reject` (a miss falls back to the
/// reject-and-retry path; a wrong hit would pipe a fabricated value
/// downstream, which is strictly worse).
fn vocabulary_tokens(text: &str) -> Vec<&str> {
    let is_token_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut tokens = Vec::new();
    let mut start: Option<usize> = None;
    let bytes = text.bytes().chain(std::iter::once(0));
    for (index, byte) in bytes.enumerate() {
        if is_token_byte(byte) {
            if start.is_none() {
                start = Some(index);
            }
        } else if let Some(begin) = start.take() {
            tokens.push(&text[begin..index]);
        }
    }
    tokens
}

/// Return one complete embedded JSON object or array. Advancing past a parsed
/// container prevents nested objects (for example, an object inside an array)
/// from becoming competing candidates. A second top-level candidate makes the
/// output ambiguous and therefore unrecoverable.
fn unique_embedded_container(text: &str) -> Option<Value> {
    let mut candidate = None;
    let mut cursor = 0;

    while cursor < text.len() {
        let Some((relative_start, opening)) = text[cursor..]
            .char_indices()
            .find(|(_, ch)| matches!(ch, '{' | '['))
        else {
            break;
        };
        let start = cursor + relative_start;
        let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        match stream.next() {
            Some(Ok(value)) if value.is_object() || value.is_array() => {
                if candidate.is_some() {
                    return None;
                }
                candidate = Some(value);
                cursor = start + stream.byte_offset();
            }
            _ => {
                cursor = start + opening.len_utf8();
            }
        }
    }

    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_encoded_output_with_schema_is_unwrapped() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["head_sha"],
            "properties": {"head_sha": {"type": "string"}}
        });
        let payload = r#"{"head_sha":"abc"}"#;
        let raw = serde_json::to_string(payload).unwrap();

        let value = parse_step_output_value_detailed(&raw, Some(&schema)).value;

        assert_eq!(value["head_sha"], "abc");
    }

    #[test]
    fn double_encoded_output_without_schema_stays_a_string() {
        let payload = r#"{"head_sha":"abc"}"#;
        let raw = serde_json::to_string(payload).unwrap();

        let value = parse_step_output_value_detailed(&raw, None).value;

        assert_eq!(value, Value::String(payload.to_string()));
    }

    #[test]
    fn double_encoded_output_failing_the_schema_stays_a_string() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["review_path"],
            "properties": {"review_path": {"type": "string"}}
        });
        let payload = r#"{"head_sha":"abc"}"#;
        let raw = serde_json::to_string(payload).unwrap();

        let value = parse_step_output_value_detailed(&raw, Some(&schema)).value;

        assert_eq!(value, Value::String(payload.to_string()));
    }

    #[test]
    fn run_data_parses_json_outputs() {
        let mut data = RunData::default();
        data.insert_output_str(1, r#"{"ok":true}"#);

        assert_eq!(data.outputs[&1]["ok"], true);
    }

    #[test]
    fn run_data_keeps_plain_outputs_as_strings() {
        let mut data = RunData::default();
        data.insert_output_str(2, "plain text");

        assert_eq!(data.outputs[&2], Value::String("plain text".into()));
    }

    #[test]
    fn parses_fenced_json_wrapped_in_prose() {
        // The shape a chat-tuned / "Claude Code"-identity model commonly emits.
        let raw = "The run has ended — final status is **failed**.\n\n\
            ```json\n{\n  \"status\": \"ok\",\n  \"word_count\": 738\n}\n```\n\n\
            Summary: the run stopped safely at Step 1.";
        let schema = json!({"type": "object", "required": ["status", "word_count"]});
        let value = parse_step_output_value_detailed(raw, Some(&schema)).value;
        assert_eq!(value["status"], "ok");
        assert_eq!(value["word_count"], 738);
    }

    #[test]
    fn parses_bare_fenced_json_without_lang_tag() {
        let raw = "```\n{\"outcome\": \"stopped\"}\n```";
        let schema = json!({"type": "object", "required": ["outcome"]});
        assert_eq!(
            parse_step_output_value_detailed(raw, Some(&schema)).value["outcome"],
            "stopped"
        );
    }

    #[test]
    fn parses_object_embedded_in_prose_without_fences() {
        let raw = "Here is the result: {\"preferences_loaded\": true} — done.";
        let schema = json!({"type": "object", "required": ["preferences_loaded"]});
        assert_eq!(
            parse_step_output_value_detailed(raw, Some(&schema)).value["preferences_loaded"],
            Value::Bool(true)
        );
    }

    #[test]
    fn braces_inside_strings_do_not_unbalance_extraction() {
        let raw = "note: {\"summary\": \"contains a } brace and { another\"} tail";
        let schema = json!({"type": "object", "required": ["summary"]});
        let value = parse_step_output_value_detailed(raw, Some(&schema)).value;
        assert_eq!(value["summary"], "contains a } brace and { another");
    }

    #[test]
    fn non_json_prose_still_falls_back_to_string() {
        let raw = "I could not complete the extraction step.";
        let schema = json!({"type": "object"});
        assert_eq!(
            parse_step_output_value_detailed(raw, Some(&schema)).value,
            Value::String(raw.into())
        );
    }

    #[test]
    fn preserves_complete_array_instead_of_extracting_nested_object() {
        let raw = "Result: [{\"approved\":true}]";
        let schema = json!({
            "type": "array",
            "items": {"type": "object", "required": ["approved"]}
        });

        assert_eq!(
            parse_step_output_value_detailed(raw, Some(&schema)).value,
            json!([{"approved": true}])
        );
    }

    #[test]
    fn competing_json_values_fail_closed_to_original_string() {
        let raw = "first {\"approved\":true}; second {\"approved\":false}";
        let schema = json!({"type": "object", "required": ["approved"]});

        assert_eq!(
            parse_step_output_value_detailed(raw, Some(&schema)).value,
            Value::String(raw.into())
        );
    }

    #[test]
    fn recovery_must_match_declared_output_shape() {
        let raw = "Use this example: {\"approved\":true}";
        let schema = json!({"type": "string"});

        assert_eq!(
            parse_step_output_value_detailed(raw, Some(&schema)).value,
            Value::String(raw.into())
        );
    }

    #[test]
    fn wrapped_json_without_output_schema_stays_raw() {
        let raw = "Use this example: {\"approved\":true}";

        assert_eq!(
            parse_step_output_value_detailed(raw, None).value,
            Value::String(raw.into())
        );
    }

    // The deploy-executor entry-class shape (its SOP step 2's `- output:`
    // bullet) — the enum-tiny shape the prose rung targets.
    fn enum_tiny_schema(prop: &str, variants: &[&str]) -> Value {
        json!({
            "type": "object",
            "required": [prop],
            "properties": {
                prop: {"type": "string", "enum": variants}
            }
        })
    }

    #[test]
    fn enum_tiny_prose_with_one_vocabulary_token_recovers_the_object() {
        let schema = enum_tiny_schema(
            "entry_class",
            &["own", "fresh", "tip", "fallback", "reject"],
        );
        let raw = "I compared the three candidate classes and the best fit for this deploy is own.";

        let parsed = parse_step_output_value_detailed(raw, Some(&schema));

        assert_eq!(parsed.value, json!({"entry_class": "own"}));
        assert_eq!(parsed.recovered_enum_token.as_deref(), Some("own"));
    }

    #[test]
    fn enum_tiny_prose_matches_tokens_case_insensitively_and_keeps_the_enum_spelling() {
        let schema = enum_tiny_schema("verdict", &["execute", "deduped", "error"]);
        let raw = "Verdict: DEDUPED — nothing further to run.";

        let parsed = parse_step_output_value_detailed(raw, Some(&schema));

        assert_eq!(parsed.value, json!({"verdict": "deduped"}));
        assert_eq!(parsed.recovered_enum_token.as_deref(), Some("deduped"));
    }

    #[test]
    fn enum_tiny_prose_requires_whole_tokens_not_substrings() {
        let schema = enum_tiny_schema(
            "entry_class",
            &["own", "fresh", "tip", "fallback", "reject"],
        );
        // `owner`/`shown` contain "own" as a substring; `candidates` contains
        // no "tip"; only whole-token occurrences may match.
        let raw = "The owner class was not among the candidates shown today.";

        let parsed = parse_step_output_value_detailed(raw, Some(&schema));

        assert_eq!(parsed.value, Value::String(raw.into()));
        assert_eq!(parsed.recovered_enum_token, None);
    }

    #[test]
    fn enum_tiny_prose_with_two_distinct_tokens_stays_a_string() {
        let schema = enum_tiny_schema("disposition", &["proceed", "hold", "not_ahead"]);
        let raw = "I could proceed with the deploy, or hold and wait for the window.";

        let parsed = parse_step_output_value_detailed(raw, Some(&schema));

        assert_eq!(parsed.value, Value::String(raw.into()));
        assert_eq!(parsed.recovered_enum_token, None);
    }

    #[test]
    fn enum_tiny_prose_with_a_repeated_single_token_still_recovers() {
        let schema = enum_tiny_schema(
            "entry_class",
            &["own", "fresh", "tip", "fallback", "reject"],
        );
        let raw = "Entry class: own. The own class was chosen.";

        let parsed = parse_step_output_value_detailed(raw, Some(&schema));

        assert_eq!(parsed.value, json!({"entry_class": "own"}));
    }

    #[test]
    fn enum_tiny_prose_with_zero_tokens_stays_a_string() {
        let schema = enum_tiny_schema(
            "entry_class",
            &["own", "fresh", "tip", "fallback", "reject"],
        );
        let raw = "I could not complete the extraction step.";

        let parsed = parse_step_output_value_detailed(raw, Some(&schema));

        assert_eq!(parsed.value, Value::String(raw.into()));
    }

    #[test]
    fn extra_property_schema_prose_stays_a_string() {
        // The deploy-executor preflight's real shape: one required enum
        // property plus an optional companion — extra properties are not
        // enum-tiny, so the rung must not fire.
        let schema = json!({
            "type": "object",
            "required": ["disposition"],
            "properties": {
                "disposition": {"type": "string", "enum": ["proceed", "hold", "not_ahead"]},
                "pr": {"type": "integer"}
            }
        });
        let raw = "the disposition is proceed, pr pending";

        let parsed = parse_step_output_value_detailed(raw, Some(&schema));

        assert_eq!(parsed.value, Value::String(raw.into()));
        assert_eq!(parsed.recovered_enum_token, None);
    }

    #[test]
    fn string_enum_without_the_object_wrapper_stays_a_string() {
        let schema = json!({"type": "string", "enum": ["proceed", "hold"]});
        let raw = "I would proceed.";

        let parsed = parse_step_output_value_detailed(raw, Some(&schema));

        assert_eq!(parsed.value, Value::String(raw.into()));
    }

    #[test]
    fn non_enum_property_type_stays_a_string() {
        let schema = json!({
            "type": "object",
            "required": ["head_sha"],
            "properties": {"head_sha": {"type": "string"}}
        });
        let raw = "the head sha is abcdef123";

        let parsed = parse_step_output_value_detailed(raw, Some(&schema));

        assert_eq!(parsed.value, Value::String(raw.into()));
    }

    #[test]
    fn multiple_required_properties_stay_a_string() {
        let schema = json!({
            "type": "object",
            "required": ["status", "word_count"],
            "properties": {
                "status": {"type": "string", "enum": ["ok", "failed"]},
                "word_count": {"type": "integer"}
            }
        });
        let raw = "status ok";

        let parsed = parse_step_output_value_detailed(raw, Some(&schema));

        assert_eq!(parsed.value, Value::String(raw.into()));
    }

    #[test]
    fn existing_container_extraction_wins_over_the_enum_tiny_rung() {
        let schema = enum_tiny_schema(
            "entry_class",
            &["own", "fresh", "tip", "fallback", "reject"],
        );
        let raw = "The verdict:\n```json\n{\"entry_class\": \"fresh\"}\n```";

        let parsed = parse_step_output_value_detailed(raw, Some(&schema));

        assert_eq!(parsed.value, json!({"entry_class": "fresh"}));
        assert_eq!(
            parsed.recovered_enum_token, None,
            "a container extraction is not a prose recovery"
        );
    }

    #[test]
    fn competing_containers_stay_a_string_even_under_an_enum_tiny_schema() {
        let schema = enum_tiny_schema(
            "entry_class",
            &["own", "fresh", "tip", "fallback", "reject"],
        );
        let raw = "first {\"entry_class\":\"own\"}; second {\"entry_class\":\"fresh\"}";

        let parsed = parse_step_output_value_detailed(raw, Some(&schema));

        assert_eq!(parsed.value, Value::String(raw.into()));
    }

    #[test]
    fn a_failing_container_blocks_the_prose_rung() {
        let schema = enum_tiny_schema(
            "entry_class",
            &["own", "fresh", "tip", "fallback", "reject"],
        );
        let raw = "the entry class is own: {\"wrong_prop\": 1}";

        let parsed = parse_step_output_value_detailed(raw, Some(&schema));

        // A container exists (it merely fails the schema): the current
        // extraction behavior holds byte-for-byte — the prose rung does not
        // fire on top of it.
        assert_eq!(parsed.value, Value::String(raw.into()));
        assert_eq!(parsed.recovered_enum_token, None);
    }

    #[test]
    fn prose_recovery_never_fires_without_a_declared_schema() {
        let raw = "the entry class is own";

        let parsed = parse_step_output_value_detailed(raw, None);

        assert_eq!(parsed.value, Value::String(raw.into()));
        assert_eq!(parsed.recovered_enum_token, None);
    }

    #[test]
    fn run_data_ignores_failed_and_skipped_outputs() {
        let results = vec![
            SopStepResult {
                effective_agent: None,
                step_number: 1,
                status: SopStepStatus::Failed,
                output: r#"{"failed":true}"#.into(),
                started_at: "now".into(),
                completed_at: Some("now".into()),
                tool_calls: Vec::new(),
            },
            SopStepResult {
                effective_agent: None,
                step_number: 2,
                status: SopStepStatus::Skipped,
                output: r#"{"skipped":true}"#.into(),
                started_at: "now".into(),
                completed_at: Some("now".into()),
                tool_calls: Vec::new(),
            },
            SopStepResult {
                effective_agent: None,
                step_number: 3,
                status: SopStepStatus::Completed,
                output: r#"{"ok":true}"#.into(),
                started_at: "now".into(),
                completed_at: Some("now".into()),
                tool_calls: Vec::new(),
            },
        ];

        let data = RunData::from_step_results(&results);

        assert!(!data.outputs.contains_key(&1));
        assert!(!data.outputs.contains_key(&2));
        assert_eq!(data.outputs[&3]["ok"], true);
    }
}
