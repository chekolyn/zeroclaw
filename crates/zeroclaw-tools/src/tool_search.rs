//! `tool_search` discovery for built-in and MCP tool schemas.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::mcp_deferred::{ActivatedToolSet, DeferredMcpToolSet, deferred_search_score};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult, ToolSpec};
use zeroclaw_config::schema::McpProfileGrant;

/// Default maximum number of search results.
const DEFAULT_MAX_RESULTS: usize = 5;

type ActivationHook = Arc<dyn Fn(Arc<dyn Tool>) + Send + Sync>;

#[derive(Clone, Default)]
pub struct ToolAccessPolicy {
    pub allowed: Option<Vec<String>>,
    pub caller_allowed: Option<Vec<String>>,
    pub denied: Option<Vec<String>>,
    /// The per-profile MCP grant map (`SecurityPolicy.mcp_servers`). `Some`
    /// only when the map carries at least one entry: a server WITH an entry
    /// is deny-by-default under [`Self::is_tool_allowed`]; servers without
    /// an entry keep the legacy `<server>__<tool>` auto-admit.
    pub mcp_servers: Option<HashMap<String, McpProfileGrant>>,
}

impl ToolAccessPolicy {
    pub fn from_security(
        allowed_tools: Option<&[String]>,
        excluded_tools: Option<&[String]>,
        caller_allowed: Option<&[String]>,
        mcp_servers: Option<&HashMap<String, McpProfileGrant>>,
    ) -> Option<Self> {
        let mut policy = Self::default();
        if let Some(list) = allowed_tools {
            policy.allowed = Some(list.to_vec());
        }
        if let Some(caller) = caller_allowed {
            policy.caller_allowed = Some(caller.to_vec());
        }
        if let Some(list) = excluded_tools {
            policy.denied = Some(list.to_vec());
        }
        // An empty map governs nothing — every server keeps the legacy
        // behavior — so it must not conjure a policy for an otherwise-gateless
        // profile (`None` stays "no policy").
        if let Some(map) = mcp_servers
            && !map.is_empty()
        {
            policy.mcp_servers = Some(map.clone());
        }
        if policy.allowed.is_some()
            || policy.caller_allowed.is_some()
            || policy.denied.is_some()
            || policy.mcp_servers.is_some()
        {
            Some(policy)
        } else {
            None
        }
    }

    /// The per-profile `mcp_servers` denial reason for `name`, when the map
    /// governs the name's server and does not grant it. `None` when the name
    /// is not map-governed (no `<server>__<tool>` shape, no map, no entry
    /// for the server) or the grant admits it.
    pub fn mcp_grant_denial_reason(&self, name: &str) -> Option<String> {
        let (server, tool) = name.split_once("__")?;
        let grant = self.mcp_servers.as_ref()?.get(server)?;
        let granted = grant.tools.iter().any(|t| t == "*" || t == tool);
        if granted {
            return None;
        }
        Some(format!(
            "`{name}` is not on this profile's mcp_servers grant for `{server}`."
        ))
    }

    pub fn is_tool_allowed(&self, name: &str) -> bool {
        // 1. Deny-list always wins.
        let in_deny = self
            .denied
            .as_ref()
            .is_some_and(|list| list.iter().any(|t| t == name));
        if in_deny {
            return false;
        }

        // 2. The per-profile `mcp_servers` grant (composition order:
        //    denied > mcp_servers > allowed > caller_allowed). For a server
        //    WITH a map entry the grant is deny-by-default: the tool passes
        //    only when the entry lists its bare tool part (the name splits
        //    at the FIRST `__`, the fork's own `<server>__<tool>` convention)
        //    or the entry carries the explicit `"*"` wildcard. A server with
        //    NO entry keeps the legacy behavior below. The map can only veto:
        //    a granted tool still passes the remaining gates, so
        //    `deny_all_tools` (the explicit empty allow-list) and the
        //    caller-supplied per-run list still narrow it.
        if let Some((server, tool)) = name.split_once("__")
            && let Some(grant) = self.mcp_servers.as_ref().and_then(|map| map.get(server))
        {
            let granted = grant.tools.iter().any(|t| t == "*" || t == tool);
            if !granted {
                return false;
            }
        }

        // 3. Risk-profile gate: MCP `<server>__<tool>` names are auto-admitted
        // when the list is non-empty. An explicit empty list (`Some(vec![])`)
        // still means "deny everything".
        let risk_ok = match self.allowed.as_ref() {
            None => true,
            Some(list) if list.is_empty() => false,
            Some(list) => list.iter().any(|t| t == name) || name.contains("__"),
        };
        if !risk_ok {
            return false;
        }

        // 4. The caller-supplied per-run list.
        match self.caller_allowed.as_ref() {
            None => true,
            Some(list) => list.iter().any(|t| t == name),
        }
    }
}

/// Fetches full schemas for admitted deferred built-ins and MCP tools.
pub struct ToolSearchTool {
    // This is the executable deferred registry, not a cached grants record.
    // Session admission may only remove entries from it.
    deferred: Mutex<Option<DeferredMcpToolSet>>,
    activated: Arc<Mutex<ActivatedToolSet>>,
    access_policy: Option<ToolAccessPolicy>,
    activation_hook: Option<ActivationHook>,
}

impl ToolSearchTool {
    /// The native creator's callable name; ownership is claimed on creation.
    pub const NAME: &'static str = "tool_search";

    pub fn new(deferred: DeferredMcpToolSet, activated: Arc<Mutex<ActivatedToolSet>>) -> Self {
        Self {
            deferred: Mutex::new(Some(deferred)),
            activated,
            access_policy: None,
            activation_hook: None,
        }
    }

    /// Built-in schema discovery requires no MCP registry or executable wrapper.
    pub fn for_builtin_schemas(activated: Arc<Mutex<ActivatedToolSet>>) -> Self {
        Self {
            deferred: Mutex::new(None),
            activated,
            access_policy: None,
            activation_hook: None,
        }
    }

    pub fn with_access_policy(mut self, policy: ToolAccessPolicy) -> Self {
        self.access_policy = Some(policy);
        self
    }

    pub fn with_activation_hook(mut self, hook: ActivationHook) -> Self {
        self.activation_hook = Some(hook);
        self
    }

    /// Remove revoked schemas and activated tools together. Recovering a
    /// poisoned guard is safe here because every surviving entry is checked
    /// against the current ceiling before execution can resume.
    pub fn narrow_to_caller(&self, allowed: &[String]) {
        let mut deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(deferred) = deferred.as_mut() {
            deferred
                .stubs
                .retain(|stub| allowed.contains(&stub.prefixed_name));
        }
        self.activated
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain_allowed(allowed);
    }

