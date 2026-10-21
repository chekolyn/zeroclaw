use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

use crate::channel::ChannelMessage;
use crate::model_provider::{ChatMessage, ChatResponse};
use crate::tool::ToolResult;

/// Typed correlation context for one prompt-build hook phase.
///
/// Carries the fields a prompt-modifying hook needs to attribute the prompt to
/// a specific agent and turn: the effective agent alias, the agent's workspace,
/// the session identifier (where one exists at the build site), and the turn
/// id. `turn_id` is deliberately present because the tool-call hook's
/// [`ToolCallHookContext`] exposes only an opaque `invocation_id` shaped
/// `turn_id:iteration:call_index`; correlating a tool event back to the turn's
/// prompt build requires the same `turn_id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PromptBuildHookContext {
    alias: String,
    workspace: PathBuf,
    session_id: String,
    turn_id: String,
}

impl PromptBuildHookContext {
    /// Create a prompt-build context.
    ///
    /// `session_id` may be empty when the build site has no session
    /// identifier; callers should use the best harness-session identifier
    /// available at that site and document the choice.
    pub fn new(
        alias: impl Into<String>,
        workspace: impl Into<PathBuf>,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> Self {
        Self {
            alias: alias.into(),
            workspace: workspace.into(),
            session_id: session_id.into(),
            turn_id: turn_id.into(),
        }
    }

    /// The effective agent alias this prompt is being built for.
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// The agent's workspace directory.
    pub fn workspace(&self) -> &PathBuf {
        &self.workspace
    }

    /// The session identifier, possibly empty where the build site has none.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The turn identifier.
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }
}

/// Opaque runtime context for one tool-call hook phase.
///
/// Correlated contexts carry one stable identity across the before and after
/// phases. Uncorrelated contexts are phase-local and must not be used to pair
/// callbacks or retain attribution-sensitive state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCallHookContext {
    invocation_id: String,
    correlated: bool,
}

impl ToolCallHookContext {
    /// Create a correlated context.
    ///
    /// The caller must provide a process-unique identity and reuse it for both
    /// phases of the same tool invocation.
    pub fn new(invocation_id: impl Into<String>) -> Self {
        Self {
            invocation_id: invocation_id.into(),
            correlated: true,
        }
    }

    /// Create a phase-local context when cross-phase identity is unavailable.
    pub fn uncorrelated(invocation_id: impl Into<String>) -> Self {
        Self {
            invocation_id: invocation_id.into(),
            correlated: false,
        }
    }

    /// Return the opaque phase or invocation identity.
    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }

    /// Whether this identity is stable across the before and after phases.
    pub fn is_correlated(&self) -> bool {
        self.correlated
    }
}

/// Result of a modifying hook — continue with (possibly modified) data, or cancel.
#[derive(Debug, Clone)]
pub enum HookResult<T> {
    Continue(T),
    Cancel(String),
}

impl<T> HookResult<T> {
    pub fn is_cancel(&self) -> bool {
        matches!(self, HookResult::Cancel(_))
    }
}

/// Trait for hook handlers. All methods have default no-op implementations.
/// Implement only the events you care about.
#[async_trait]
pub trait HookHandler: Send + Sync {
    fn name(&self) -> &str;
    fn priority(&self) -> i32 {
        0
    }