    /// Advertise only still-loadable, not-yet-activated tools, from the same
    /// registry used by both keyword search and exact selection.
    pub fn deferred_prompt_section(&self) -> String {
        let deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
        let activated = self.activated.lock().unwrap_or_else(|e| e.into_inner());
        let names = activated
            .tool_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut section = match self.access_policy.as_ref() {
            Some(policy) => {
                crate::mcp_deferred::build_deferred_builtin_tools_section(&activated, Some(policy))
            }
            None => activated.deferred_builtin_prompt_section(),
        };
        if let Some(deferred) = deferred.as_ref() {
            section.push_str(
                &crate::mcp_deferred::build_deferred_tools_section_excluding(
                    deferred,
                    self.access_policy.as_ref(),
                    &names,
                ),
            );
        }
        section
    }

    fn is_allowed(&self, tool_name: &str) -> bool {
        self.access_policy
            .as_ref()
            .is_none_or(|p| p.is_tool_allowed(tool_name))
    }

    fn notify_activated(&self, tools: Vec<Arc<dyn Tool>>) {
        if let Some(hook) = &self.activation_hook {
            for tool in tools {
                hook(tool);
            }
        }
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Fetch full schemas for deferred tools"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string"
                },
                "max_results": {
                    "type": "number"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim();

        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|v| usize::try_from(v).unwrap_or(DEFAULT_MAX_RESULTS))
            .unwrap_or(DEFAULT_MAX_RESULTS);

        if query.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("query parameter is required".into()),
            });
        }

        // Parse query mode
        if let Some(names_str) = query.strip_prefix("select:") {
            // Exact selection mode
            let names: Vec<&str> = names_str.split(',').map(str::trim).collect();
            return self.select_tools(&names);
        }

        self.search_tools(query, max_results)
    }
}

impl ToolSearchTool {
    /// Select built-in metadata or activate an MCP wrapper. Only MCP tools
    /// enter the executable set and the activation hook's notification list.
    fn load_schema(
        deferred: Option<&DeferredMcpToolSet>,
        activated: &mut ActivatedToolSet,
        name: &str,
        newly_activated: &mut Vec<Arc<dyn Tool>>,
    ) -> Option<ToolSpec> {
        if let Some(spec) = activated.select_builtin_schema(name) {
            return Some(spec);
        }
        let deferred = deferred?;
        let spec = deferred.tool_spec(name)?;
        if !activated.is_activated(name)
            && let Some(tool) = deferred.activate(name)
        {
            let tool: Arc<dyn Tool> = Arc::from(tool);
            activated.activate(name.to_string(), Arc::clone(&tool));
            newly_activated.push(tool);
        }
        Some(spec)
    }

    fn search_tools(&self, query: &str, max_results: usize) -> anyhow::Result<ToolResult> {
        let deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
        let mut guard = match self.activated.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "query": query,
                            "mode": "keyword_search",
                        })),
                    "tool_search activated-tool lock poisoned during keyword activation; recovering guard"
                );
                poisoned.into_inner()
            }
        };
        let terms: Vec<_> = query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        // Rank the combined catalog before capping. Denied names never occupy
        // a result slot. MCP ties preserve the registry's existing order.
        let mut results = Vec::new();
        if let Some(deferred) = deferred.as_ref() {
            for stub in &deferred.stubs {
                let hits = deferred_search_score(&stub.prefixed_name, &stub.description, &terms);
                if hits > 0 && self.is_allowed(&stub.prefixed_name) {
                    results.push((stub.prefixed_name.clone(), hits));
                }
            }
        }
        let mut builtin_specs: Vec<_> = guard.deferred_builtin_specs().collect();
        builtin_specs.sort_by(|a, b| a.name.cmp(&b.name));
        for spec in builtin_specs {
            let hits = deferred_search_score(&spec.name, &spec.description, &terms);
            if hits > 0 && self.is_allowed(&spec.name) {
                results.push((spec.name.clone(), hits));
            }
        }
        results.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        if results.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No matching deferred tools found.".into(),
                error: None,
            });
        }
        let mut output = String::from("<functions>\n");
        let mut newly_activated = Vec::new();
        let mut returned_names = std::collections::HashSet::new();
        for (name, _) in &results {
            if returned_names.len() >= max_results {
                break;
            }
            if returned_names.contains(name) {
                continue;
            }
            if let Some(spec) =
                Self::load_schema(deferred.as_ref(), &mut guard, name, &mut newly_activated)
            {
                write_schema(&mut output, &spec)?;
                returned_names.insert(name.clone());
            }
        }
        output.push_str("</functions>\n");
        drop(guard);
        drop(deferred);
        let activated_count = newly_activated.len();
        self.notify_activated(newly_activated);

        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "tool_search: query={query:?}, matched={}, activated={activated_count}",
                results.len()
            )
        );

        Ok(ToolResult {
            success: true,
            output: output.into(),
            error: None,
        })
    }

    fn select_tools(&self, names: &[&str]) -> anyhow::Result<ToolResult> {
        let deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
        let mut output = String::from("<functions>\n");
        let mut not_found = Vec::new();
        let mut newly_activated = Vec::new();
        let mut returned_names = std::collections::HashSet::new();
        let mut guard = match self.activated.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "requested_names": names,
                            "mode": "select",
                        })),
                    "tool_search activated-tool lock poisoned during select activation; recovering guard"
                );
                poisoned.into_inner()
            }
        };

        for name in names {
            if name.is_empty() || returned_names.contains(*name) {
                continue;
            }
            if !self.is_allowed(name) {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    &format!("tool_search select: '{}' denied by access policy", name)
                );
                not_found.push(*name);
                continue;
            }
            match Self::load_schema(deferred.as_ref(), &mut guard, name, &mut newly_activated) {
                Some(spec) => {
                    write_schema(&mut output, &spec)?;
                    returned_names.insert(*name);
                }
                None => {
                    not_found.push(*name);
                }
            }
        }

        output.push_str("</functions>\n");
        drop(guard);
        drop(deferred);
        let activated_count = newly_activated.len();
        self.notify_activated(newly_activated);

        if !not_found.is_empty() {
            let _ = write!(output, "\nNot found: {}", not_found.join(", "));
        }

        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            &format!(
                "tool_search select: requested={}, activated={activated_count}, not_found={}",
                names.len(),
                not_found.len()
            )
        );

        Ok(ToolResult {
            success: true,
            output: output.into(),
            error: None,
        })
    }
}