    // --- Void hooks (parallel, fire-and-forget) ---
    async fn on_gateway_start(&self, _host: &str, _port: u16) {}
    async fn on_gateway_stop(&self) {}
    async fn on_session_start(&self, _session_id: &str, _channel: &str) {}
    async fn on_session_end(&self, _session_id: &str, _channel: &str) {}
    async fn on_llm_input(&self, _messages: &[ChatMessage], _model: &str) {}
    async fn on_llm_output(&self, _response: &ChatResponse) {}
    async fn on_after_tool_call(&self, _tool: &str, _result: &ToolResult, _duration: Duration) {}
    /// Observe tool completion with explicit correlation context.
    ///
    /// When `context.is_correlated()` is false, handlers must not use its
    /// phase-local identity to pair callbacks or retain attributed state.
    async fn on_after_tool_call_with_context(
        &self,
        _context: &ToolCallHookContext,
        tool: &str,
        result: &ToolResult,
        duration: Duration,
    ) {
        self.on_after_tool_call(tool, result, duration).await;
    }
    /// Observe tool completion with explicit correlation context and the
    /// arguments that were actually dispatched.
    ///
    /// Runners that know the final prepared arguments dispatch this variant
    /// instead of [`HookHandler::on_after_tool_call_with_context`]; the
    /// default delegates there, dropping the arguments. Handlers that audit
    /// arguments should override this variant so their export reflects what
    /// was dispatched — and so they never need to retain arguments between
    /// the before and after phases.
    async fn on_after_tool_call_with_context_and_args(
        &self,
        context: &ToolCallHookContext,
        tool: &str,
        _args: &Value,
        result: &ToolResult,
        duration: Duration,
    ) {
        self.on_after_tool_call_with_context(context, tool, result, duration)
            .await;
    }
    /// Observe that a tool-call context will never reach its after phase.
    ///
    /// Fired exactly once per correlated context whose before phase ran, when
    /// the call cannot complete: the before hook chain cancelled it, approval
    /// denied or replaced it, duplicate suppression skipped it, or execution
    /// failed or was interrupted before post-execution handling. Abandonment
    /// is NOT execution completion — handlers must never synthesize a tool
    /// result from it.
    ///
    /// Best-effort by nature: the runner dispatches abandonment on every
    /// awaited non-completion path, but a turn future dropped outright (an
    /// outer timeout, outer cancellation, or hard task abort) cannot run any
    /// async callback, abandonment included. Handlers must therefore avoid
    /// retaining safety-critical state between the before and after phases in
    /// the first place, and treat abandonment as a best-effort release signal
    /// rather than a guarantee.
    async fn on_tool_call_abandoned(&self, _context: &ToolCallHookContext, _tool: &str) {}
    async fn on_message_sent(&self, _channel: &str, _recipient: &str, _content: &str) {}
    async fn on_heartbeat_tick(&self) {}

    // --- Modifying hooks (sequential by priority, can cancel) ---
    async fn before_model_resolve(
        &self,
        model_provider: String,
        model: String,
    ) -> HookResult<(String, String)> {
        HookResult::Continue((model_provider, model))
    }

    async fn before_prompt_build(&self, prompt: String) -> HookResult<String> {
        HookResult::Continue(prompt)
    }

    /// Inspect or modify a prompt build with explicit correlation context.
    ///
    /// Runners that can attribute the prompt to an agent and turn dispatch
    /// this variant instead of [`HookHandler::before_prompt_build`]; the
    /// default delegates there, dropping the context. Handlers that key on the
    /// agent alias, workspace, or correlate prompt builds with tool events via
    /// `turn_id` should override this variant.
    async fn before_prompt_build_with_context(
        &self,
        _context: &PromptBuildHookContext,
        prompt: String,
    ) -> HookResult<String> {
        self.before_prompt_build(prompt).await
    }

    async fn before_llm_call(
        &self,
        _messages: &mut Vec<ChatMessage>,
        _model: &mut String,
    ) -> HookResult<()> {
        HookResult::Continue(())
    }

    async fn before_tool_call(&self, name: String, args: Value) -> HookResult<(String, Value)> {
        HookResult::Continue((name, args))
    }

    /// Inspect or modify a tool call with explicit correlation context.
    ///
    /// When `context.is_correlated()` is false, handlers must not retain state
    /// that assumes the after phase will receive the same identity.
    async fn before_tool_call_with_context(
        &self,
        _context: &ToolCallHookContext,
        name: String,
        args: Value,
    ) -> HookResult<(String, Value)> {
        self.before_tool_call(name, args).await
    }

    async fn on_message_received(&self, message: ChannelMessage) -> HookResult<ChannelMessage> {
        HookResult::Continue(message)
    }

    async fn on_message_sending(
        &self,
        channel: String,
        recipient: String,
        content: String,
    ) -> HookResult<(String, String, String)> {
        HookResult::Continue((channel, recipient, content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHook {
        name: String,
        priority: i32,
    }

    impl TestHook {
        fn new(name: &str, priority: i32) -> Self {
            Self {
                name: name.to_string(),
                priority,
            }
        }
    }

    #[async_trait]
    impl HookHandler for TestHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn priority(&self) -> i32 {
            self.priority
        }
    }

    #[test]
    fn hook_result_is_cancel() {
        let ok: HookResult<String> = HookResult::Continue("hi".into());
        assert!(!ok.is_cancel());
        let cancel: HookResult<String> = HookResult::Cancel("blocked".into());
        assert!(cancel.is_cancel());
    }

    #[test]
    fn default_priority_is_zero() {
        struct MinimalHook;
        #[async_trait]
        impl HookHandler for MinimalHook {
            fn name(&self) -> &str {
                "minimal"
            }
        }
        assert_eq!(MinimalHook.priority(), 0);
    }

    #[tokio::test]
    async fn default_modifying_hooks_pass_through() {
        let hook = TestHook::new("test", 0);
        match hook
            .before_tool_call("shell".into(), serde_json::json!({"cmd": "ls"}))
            .await
        {
            HookResult::Continue((name, _args)) => assert_eq!(name, "shell"),
            HookResult::Cancel(_) => panic!("should not cancel"),
        }
    }

    #[tokio::test]
    async fn context_aware_defaults_delegate_to_legacy_tool_hooks() {
        use std::sync::{Arc, Mutex};

        struct LegacyHook {
            after: Arc<Mutex<Option<(String, bool, Duration)>>>,
        }

        #[async_trait]
        impl HookHandler for LegacyHook {
            fn name(&self) -> &str {
                "legacy"
            }

            async fn before_tool_call(
                &self,
                name: String,
                args: Value,
            ) -> HookResult<(String, Value)> {
                HookResult::Continue((format!("{name}_legacy"), args))
            }

            async fn on_after_tool_call(
                &self,
                tool: &str,
                result: &ToolResult,
                duration: Duration,
            ) {
                *self.after.lock().unwrap() = Some((tool.to_string(), result.success, duration));
            }
        }

        let after = Arc::new(Mutex::new(None));
        let hook = LegacyHook {
            after: Arc::clone(&after),
        };
        let context = ToolCallHookContext::new("opaque-id");
        let args = serde_json::json!({"cmd": "ls"});

        let before = hook
            .before_tool_call_with_context(&context, "shell".into(), args.clone())
            .await;
        match before {
            HookResult::Continue((name, actual_args)) => {
                assert_eq!(name, "shell_legacy");
                assert_eq!(actual_args, args);
            }
            HookResult::Cancel(_) => panic!("legacy hook should continue"),
        }

        let result = ToolResult {
            success: true,
            output: "ok".into(),
            error: None,
        };
        let duration = Duration::from_millis(12);
        hook.on_after_tool_call_with_context(&context, "shell", &result, duration)
            .await;

        assert_eq!(
            *after.lock().unwrap(),
            Some(("shell".to_string(), true, duration))
        );
    }

    #[test]
    fn tool_call_context_marks_unavailable_correlation() {
        let exact = ToolCallHookContext::new("exact");
        let unavailable = ToolCallHookContext::uncorrelated("legacy");

        assert!(exact.is_correlated());
        assert!(!unavailable.is_correlated());
        assert_eq!(unavailable.invocation_id(), "legacy");
    }

    #[tokio::test]
    async fn args_carrying_after_variant_defaults_to_context_variant() {
        use std::sync::{Arc, Mutex};

        struct ContextOnlyHook {
            seen: Arc<Mutex<Option<(String, String, bool)>>>,
        }

        #[async_trait]
        impl HookHandler for ContextOnlyHook {
            fn name(&self) -> &str {
                "context-only"
            }

            async fn on_after_tool_call_with_context(
                &self,
                context: &ToolCallHookContext,
                tool: &str,
                result: &ToolResult,
                _duration: Duration,
            ) {
                *self.seen.lock().unwrap() = Some((
                    context.invocation_id().to_string(),
                    tool.to_string(),
                    result.success,
                ));
            }
        }

        let seen = Arc::new(Mutex::new(None));
        let hook = ContextOnlyHook {
            seen: Arc::clone(&seen),
        };
        let result = ToolResult {
            success: true,
            output: "ok".into(),
            error: None,
        };
        hook.on_after_tool_call_with_context_and_args(
            &ToolCallHookContext::new("invocation"),
            "shell",
            &serde_json::json!({"command": "ls"}),
            &result,
            Duration::ZERO,
        )
        .await;
        assert_eq!(
            *seen.lock().unwrap(),
            Some(("invocation".to_string(), "shell".to_string(), true)),
            "the args-carrying default must land in the context-aware variant"
        );
    }

    #[tokio::test]
    async fn default_abandonment_hook_compiles_and_is_a_noop() {
        // A pre-existing handler shape: implements only `name`. It must keep
        // compiling unchanged and the default abandonment must be a no-op.
        struct MinimalHook;
        #[async_trait]
        impl HookHandler for MinimalHook {
            fn name(&self) -> &str {
                "minimal"
            }
        }

        let hook = MinimalHook;
        hook.on_tool_call_abandoned(&ToolCallHookContext::new("invocation"), "shell")
            .await;
        hook.on_tool_call_abandoned(&ToolCallHookContext::uncorrelated("legacy"), "shell")
            .await;
    }

    #[test]
    fn prompt_build_context_carries_alias_workspace_session_turn() {
        let ctx = PromptBuildHookContext::new(
            "director",
            std::path::PathBuf::from("/storage/agents/director/workspace"),
            "sess-1",
            "turn-9",
        );
        assert_eq!(ctx.alias(), "director");
        assert_eq!(
            ctx.workspace(),
            &std::path::PathBuf::from("/storage/agents/director/workspace")
        );
        assert_eq!(ctx.session_id(), "sess-1");
        assert_eq!(ctx.turn_id(), "turn-9");
    }

    #[test]
    fn prompt_build_context_allows_empty_session() {
        let ctx = PromptBuildHookContext::new(
            "worker",
            std::path::PathBuf::from("/storage/agents/worker/workspace"),
            "",
            "turn-2",
        );
        assert!(ctx.session_id().is_empty());
    }

    #[tokio::test]
    async fn context_aware_prompt_build_defaults_to_legacy_hook() {
        use std::sync::{Arc, Mutex};

        struct LegacyPromptHook {
            seen: Arc<Mutex<Option<String>>>,
        }

        #[async_trait]
        impl HookHandler for LegacyPromptHook {
            fn name(&self) -> &str {
                "legacy-prompt"
            }

            async fn before_prompt_build(&self, prompt: String) -> HookResult<String> {
                *self.seen.lock().unwrap() = Some(prompt.clone());
                HookResult::Continue(format!("{prompt}_v2"))
            }
        }

        let seen = Arc::new(Mutex::new(None));
        let hook = LegacyPromptHook {
            seen: Arc::clone(&seen),
        };
        let ctx = PromptBuildHookContext::new(
            "director",
            std::path::PathBuf::from("/storage/agents/director/workspace"),
            "sess-1",
            "turn-9",
        );

        match hook
            .before_prompt_build_with_context(&ctx, "hello".into())
            .await
        {
            HookResult::Continue(p) => assert_eq!(p, "hello_v2"),
            HookResult::Cancel(_) => panic!("legacy prompt hook must continue"),
        }
        assert_eq!(*seen.lock().unwrap(), Some("hello".to_string()));
    }
}