/// Keep the existing function envelope while escaping descriptions as JSON
/// and retaining any declared structured-output schema and parameter domains.
fn write_schema(output: &mut String, spec: &ToolSpec) -> serde_json::Result<()> {
    let _ = write!(
        output,
        "<function>{{\"name\": {}, \"description\": {}, \"parameters\": {}",
        serde_json::to_string(&spec.name)?,
        serde_json::to_string(&spec.description)?,
        spec.parameters
    );
    if let Some(schema) = &spec.output {
        let _ = write!(output, ", \"output\": {schema}");
    }
    if !spec.param_domains.is_empty() {
        let _ = write!(
            output,
            ", \"param_domains\": {}",
            serde_json::to_string(&spec.param_domains)?
        );
    }
    output.push_str("}</function>\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_client::McpRegistry;
    use crate::mcp_deferred::{
        DeferredMcpToolStub, build_deferred_tools_section, build_deferred_tools_section_filtered,
    };
    use crate::mcp_protocol::McpToolDef;

    #[tokio::test]
    async fn tool_search_eager_schema_is_shape_only() {
        // The eager-served tools carry the schema SHAPE on the wire; property
        // descriptions are stripped — the deferred-fetch usage prose lives in
        // the deferred section header the model already sees.
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![]).await,
            std::sync::Arc::new(std::sync::Mutex::new(ActivatedToolSet::new())),
        );
        assert!(
            tool.description().chars().count() <= 60,
            "eager tool description must stay a short clause: {}",
            tool.description()
        );
        let schema = tool.parameters_schema();
        for (name, prop) in schema["properties"].as_object().expect("properties object") {
            assert!(
                prop.get("description").is_none(),
                "property `{name}` must not carry a description on the eager wire"
            );
            assert!(
                prop.get("default").is_none(),
                "property `{name}` must not carry a default on the eager wire"
            );
        }
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("query"))
        );
    }

    async fn make_deferred_set(stubs: Vec<DeferredMcpToolStub>) -> DeferredMcpToolSet {
        let registry = Arc::new(McpRegistry::connect_all(&[]).await.unwrap());
        DeferredMcpToolSet {
            stubs,
            registry,
            security: Arc::new(zeroclaw_config::policy::SecurityPolicy::default()),
        }
    }

    fn make_stub(name: &str, desc: &str) -> DeferredMcpToolStub {
        let def = McpToolDef {
            name: name.to_string(),
            description: Some(desc.to_string()),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        };
        DeferredMcpToolStub::new(name.to_string(), def)
    }

    fn assert_poisoned_activated_contains(
        activated: &Arc<Mutex<ActivatedToolSet>>,
        tool_name: &str,
    ) {
        let guard = activated
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(guard.is_activated(tool_name));
    }

    fn builtin_spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec::new(
            name,
            description,
            serde_json::json!({
                "type": "object",
                "properties": {"event_date": {"type": "string", "format": "date"}},
                "required": ["event_date"]
            }),
        )
    }

    fn returned_schemas(result: &ToolResult) -> Vec<serde_json::Value> {
        result
            .output
            .lines()
            .filter_map(|line| line.strip_prefix("<function>")?.strip_suffix("</function>"))
            .map(|schema| serde_json::from_str(schema).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn builtin_only_search_returns_full_schema_without_executable_activation() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let mut spec = builtin_spec(
            "calendar",
            "Manage events.\nUse the \"calendar\" path C:\\events.",
        );
        spec.output = Some(serde_json::json!({
            "type": "object", "properties": {"event_id": {"type": "string"}}
        }));
        activated
            .lock()
            .unwrap()
            .set_deferred_builtin_specs(vec![spec.clone()]);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_hook = Arc::clone(&seen);
        let tool = ToolSearchTool::for_builtin_schemas(Arc::clone(&activated))
            .with_activation_hook(Arc::new(move |tool| {
                seen_hook.lock().unwrap().push(tool.name().to_string());
            }));
        assert!(tool.deferred.lock().unwrap().is_none());
        let prompt = tool.deferred_prompt_section();
        assert!(prompt.contains("calendar - Manage events."));
        assert!(!prompt.contains("event_date"));

        // The full description remains searchable even though the compact
        // prompt advertises only its first line.
        let result = tool
            .execute(serde_json::json!({"query": "C:\\events"}))
            .await
            .unwrap();
        let schemas = returned_schemas(&result);
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0], serde_json::to_value(&spec).unwrap());
        let guard = activated.lock().unwrap();
        assert!(guard.is_builtin_schema_exposed("calendar"));
        assert!(!guard.is_activated("calendar"));
        assert!(guard.get("calendar").is_none());
        assert!(guard.get_resolved("calendar").is_none());
        assert!(guard.tool_names().is_empty());
        assert!(guard.tool_specs().is_empty());
        drop(guard);
        assert!(seen.lock().unwrap().is_empty());
        assert!(tool.deferred_prompt_section().is_empty());
    }

    #[tokio::test]
    async fn builtin_exact_selection_preserves_explicit_names_and_recovers_poisoned_lock() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        activated.lock().unwrap().set_deferred_builtin_specs(vec![
            builtin_spec("calendar", "Calendar events"),
            builtin_spec("weather", "Weather forecast"),
        ]);
        let poison = Arc::clone(&activated);
        assert!(
            std::thread::spawn(move || {
                let _guard = poison.lock().unwrap();
                panic!("test poison");
            })
            .join()
            .is_err()
        );
        let tool = ToolSearchTool::for_builtin_schemas(Arc::clone(&activated));
        let result = tool
            .execute(serde_json::json!({
                "query": "select:calendar,calendar,weather", "max_results": 1
            }))
            .await
            .unwrap();
        assert_eq!(returned_schemas(&result).len(), 2);
        {
            let guard = activated.lock().unwrap_or_else(|e| e.into_inner());
            assert!(guard.is_builtin_schema_exposed("calendar"));
            assert!(guard.is_builtin_schema_exposed("weather"));
            assert!(guard.tool_specs().is_empty());
        }
        assert!(tool.deferred_prompt_section().is_empty());
        let result = tool
            .execute(serde_json::json!({"query": "select:weather"}))
            .await
            .unwrap();
        assert_eq!(returned_schemas(&result)[0]["name"], "weather");
        assert!(tool.deferred_prompt_section().is_empty());
    }

    #[tokio::test]
    async fn mixed_search_and_selection_share_result_limit_and_only_activate_mcp() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        activated.lock().unwrap().set_deferred_builtin_specs(vec![
            builtin_spec("calendar", "Calendar events search"),
            builtin_spec("weather", "Weather search"),
        ]);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_hook = Arc::clone(&seen);
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("remote__calendar", "Calendar search")]).await,
            Arc::clone(&activated),
        )
        .with_activation_hook(Arc::new(move |tool| {
            seen_hook.lock().unwrap().push(tool.name().to_string());
        }));
        let prompt = tool.deferred_prompt_section();
        assert!(prompt.contains("calendar - Calendar events search"));
        assert!(prompt.contains("remote__calendar - Calendar search"));
        let result = tool
            .execute(serde_json::json!({"query": "calendar events search", "max_results": 2}))
            .await
            .unwrap();
        let schemas = returned_schemas(&result);
        assert_eq!(schemas.len(), 2);
        assert_eq!(schemas[0]["name"], "calendar");
        assert_eq!(schemas[1]["name"], "remote__calendar");
        {
            let guard = activated.lock().unwrap();
            assert!(guard.is_builtin_schema_exposed("calendar"));
            assert!(!guard.is_builtin_schema_exposed("weather"));
            assert!(guard.is_activated("remote__calendar"));
            assert_eq!(guard.tool_names(), vec!["remote__calendar"]);
        }
        assert_eq!(seen.lock().unwrap().as_slice(), ["remote__calendar"]);

        let result = tool
            .execute(serde_json::json!({
                "query": "select:weather,remote__calendar,calendar", "max_results": 1
            }))
            .await
            .unwrap();
        assert_eq!(returned_schemas(&result).len(), 3);
        assert_eq!(returned_schemas(&result)[0]["name"], "weather");
        assert_eq!(seen.lock().unwrap().as_slice(), ["remote__calendar"]);
        assert!(tool.deferred_prompt_section().is_empty());
    }

    #[tokio::test]
    async fn builtin_policy_and_principal_narrowing_remove_discovery_and_selection() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        activated.lock().unwrap().set_deferred_builtin_specs(vec![
            builtin_spec("calendar", "Calendar tool"),
            builtin_spec("weather", "Weather tool"),
            builtin_spec("blocked", "Blocked tool"),
            builtin_spec("outside", "Outside caller tool"),
            builtin_spec("risk_blocked", "Outside risk tool"),
        ]);
        let policy = ToolAccessPolicy {
            allowed: Some(vec![
                "calendar".into(),
                "weather".into(),
                "blocked".into(),
                "outside".into(),
            ]),
            caller_allowed: Some(vec![
                "calendar".into(),
                "weather".into(),
                "blocked".into(),
                "risk_blocked".into(),
            ]),
            denied: Some(vec!["blocked".into()]),
            mcp_servers: None,
        };
        let tool =
            ToolSearchTool::for_builtin_schemas(Arc::clone(&activated)).with_access_policy(policy);
        let prompt = tool.deferred_prompt_section();
        assert!(prompt.contains("calendar"));
        for denied in ["blocked", "outside", "risk_blocked"] {
            assert!(!prompt.contains(denied));
        }
        let result = tool
            .execute(serde_json::json!({"query": "tool", "max_results": 1}))
            .await
            .unwrap();
        assert_eq!(returned_schemas(&result).len(), 1);
        assert_eq!(returned_schemas(&result)[0]["name"], "calendar");
        let denied = tool
            .execute(serde_json::json!({"query": "select:blocked,outside,risk_blocked"}))
            .await
            .unwrap();
        assert!(returned_schemas(&denied).is_empty());
        assert!(
            !activated
                .lock()
                .unwrap()
                .is_builtin_schema_exposed("blocked")
        );

        tool.narrow_to_caller(&["weather".to_string()]);
        let prompt = tool.deferred_prompt_section();
        assert!(prompt.contains("weather"));
        assert!(!prompt.contains("calendar"));
        for query in ["calendar", "select:calendar"] {
            let result = tool
                .execute(serde_json::json!({"query": query}))
                .await
                .unwrap();
            assert!(returned_schemas(&result).is_empty());
        }
        let guard = activated.lock().unwrap();
        assert_eq!(
            guard.hidden_builtin_names(),
            std::collections::HashSet::from(["weather".to_string()])
        );
        assert!(guard.tool_specs().is_empty());
    }

    #[tokio::test]
    async fn principal_narrowing_keeps_positive_prompt_and_blocks_reactivation() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![
                make_stub("mcp__keep", "retained tool"),
                make_stub("mcp__revoke", "revoked tool"),
            ])
            .await,
            Arc::clone(&activated),
        );
        assert!(tool.deferred_prompt_section().contains("mcp__revoke"));
        tool.execute(serde_json::json!({"query":"select:mcp__revoke"}))
            .await
            .unwrap();
        assert!(activated.lock().unwrap().is_activated("mcp__revoke"));
        let poison = Arc::clone(&activated);
        assert!(
            std::thread::spawn(move || {
                let _guard = poison.lock().unwrap();
                panic!("test poison");
            })
            .join()
            .is_err()
        );
        tool.narrow_to_caller(&["mcp__keep".into()]);
        let prompt = tool.deferred_prompt_section();
        assert!(prompt.contains("mcp__keep") && prompt.contains("tool_search"));
        assert!(!prompt.contains("mcp__revoke"));
        for query in ["select:mcp__revoke", "revoked"] {
            let result = tool
                .execute(serde_json::json!({"query":query}))
                .await
                .unwrap();
            assert!(!result.output.contains("<function>{"));
            assert!(
                !activated
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_activated("mcp__revoke")
            );
        }
        tool.execute(serde_json::json!({"query":"select:mcp__keep"}))
            .await
            .unwrap();
        assert!(
            activated
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_activated("mcp__keep")
        );
        assert!(tool.deferred_prompt_section().is_empty());
    }

    #[tokio::test]
    async fn tool_metadata() {
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![]).await,
            Arc::new(Mutex::new(ActivatedToolSet::new())),
        );
        assert_eq!(tool.name(), "tool_search");
        assert!(!tool.description().is_empty());
        assert!(tool.parameters_schema()["properties"]["query"].is_object());
    }

    #[tokio::test]
    async fn empty_query_returns_error() {
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![]).await,
            Arc::new(Mutex::new(ActivatedToolSet::new())),
        );
        let result = tool
            .execute(serde_json::json!({"query": ""}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn select_nonexistent_tool_reports_not_found() {
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![]).await,
            Arc::new(Mutex::new(ActivatedToolSet::new())),
        );
        let result = tool
            .execute(serde_json::json!({"query": "select:nonexistent"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Not found"));
    }

    #[tokio::test]
    async fn keyword_search_no_matches() {
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("fs__read", "Read file")]).await,
            Arc::new(Mutex::new(ActivatedToolSet::new())),
        );
        let result = tool
            .execute(serde_json::json!({"query": "zzzzz_nonexistent"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("No matching"));
    }

    #[tokio::test]
    async fn keyword_search_finds_match() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("fs__read", "Read a file from disk")]).await,
            Arc::clone(&activated),
        );
        let result = tool
            .execute(serde_json::json!({"query": "read file"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("<function>"));
        assert!(result.output.contains("fs__read"));
        // Tool should now be activated
        assert!(activated.lock().unwrap().is_activated("fs__read"));
    }

    #[tokio::test]
    async fn keyword_search_recovers_poisoned_activated_lock() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let poisoned = Arc::clone(&activated);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().expect("test mutex should lock");
            panic!("poison activated-tools lock");
        })
        .join();
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("fs__read", "Read a file from disk")]).await,
            Arc::clone(&activated),
        );

        let result = tool
            .execute(serde_json::json!({"query": "read file"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("<function>"));
        assert!(result.output.contains("fs__read"));
        assert_poisoned_activated_contains(&activated, "fs__read");
    }

    #[tokio::test]
    async fn multiple_servers_stubs_all_searchable() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let stubs = vec![
            make_stub("server_a__list_files", "List files on server A"),
            make_stub("server_a__read_file", "Read file on server A"),
            make_stub("server_b__query_db", "Query database on server B"),
            make_stub("server_b__insert_row", "Insert row on server B"),
        ];
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated));

        // Search should find tools across both servers
        let result = tool
            .execute(serde_json::json!({"query": "file"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("server_a__list_files"));
        assert!(result.output.contains("server_a__read_file"));

        // Server B tools should also be searchable
        let result = tool
            .execute(serde_json::json!({"query": "database query"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("server_b__query_db"));
    }

    #[tokio::test]
    async fn select_activates_and_persists_across_calls() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let stubs = vec![
            make_stub("srv__tool_a", "Tool A"),
            make_stub("srv__tool_b", "Tool B"),
        ];
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated));

        // Activate tool_a
        let result = tool
            .execute(serde_json::json!({"query": "select:srv__tool_a"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(activated.lock().unwrap().is_activated("srv__tool_a"));
        assert!(!activated.lock().unwrap().is_activated("srv__tool_b"));

        // Activate tool_b in a separate call
        let result = tool
            .execute(serde_json::json!({"query": "select:srv__tool_b"}))
            .await
            .unwrap();
        assert!(result.success);

        // Both should remain activated
        let guard = activated.lock().unwrap();
        assert!(guard.is_activated("srv__tool_a"));
        assert!(guard.is_activated("srv__tool_b"));
        assert_eq!(guard.tool_specs().len(), 2);
    }

    #[tokio::test]
    async fn select_recovers_poisoned_activated_lock() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let poisoned = Arc::clone(&activated);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().expect("test mutex should lock");
            panic!("poison activated-tools lock");
        })
        .join();
        let stubs = vec![
            make_stub("srv__tool_a", "Tool A"),
            make_stub("srv__tool_b", "Tool B"),
        ];
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated));

        let result = tool
            .execute(serde_json::json!({"query": "select:srv__tool_a"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("<function>"));
        assert!(result.output.contains("srv__tool_a"));
        assert_poisoned_activated_contains(&activated, "srv__tool_a");
    }

    #[tokio::test]
    async fn reactivation_is_idempotent() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("srv__tool", "A tool")]).await,
            Arc::clone(&activated),
        );

        tool.execute(serde_json::json!({"query": "select:srv__tool"}))
            .await
            .unwrap();
        tool.execute(serde_json::json!({"query": "select:srv__tool"}))
            .await
            .unwrap();

        assert_eq!(activated.lock().unwrap().tool_specs().len(), 1);
    }

    #[tokio::test]
    async fn activation_hook_receives_newly_activated_tools_once() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_hook = Arc::clone(&seen);
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("srv__tool", "A tool")]).await,
            Arc::clone(&activated),
        )
        .with_activation_hook(Arc::new(move |tool| {
            seen_hook.lock().unwrap().push(tool.name().to_string());
        }));

        tool.execute(serde_json::json!({"query": "select:srv__tool"}))
            .await
            .unwrap();
        tool.execute(serde_json::json!({"query": "select:srv__tool"}))
            .await
            .unwrap();

        assert_eq!(seen.lock().unwrap().as_slice(), ["srv__tool"]);
    }

    #[test]
    fn policy_none_is_unrestricted() {
        let p = ToolAccessPolicy::default();
        assert!(p.is_tool_allowed("shell"));
        assert!(p.is_tool_allowed("anything"));
    }

    #[test]
    fn policy_allowlist_admits_only_listed() {
        let p = ToolAccessPolicy {
            allowed: Some(vec!["shell".into(), "file_read".into()]),
            ..ToolAccessPolicy::default()
        };
        assert!(p.is_tool_allowed("shell"));
        assert!(!p.is_tool_allowed("file_write"));
    }

    #[test]
    fn policy_denylist_rejects_listed() {
        let p = ToolAccessPolicy {
            denied: Some(vec!["shell".into()]),
            ..ToolAccessPolicy::default()
        };
        assert!(!p.is_tool_allowed("shell"));
        assert!(p.is_tool_allowed("file_read"));
    }

    #[test]
    fn policy_deny_overrides_allow() {
        let p = ToolAccessPolicy {
            allowed: Some(vec!["shell".into(), "file_read".into()]),
            denied: Some(vec!["shell".into()]),
            ..ToolAccessPolicy::default()
        };
        assert!(!p.is_tool_allowed("shell"));
        assert!(p.is_tool_allowed("file_read"));
    }

    #[tokio::test]
    async fn policy_filters_keyword_search_results() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let stubs = vec![
            make_stub("srv__allowed_tool", "An allowed tool"),
            make_stub("srv__blocked_tool", "A blocked tool"),
            make_stub("srv__unlisted_tool", "An unlisted tool"),
            make_stub("other__legacy_tool", "A legacy tool"),
        ];
        // The per-profile `mcp_servers` grant for `srv` is deny-by-default:
        // `srv__unlisted_tool` is dropped by the MAP (not the deny-list),
        // while `other` has no map entry and keeps the legacy auto-admit.
        let map = grant_map(&[("srv", &["allowed_tool"])]);
        let policy = ToolAccessPolicy {
            denied: Some(vec!["srv__blocked_tool".into()]),
            mcp_servers: Some(map),
            ..ToolAccessPolicy::default()
        };
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated))
            .with_access_policy(policy);

        let result = tool
            .execute(serde_json::json!({"query": "tool"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("srv__allowed_tool"));
        assert!(!result.output.contains("srv__blocked_tool"));
        assert!(
            !result.output.contains("srv__unlisted_tool"),
            "a mapped server's unlisted tool must be denied by the mcp_servers grant"
        );
        assert!(
            result.output.contains("other__legacy_tool"),
            "a server with no map entry keeps the legacy auto-admit"
        );
        assert!(!activated.lock().unwrap().is_activated("srv__blocked_tool"));
        assert!(!activated.lock().unwrap().is_activated("srv__unlisted_tool"));
    }

    #[tokio::test]
    async fn policy_denied_tool_does_not_consume_max_results_slot() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        // "denied_tool" ranks higher (more keyword matches) but is blocked.
        // "allowed_tool" ranks lower but should still be returned with max_results=1.
        let stubs = vec![
            make_stub("srv__denied_tool", "tool for searching files"),
            make_stub("srv__allowed_tool", "tool for files"),
        ];
        let policy = ToolAccessPolicy {
            denied: Some(vec!["srv__denied_tool".into()]),
            ..ToolAccessPolicy::default()
        };
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated))
            .with_access_policy(policy);

        let result = tool
            .execute(serde_json::json!({"query": "searching files", "max_results": 1}))
            .await
            .unwrap();
        assert!(result.success);
        // The allowed tool should be returned even though max_results=1
        // and the denied tool ranked higher.
        assert!(result.output.contains("srv__allowed_tool"));
        assert!(!result.output.contains("srv__denied_tool"));
        assert!(activated.lock().unwrap().is_activated("srv__allowed_tool"));
    }

    #[tokio::test]
    async fn policy_filters_select_results() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let stubs = vec![
            make_stub("srv__ok", "OK tool"),
            make_stub("srv__nope", "Blocked tool"),
            make_stub("srv__unlisted", "Unlisted tool"),
            make_stub("other__legacy", "Legacy tool"),
        ];
        // Runtime-discovered MCP tools (names containing "__") are governed
        // by the per-profile `mcp_servers` grant when the profile carries an
        // entry for the server (deny-by-default); servers without an entry
        // keep the legacy allow-list auto-admit. The operator-visible way to
        // block a specific MCP tool is the map entry (or the deny-list, the
        // `excluded_tools` equivalent, which still wins over the map).
        // See `ToolAccessPolicy::is_tool_allowed` and
        let map = grant_map(&[("srv", &["ok"])]);
        let policy = ToolAccessPolicy {
            allowed: Some(vec!["srv__ok".into()]),
            denied: Some(vec!["srv__nope".into()]),
            mcp_servers: Some(map),
            ..ToolAccessPolicy::default()
        };
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated))
            .with_access_policy(policy);

        let result = tool
            .execute(serde_json::json!({"query": "select:srv__ok,srv__nope,srv__unlisted,other__legacy"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("srv__ok"));
        assert!(!result.output.contains("\"name\": \"srv__nope\""));
        assert!(
            !result.output.contains("\"name\": \"srv__unlisted\""),
            "the mapped server's unlisted tool must be denied by the map — \
             the legacy allow-list auto-admit would have passed it"
        );
        assert!(
            result.output.contains("other__legacy"),
            "the no-entry legacy path keeps the auto-admit"
        );
        assert!(result.output.contains("Not found"));
        assert!(!activated.lock().unwrap().is_activated("srv__nope"));
        assert!(!activated.lock().unwrap().is_activated("srv__unlisted"));
    }

    #[tokio::test]
    async fn prompt_section_and_tool_search_see_same_filtered_set() {
        // The prompt-side
        // `build_deferred_tools_section_filtered` and the runtime
        // `ToolSearchTool` constructor (built from the same
        // `filtered_deferred` value, the single source of
        // truth) must agree on which tools are visible. If the
        // prompt and the search tool each applied the access policy
        // independently, a denied tool could leak through one of them.
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let set = make_deferred_set(vec![
            make_stub("srv__visible", "Visible tool"),
            make_stub("srv__hidden", "Hidden tool"),
        ])
        .await;
        let policy = ToolAccessPolicy {
            denied: Some(vec!["srv__hidden".into()]),
            ..ToolAccessPolicy::default()
        };

        // Single source of truth: build the filtered set once and
        // feed it to both consumers.
        let filtered = set.filter_by_policy(Some(&policy));

        // Prompt-side: the section must not mention the denied tool.
        let section = build_deferred_tools_section_filtered(&filtered, Some(&policy));
        assert!(section.contains("srv__visible"));
        assert!(!section.contains("srv__hidden"));

        // Runtime-side: the search tool must not return the denied
        // tool's schema on a keyword match.
        let tool = ToolSearchTool::new(filtered, Arc::clone(&activated)).with_access_policy(policy);
        let result = tool
            .execute(serde_json::json!({"query": "tool"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("srv__visible"));
        assert!(!result.output.contains("srv__hidden"));
        assert!(!activated.lock().unwrap().is_activated("srv__hidden"));
    }

    #[tokio::test]
    async fn omission_safety_pre_filtered_set_suffices_without_reapplied_policy() {
        // The `filter_by_policy` helper alone (the single source of
        // truth) is sufficient to keep a denied tool schema out of
        // both consumers, even when the policy is NOT reapplied at the
        // consumer level. A missed `with_access_policy` builder step
        // on `ToolSearchTool` is therefore not load-bearing.
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let set = make_deferred_set(vec![
            make_stub("srv__visible", "Visible tool"),
            make_stub("srv__hidden", "Hidden tool"),
        ])
        .await;
        let policy = ToolAccessPolicy {
            denied: Some(vec!["srv__hidden".into()]),
            ..ToolAccessPolicy::default()
        };

        // Single source of truth: build the filtered set once.
        let filtered = set.filter_by_policy(Some(&policy));

        // Prompt-side: use the UNFILTERED prompt builder — the pre-filtered
        // set alone must keep the denied tool out. No policy re-applied.
        let section = build_deferred_tools_section(&filtered);
        assert!(section.contains("srv__visible"));
        assert!(!section.contains("srv__hidden"));

        // Runtime-side: construct ToolSearchTool WITHOUT with_access_policy.
        // The pre-filtered stub set alone must keep the denied schema out.
        let tool = ToolSearchTool::new(filtered, Arc::clone(&activated));
        let result = tool
            .execute(serde_json::json!({"query": "tool"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("srv__visible"));
        assert!(!result.output.contains("srv__hidden"));
        assert!(!activated.lock().unwrap().is_activated("srv__hidden"));
    }

    #[test]
    fn caller_allowed_per_run_gate_does_not_auto_admit_mcp_names() {
        // The risk-profile gate is wide (unrestricted), so the MCP
        // auto-admit would happily pass any `__` name. The caller-supplied
        // per-run list narrows down to a single non-MCP tool (`cron_add`).
        let policy =
            ToolAccessPolicy::from_security(None, None, Some(&["cron_add".to_string()]), None)
                .expect("caller-supplied list should produce a policy");

        assert!(
            policy.is_tool_allowed("cron_add"),
            "cron_add must pass — it is in the caller list"
        );
        assert!(
            !policy.is_tool_allowed("filesystem__write_file"),
            "MCP wrapper outside the caller list must be rejected, but \
             was admitted — the per-run gate is leaking the risk-profile \
             MCP auto-admit exception (PR #7547 review regression)"
        );
        assert!(
            !policy.is_tool_allowed("github__search"),
            "second MCP wrapper outside the caller list must also be \
             rejected (PR #7547 review regression)"
        );

        // The map does not leak past the caller gate either: a map-granted
        // tool outside the per-run list is still rejected, and a no-entry
        // server's legacy path still narrows at the caller gate.
        let map = grant_map(&[("grafana", &["search_dashboards"])]);
        let policy = ToolAccessPolicy::from_security(
            None,
            None,
            Some(&["cron_add".to_string()]),
            Some(&map),
        )
        .expect("policy");
        assert!(
            !policy.is_tool_allowed("grafana__search_dashboards"),
            "a map grant does not bypass the caller-supplied per-run list"
        );
        assert!(
            !policy.is_tool_allowed("github__search"),
            "the no-entry legacy path still narrows at the caller gate"
        );
    }

    #[test]
    fn caller_allowed_per_run_gate_narrows_after_risk_profile_auto_admit() {
        let policy = ToolAccessPolicy::from_security(
            Some(&["shell".to_string()]),
            None,
            Some(&["shell".to_string(), "github__search".to_string()]),
            None,
        )
        .expect("risk + caller lists should produce a policy");

        // `shell`: in risk allow + in caller list → admitted.
        assert!(policy.is_tool_allowed("shell"));
        // `github__search`: auto-admitted by risk MCP exception + in caller
        // list → admitted.
        assert!(policy.is_tool_allowed("github__search"));
        // `filesystem__write_file`: auto-admitted by risk MCP exception
        // (would pass the risk gate) but NOT in caller list → rejected.
        // This is the per-run narrowing the bug used to break.
        assert!(
            !policy.is_tool_allowed("filesystem__write_file"),
            "MCP wrapper not in caller list must be rejected even when \
             the risk-profile auto-admit would let it through"
        );
        // Non-MCP outside both lists: rejected.
        assert!(!policy.is_tool_allowed("memory_recall"));

        // With a map entry, the granted tool still narrows at the caller
        // gate: the map can only veto, and the per-run list is the last gate.
        let map = grant_map(&[("grafana", &["search_dashboards"])]);
        let policy = ToolAccessPolicy::from_security(
            Some(&["shell".to_string()]),
            None,
            Some(&["shell".to_string(), "github__search".to_string()]),
            Some(&map),
        )
        .expect("policy");
        assert!(
            !policy.is_tool_allowed("grafana__search_dashboards"),
            "a map-granted tool outside the caller list is still narrowed"
        );
    }

    #[test]
    fn empty_caller_allowlist_denies_every_mcp_tool() {
        // The principal tool narrowing flows in as `caller_allowed`. An
        // empty principal list must yield a tool-less session even in the
        // presence of MCP `<server>__<tool>` names, which the risk-profile
        // gate would otherwise auto-admit. No `__` escape at the caller layer.
        let policy = ToolAccessPolicy::from_security(None, None, Some(&[]), None)
            .expect("an empty caller allowlist still produces a policy");
        assert!(!policy.is_tool_allowed("shell"));
        assert!(
            !policy.is_tool_allowed("filesystem__write_file"),
            "empty caller allowlist must deny MCP tools despite the __ auto-admit"
        );

        // The map grant does not escape an empty caller allowlist either —
        // even a wildcard entry stays narrowed by the per-run list.
        let map = grant_map(&[("grafana", &["*"])]);
        let policy =
            ToolAccessPolicy::from_security(None, None, Some(&[]), Some(&map)).expect("policy");
        assert!(
            !policy.is_tool_allowed("grafana__search_dashboards"),
            "empty caller allowlist denies even a wildcard-granted MCP tool"
        );
    }

    #[test]
    fn caller_allowed_per_run_gate_still_honors_denylist() {
        let policy = ToolAccessPolicy::from_security(
            Some(&["shell".to_string()]),
            Some(&["filesystem__write_file".to_string()]),
            Some(&["shell".to_string(), "filesystem__write_file".to_string()]),
            None,
        )
        .expect("policy with all three fields should be constructed");

        assert!(policy.is_tool_allowed("shell"));
        assert!(
            !policy.is_tool_allowed("filesystem__write_file"),
            "denylist subtracts even when both gates would admit"
        );

        // The denylist subtracts from a map grant too (denied > mcp_servers).
        let map = grant_map(&[("grafana", &["write_file"])]);
        let policy = ToolAccessPolicy::from_security(
            Some(&["shell".to_string()]),
            Some(&["grafana__write_file".to_string()]),
            Some(&["shell".to_string(), "grafana__write_file".to_string()]),
            Some(&map),
        )
        .expect("policy");
        assert!(
            !policy.is_tool_allowed("grafana__write_file"),
            "denylist subtracts even when the mcp_servers grant admits"
        );
    }

    // ── The per-profile `mcp_servers` grant map ─────────────────────────
    // (composition order: denied > mcp_servers > allowed > caller_allowed)
    //
    // `mcp_servers` is `Option<HashMap<String, McpProfileGrant>>`: `None`
    // keeps the legacy `<server>__<tool>` auto-admit; a map entry for a
    // server makes that server deny-by-default (only the listed bare tool
    // names, or the explicit `"*"` wildcard, pass). The map can only veto —
    // a granted tool still passes the allow-list and caller gates.

    fn grant_map(entries: &[(&str, &[&str])]) -> HashMap<String, McpProfileGrant> {
        entries
            .iter()
            .map(|(server, tools)| {
                (
                    (*server).to_string(),
                    McpProfileGrant {
                        tools: tools.iter().map(|t| (*t).to_string()).collect(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn mcp_servers_map_entry_is_deny_by_default_per_server() {
        // A map entry for `grafana` makes that server deny-by-default: only
        // the listed bare tool names pass. A server with NO entry keeps the
        // legacy auto-admit, and non-MCP names are never map-governed.
        let map = grant_map(&[("grafana", &["search_dashboards"])]);
        let policy =
            ToolAccessPolicy::from_security(Some(&["shell".to_string()]), None, None, Some(&map))
                .expect("a map entry produces a policy");
        assert!(policy.is_tool_allowed("grafana__search_dashboards"));
        assert!(
            !policy.is_tool_allowed("grafana__update_dashboard"),
            "a mapped server denies its unlisted tools"
        );
        assert!(
            policy.is_tool_allowed("github__search"),
            "a server with no map entry keeps the legacy auto-admit"
        );
        assert!(policy.is_tool_allowed("shell"));
        assert!(
            !policy.is_tool_allowed("file_write"),
            "non-MCP names ride the allow-list exactly as before"
        );
    }

    #[test]
    fn mcp_servers_wildcard_entry_admits_every_server_tool() {
        let map = grant_map(&[("grafana", &["*"])]);
        let policy =
            ToolAccessPolicy::from_security(Some(&["shell".to_string()]), None, None, Some(&map))
                .expect("a map entry produces a policy");
        assert!(policy.is_tool_allowed("grafana__search_dashboards"));
        assert!(policy.is_tool_allowed("grafana__update_dashboard"));
        assert!(policy.is_tool_allowed("grafana__anything_at_all"));

        // The wildcard is per-server: another mapped server without "*"
        // stays deny-by-default.
        let map = grant_map(&[("grafana", &["*"]), ("k8s", &["namespaces_list"])]);
        let policy =
            ToolAccessPolicy::from_security(Some(&["shell".to_string()]), None, None, Some(&map))
                .expect("policy");
        assert!(policy.is_tool_allowed("k8s__namespaces_list"));
        assert!(!policy.is_tool_allowed("k8s__pods_delete"));
    }

    #[test]
    fn mcp_servers_absence_keeps_the_legacy_auto_admit() {
        // No map at all (the `None` pass): the legacy `__` auto-admit under
        // a non-empty allow-list, exactly as before the map existed.
        let policy =
            ToolAccessPolicy::from_security(Some(&["shell".to_string()]), None, None, None)
                .expect("an allow-list produces a policy");
        assert!(policy.is_tool_allowed("shell"));
        assert!(policy.is_tool_allowed("github__search"));
        assert!(!policy.is_tool_allowed("file_write"));

        // An empty map governs nothing: it must not conjure a policy for an
        // otherwise-ungated profile (the `None`-policy = unrestricted
        // contract).
        let empty = grant_map(&[]);
        assert!(
            ToolAccessPolicy::from_security(None, None, None, Some(&empty)).is_none(),
            "an empty map is not a gate — from_security stays None"
        );
        // ...and alongside a real allow-list it behaves as the legacy path.
        let policy =
            ToolAccessPolicy::from_security(Some(&["shell".to_string()]), None, None, Some(&empty))
                .expect("the allow-list produces a policy");
        assert!(policy.is_tool_allowed("github__search"));
    }

    #[test]
    fn denylist_beats_the_mcp_servers_grant() {
        // Composition order: denied > mcp_servers. An exact `denied` entry
        // subtracts a tool the map grants.
        let map = grant_map(&[("grafana", &["search_dashboards"])]);
        let policy = ToolAccessPolicy::from_security(
            Some(&["shell".to_string()]),
            Some(&["grafana__search_dashboards".to_string()]),
            None,
            Some(&map),
        )
        .expect("policy");
        assert!(
            !policy.is_tool_allowed("grafana__search_dashboards"),
            "an exact denied entry beats the mcp_servers grant"
        );
    }

    #[test]
    fn caller_allowlist_narrows_a_map_granted_tool() {
        let map = grant_map(&[("grafana", &["search_dashboards"])]);
        let policy = ToolAccessPolicy::from_security(
            Some(&["shell".to_string()]),
            None,
            Some(&["shell".to_string()]),
            Some(&map),
        )
        .expect("policy");
        assert!(
            !policy.is_tool_allowed("grafana__search_dashboards"),
            "the map grant does not bypass the caller-supplied per-run list"
        );

        // ...and the caller list naming the full prefixed name admits it.
        let policy = ToolAccessPolicy::from_security(
            Some(&["shell".to_string()]),
            None,
            Some(&["grafana__search_dashboards".to_string()]),
            Some(&map),
        )
        .expect("policy");
        assert!(policy.is_tool_allowed("grafana__search_dashboards"));
    }

    #[test]
    fn deny_all_tools_beats_the_mcp_servers_grant() {
        // `deny_all_tools` surfaces as the explicit empty allow-list
        // (`Some(vec![])`): it denies everything, map grant included — the
        // map can only veto, never force-admit past the allow-list gate.
        let map = grant_map(&[("grafana", &["search_dashboards"])]);
        let policy = ToolAccessPolicy::from_security(Some(&[]), None, None, Some(&map))
            .expect("an explicit empty allow-list produces a policy");
        assert!(
            !policy.is_tool_allowed("grafana__search_dashboards"),
            "deny-all beats a map grant — no map escape from an explicit \
             empty allow-list"
        );
    }

    #[test]
    fn mcp_servers_map_split_is_at_the_first_double_underscore() {
        // `server__tool__sub` splits at the FIRST `__`: server = `server`,
        // tool = `tool__sub` (the fork's own `<server>__<tool>` convention,
        // the `split_once` shape mcp_deferred uses). A map entry must name
        // the full bare tool part to grant it.
        let map = grant_map(&[("server", &["tool__sub"])]);
        let policy =
            ToolAccessPolicy::from_security(Some(&["shell".to_string()]), None, None, Some(&map))
                .expect("policy");
        assert!(policy.is_tool_allowed("server__tool__sub"));
        assert!(
            !policy.is_tool_allowed("server__tool"),
            "the entry lists `tool__sub`, not `tool` — the split is at the \
             first `__`"
        );
    }

    #[test]
    fn mcp_servers_map_never_governs_non_mcp_names() {
        // A name without `__` rides `allowed`/`caller_allowed` exactly as
        // before — the map is consulted only for `<server>__<tool>` names,
        // even when a map entry's key would collide with the name's prefix.
        let map = grant_map(&[("shell", &["*"])]);
        let policy =
            ToolAccessPolicy::from_security(Some(&["shell".to_string()]), None, None, Some(&map))
                .expect("policy");
        assert!(policy.is_tool_allowed("shell"));
        assert!(
            !policy.is_tool_allowed("file_write"),
            "a non-MCP name outside the allow-list is denied — the map's \
             wildcard entry is irrelevant to it"
        );
    }

    #[test]
    fn mcp_grant_denial_reason_names_the_server_and_the_exact_message() {
        let map = grant_map(&[("grafana", &["search_dashboards"])]);
        let policy =
            ToolAccessPolicy::from_security(Some(&["shell".to_string()]), None, None, Some(&map))
                .expect("policy");
        assert_eq!(
            policy.mcp_grant_denial_reason("grafana__update_dashboard"),
            Some(
                "`grafana__update_dashboard` is not on this profile's \
                 mcp_servers grant for `grafana`."
                    .to_string()
            )
        );
        // A granted tool carries no denial reason.
        assert_eq!(
            policy.mcp_grant_denial_reason("grafana__search_dashboards"),
            None
        );
        // No map entry for the server → no map denial reason (the legacy
        // path's refusals carry their own wording elsewhere).
        assert_eq!(policy.mcp_grant_denial_reason("github__search"), None);
        // Non-MCP name → no map denial reason.
        assert_eq!(policy.mcp_grant_denial_reason("file_write"), None);
        // No map at all → no map denial reason.
        let no_map =
            ToolAccessPolicy::from_security(Some(&["shell".to_string()]), None, None, None)
                .expect("policy");
        assert_eq!(
            no_map.mcp_grant_denial_reason("grafana__update_dashboard"),
            None
        );
    }
}
