use crate::{
    AppError, AppResult,
    config::{Config, ProviderKind},
    ledger::{EventRecorder, SessionTurn, SqliteLedger},
    model::{ModelBlock, ModelMessage, ModelRequest, ModelResponse, ModelStop, system_prompt},
    provider::openai_compat::{OpenAiCompatibleClient, TokenLimitField},
    tool_catalog::{ToolSpec, effect_for_tool, tool_specs},
    tools::{ApprovalOutcome, approval_diff_preview, ask_for_approval, execute_tool},
};
use platonic_core::{
    ActorId, AgentId, ContextFragment, ContextLane, ContextPack, EffectClass, Error as CoreError,
    HarnessEvent, Message, MessageRole, ModelName, PolicyDecision, RecordedEvent, RunId, ToolCall,
    ToolCallId, ToolName, ToolProposal, TurnId,
};
use serde_json::Value;
use std::{
    fmt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
};

#[derive(Clone, Debug)]
pub struct RunOptions {
    pub question: String,
    pub config_path: Option<PathBuf>,
    pub ledger: RunLedger,
    pub workspace_root: PathBuf,
    pub approval_mode: ApprovalMode,
    pub run_id: Option<RunId>,
    pub session: Option<RunSession>,
    pub event_sender: Option<Sender<RecordedEvent>>,
    pub cancel: Option<Arc<AtomicBool>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunSession {
    Fresh { session_id: String },
    Continue { session_id: String },
}

impl RunSession {
    pub fn session_id(&self) -> &str {
        match self {
            Self::Fresh { session_id } | Self::Continue { session_id } => session_id,
        }
    }

    fn create_session(&self) -> bool {
        matches!(self, Self::Fresh { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunOutcome {
    pub run_id: RunId,
    pub final_answer: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunLedger {
    Jsonl(PathBuf),
    Sqlite(PathBuf),
}

#[derive(Clone, Default)]
pub enum ApprovalMode {
    #[default]
    Prompt,
    AutoApprove,
    Deny {
        actor: &'static str,
    },
    External(ApprovalHandler),
}

#[derive(Clone)]
pub struct ApprovalHandler {
    actor: &'static str,
    decide: Arc<dyn Fn(ApprovalRequest) -> AppResult<ApprovalOutcome> + Send + Sync>,
}

impl fmt::Debug for ApprovalMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prompt => formatter.write_str("Prompt"),
            Self::AutoApprove => formatter.write_str("AutoApprove"),
            Self::Deny { actor } => formatter
                .debug_struct("Deny")
                .field("actor", actor)
                .finish(),
            Self::External(handler) => formatter
                .debug_struct("External")
                .field("actor", &handler.actor)
                .finish_non_exhaustive(),
        }
    }
}

impl fmt::Debug for ApprovalHandler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApprovalHandler")
            .field("actor", &self.actor)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalRequest {
    pub run_id: RunId,
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub effect: EffectClass,
    pub reason: String,
    pub diff_preview: Option<String>,
}

impl ApprovalMode {
    pub fn from_yolo(enabled: bool) -> Self {
        if enabled {
            Self::AutoApprove
        } else {
            Self::Prompt
        }
    }

    fn auto_grant_actor(&self, policy: &PolicyDecision) -> Option<&'static str> {
        match (self, policy) {
            (Self::AutoApprove, PolicyDecision::RequireApproval { .. }) => Some("yolo"),
            _ => None,
        }
    }

    fn deny_actor(&self, policy: &PolicyDecision) -> Option<&'static str> {
        match (self, policy) {
            (Self::Deny { actor }, PolicyDecision::RequireApproval { .. }) => Some(actor),
            _ => None,
        }
    }

    pub fn external(
        actor: &'static str,
        decide: impl Fn(ApprovalRequest) -> AppResult<ApprovalOutcome> + Send + Sync + 'static,
    ) -> Self {
        Self::External(ApprovalHandler {
            actor,
            decide: Arc::new(decide),
        })
    }
}

const SESSION_TRUNCATION_MARKER: &str = "[older session turns omitted to fit the context budget]";

struct ActiveSessionRun {
    ledger: SqliteLedger,
    run_id: RunId,
    closed: bool,
}

impl ActiveSessionRun {
    fn begin(
        ledger_path: &std::path::Path,
        session: &RunSession,
        run_id: &RunId,
        question: &str,
        config: &Config,
        tools: &[ToolSpec],
    ) -> AppResult<(Self, Vec<ModelMessage>)> {
        let mut ledger = SqliteLedger::open_or_create(ledger_path)?;
        let turns = ledger.begin_session_run(
            session.session_id(),
            run_id,
            question,
            session.create_session(),
        )?;
        let messages = hydrated_messages(&turns, question, config, tools)?;
        Ok((
            Self {
                ledger,
                run_id: run_id.clone(),
                closed: false,
            },
            messages,
        ))
    }

    fn finish(&mut self, final_answer: &str) -> AppResult<()> {
        self.ledger.finish_session_run(&self.run_id, final_answer)?;
        self.closed = true;
        Ok(())
    }

    fn fail(&mut self, error: &str, canceled: bool) -> AppResult<()> {
        self.ledger
            .fail_session_run(&self.run_id, error, canceled)?;
        self.closed = true;
        Ok(())
    }
}

impl Drop for ActiveSessionRun {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.ledger.fail_session_run(
                &self.run_id,
                "run ended before session status was closed",
                false,
            );
        }
    }
}

fn hydrated_messages(
    turns: &[SessionTurn],
    question: &str,
    config: &Config,
    tools: &[ToolSpec],
) -> AppResult<Vec<ModelMessage>> {
    let mut first_turn = 0;
    let mut truncated = false;
    loop {
        let messages = session_messages_from(&turns[first_turn..], question, truncated);
        if estimated_context_tokens(&messages, tools)? <= config.limits.token_budget
            || first_turn == turns.len()
        {
            return Ok(messages);
        }
        first_turn += 1;
        truncated = true;
    }
}

fn session_messages_from(
    turns: &[SessionTurn],
    question: &str,
    truncated: bool,
) -> Vec<ModelMessage> {
    let mut messages = Vec::new();
    if truncated {
        messages.push(ModelMessage::user_text(SESSION_TRUNCATION_MARKER));
    }
    for turn in turns {
        messages.push(ModelMessage::user_text(turn.question.clone()));
        messages.push(ModelMessage::assistant_blocks(vec![ModelBlock::Text {
            text: turn.final_answer.clone(),
        }]));
    }
    messages.push(ModelMessage::user_text(question.to_string()));
    messages
}

fn estimated_context_tokens(messages: &[ModelMessage], tools: &[ToolSpec]) -> AppResult<u32> {
    let messages = serde_json::to_string(messages)?;
    let tools = serde_json::to_string(tools)?;
    Ok(estimate_tokens(system_prompt())
        .saturating_add(estimate_tokens(&messages))
        .saturating_add(estimate_tokens(&tools)))
}

pub fn run_question(options: RunOptions) -> AppResult<RunOutcome> {
    if options.question.trim().is_empty() {
        return Err(AppError::EmptyQuestion);
    }

    let config = Config::load(&options.workspace_root, options.config_path.as_deref())?;
    let run_id = options.run_id.clone().unwrap_or(new_run_id()?);
    let client = OpenAiCompatibleClient::from_config(
        &config.provider.api_key_env,
        config.provider.base_url.clone(),
        config.provider.timeout_ms,
        config.provider.http_referer.clone(),
        config.provider.app_title.clone(),
        token_limit_field(&config.provider.kind),
    )?;
    let tools = tool_specs(&config.tools.enabled);
    let (mut session_run, mut messages) = match (&options.ledger, &options.session) {
        (RunLedger::Sqlite(path), Some(session)) => {
            let (session_run, messages) = ActiveSessionRun::begin(
                path,
                session,
                &run_id,
                &options.question,
                &config,
                &tools,
            )?;
            (Some(session_run), messages)
        }
        (RunLedger::Jsonl(_), Some(_)) => {
            return Err(AppError::Config("sessions require a SQLite ledger".into()));
        }
        (_, None) => (
            None,
            vec![ModelMessage::user_text(options.question.clone())],
        ),
    };
    let mut recorder = match &options.ledger {
        RunLedger::Jsonl(path) => EventRecorder::create_jsonl(path)?,
        RunLedger::Sqlite(path) => EventRecorder::create_sqlite(path, &run_id)?,
    };
    let agent_id = AgentId::new("plato")?;
    let model = ModelName::new(config.provider.model.clone())?;
    let stdin_actor_id = ActorId::new("stdin")?;

    record_event(
        &mut recorder,
        &options,
        HarnessEvent::RunStarted {
            run_id: run_id.clone(),
            agent_id,
        },
    )?;

    for turn_index in 0..config.limits.max_turns {
        let turn_id = TurnId::new(format!("turn_{}", turn_index + 1))?;
        let request = ModelRequest {
            model: config.provider.model.clone(),
            system: system_prompt().into(),
            max_output_tokens: config.limits.max_output_tokens,
            messages: messages.clone(),
            tools: tools.clone(),
        };
        let context = context_pack(&request, config.limits.token_budget)?;
        check_cancel(&mut recorder, &options, &run_id)?;
        record_context_built(&mut recorder, &options, &run_id, turn_id.clone(), context)?;
        record_event(
            &mut recorder,
            &options,
            HarnessEvent::ModelRequested {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                step: turn_index,
                model: model.clone(),
            },
        )?;

        let response = match client.send(&request) {
            Ok(response) => response,
            Err(error) => {
                let reason = error.to_string();
                record_event(
                    &mut recorder,
                    &options,
                    HarnessEvent::RunFailed {
                        run_id,
                        reason: reason.clone(),
                    },
                )?;
                if let Some(session_run) = &mut session_run {
                    session_run.fail(&reason, false)?;
                }
                return Err(error);
            }
        };

        let proposals = proposals_from_response(&response)?;
        record_event(
            &mut recorder,
            &options,
            HarnessEvent::ModelResponded {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                step: turn_index,
                output: Message {
                    role: MessageRole::Assistant,
                    content: response.text(),
                },
                proposed_calls: proposals.clone(),
                usage: response.usage.clone(),
            },
        )?;

        match response.stop {
            ModelStop::MaxOutput => {
                let reason = "model reached max output tokens".to_string();
                record_event(
                    &mut recorder,
                    &options,
                    HarnessEvent::RunFailed { run_id, reason },
                )?;
                if let Some(session_run) = &mut session_run {
                    session_run.fail("model reached max output tokens", false)?;
                }
                return Err(AppError::RunFailed(
                    "model reached max output tokens".into(),
                ));
            }
            ModelStop::ContentFilter => {
                let reason = "model response was stopped by content filter".to_string();
                record_event(
                    &mut recorder,
                    &options,
                    HarnessEvent::RunFailed { run_id, reason },
                )?;
                if let Some(session_run) = &mut session_run {
                    session_run.fail("model response was stopped by content filter", false)?;
                }
                return Err(AppError::RunFailed(
                    "model response was stopped by content filter".into(),
                ));
            }
            ModelStop::EndTurn | ModelStop::ToolUse => {}
        }

        check_cancel(&mut recorder, &options, &run_id)?;
        let tool_uses = response.tool_uses();
        if response.stop == ModelStop::ToolUse && tool_uses.is_empty() {
            let reason = "provider reported tool use without tool calls".to_string();
            record_event(
                &mut recorder,
                &options,
                HarnessEvent::RunFailed { run_id, reason },
            )?;
            if let Some(session_run) = &mut session_run {
                session_run.fail("provider reported tool use without tool calls", false)?;
            }
            return Err(AppError::RunFailed(
                "provider reported tool use without tool calls".into(),
            ));
        }
        if tool_uses.is_empty() {
            let final_answer = response.text();
            record_event(
                &mut recorder,
                &options,
                HarnessEvent::RunFinished {
                    run_id: run_id.clone(),
                },
            )?;
            if let Some(session_run) = &mut session_run {
                session_run.finish(&final_answer)?;
            }
            return Ok(RunOutcome {
                run_id,
                final_answer,
            });
        }

        if tool_uses.len() > 1 {
            let reason = "model requested multiple tools in one response".to_string();
            record_event(
                &mut recorder,
                &options,
                HarnessEvent::RunFailed { run_id, reason },
            )?;
            if let Some(session_run) = &mut session_run {
                session_run.fail("model requested multiple tools in one response", false)?;
            }
            return Err(AppError::RunFailed(
                "model requested multiple tools in one response".into(),
            ));
        }

        if !response.text().trim().is_empty() {
            eprintln!("{}", response.text());
        }

        messages.push(ModelMessage::assistant_blocks(response.content.clone()));
        let (tool_use_id, tool_name, input) = tool_uses.into_iter().next().expect("checked len");
        let call_id = ToolCallId::new(tool_use_id.clone())?;
        let call = tool_call(call_id.clone(), &tool_name, input.clone())?;
        record_event(
            &mut recorder,
            &options,
            HarnessEvent::ToolCallProposed {
                run_id: run_id.clone(),
                turn_id,
                call: call.clone(),
            },
        )?;

        let policy = evaluate_policy(&config.tools.enabled, &call);
        record_event(
            &mut recorder,
            &options,
            HarnessEvent::PolicyEvaluated {
                run_id: run_id.clone(),
                call_id: call_id.clone(),
                decision: policy.clone(),
            },
        )?;

        let tool_message = match policy {
            PolicyDecision::Allow => execute_and_record_tool(
                &mut recorder,
                &options,
                &run_id,
                &options.workspace_root,
                call_id.clone(),
                &tool_name,
                input,
            )?,
            PolicyDecision::RequireApproval { ref reason } => {
                if let Some(actor) = options.approval_mode.auto_grant_actor(&policy) {
                    let actor_id = ActorId::new(actor)?;
                    record_event(
                        &mut recorder,
                        &options,
                        HarnessEvent::ApprovalGranted {
                            run_id: run_id.clone(),
                            call_id: call_id.clone(),
                            actor_id,
                        },
                    )?;
                    execute_and_record_tool(
                        &mut recorder,
                        &options,
                        &run_id,
                        &options.workspace_root,
                        call_id.clone(),
                        &tool_name,
                        input,
                    )?
                } else if let Some(actor) = options.approval_mode.deny_actor(&policy) {
                    let reason =
                        format!("approval required but no approval channel is available: {reason}");
                    record_event(
                        &mut recorder,
                        &options,
                        HarnessEvent::ApprovalDenied {
                            run_id: run_id.clone(),
                            call_id,
                            actor_id: ActorId::new(actor)?,
                            reason: reason.clone(),
                        },
                    )?;
                    ToolMessage {
                        content: reason,
                        is_error: true,
                    }
                } else if let ApprovalMode::External(handler) = options.approval_mode.clone() {
                    let request = ApprovalRequest {
                        run_id: run_id.clone(),
                        call_id: call_id.clone(),
                        tool_name: call.tool.to_string(),
                        effect: call.effect.clone(),
                        reason: reason.clone(),
                        diff_preview: approval_diff_preview(
                            &options.workspace_root,
                            call.tool.as_str(),
                            &call.input,
                        ),
                    };
                    match (handler.decide)(request)? {
                        ApprovalOutcome::Granted => {
                            record_event(
                                &mut recorder,
                                &options,
                                HarnessEvent::ApprovalGranted {
                                    run_id: run_id.clone(),
                                    call_id: call_id.clone(),
                                    actor_id: ActorId::new(handler.actor)?,
                                },
                            )?;
                            execute_and_record_tool(
                                &mut recorder,
                                &options,
                                &run_id,
                                &options.workspace_root,
                                call_id.clone(),
                                &tool_name,
                                input,
                            )?
                        }
                        ApprovalOutcome::Denied { reason } => {
                            record_event(
                                &mut recorder,
                                &options,
                                HarnessEvent::ApprovalDenied {
                                    run_id: run_id.clone(),
                                    call_id,
                                    actor_id: ActorId::new(handler.actor)?,
                                    reason: reason.clone(),
                                },
                            )?;
                            ToolMessage {
                                content: reason,
                                is_error: true,
                            }
                        }
                    }
                } else {
                    match ask_for_approval(&tool_name, &call.input)? {
                        ApprovalOutcome::Granted => {
                            record_event(
                                &mut recorder,
                                &options,
                                HarnessEvent::ApprovalGranted {
                                    run_id: run_id.clone(),
                                    call_id: call_id.clone(),
                                    actor_id: stdin_actor_id.clone(),
                                },
                            )?;
                            execute_and_record_tool(
                                &mut recorder,
                                &options,
                                &run_id,
                                &options.workspace_root,
                                call_id.clone(),
                                &tool_name,
                                input,
                            )?
                        }
                        ApprovalOutcome::Denied { reason } => {
                            record_event(
                                &mut recorder,
                                &options,
                                HarnessEvent::ApprovalDenied {
                                    run_id: run_id.clone(),
                                    call_id,
                                    actor_id: stdin_actor_id.clone(),
                                    reason: reason.clone(),
                                },
                            )?;
                            ToolMessage {
                                content: reason,
                                is_error: true,
                            }
                        }
                    }
                }
            }
            PolicyDecision::Deny { reason } => ToolMessage {
                content: reason,
                is_error: true,
            },
        };

        messages.push(ModelMessage::tool_result(
            tool_use_id,
            tool_message.content,
            tool_message.is_error,
        ));
    }

    let reason = format!("exceeded maximum turn count of {}", config.limits.max_turns);
    record_event(
        &mut recorder,
        &options,
        HarnessEvent::RunFailed {
            run_id,
            reason: reason.clone(),
        },
    )?;
    if let Some(session_run) = &mut session_run {
        session_run.fail(&reason, false)?;
    }
    Err(AppError::RunFailed(reason))
}

#[derive(Debug)]
struct ToolMessage {
    content: String,
    is_error: bool,
}

fn record_event(
    recorder: &mut EventRecorder,
    options: &RunOptions,
    event: HarnessEvent,
) -> AppResult<RecordedEvent> {
    let record = recorder.record(event)?;
    if let Some(sender) = &options.event_sender {
        let _ = sender.send(record.clone());
    }
    Ok(record)
}

fn record_context_built(
    recorder: &mut EventRecorder,
    options: &RunOptions,
    run_id: &RunId,
    turn_id: TurnId,
    context: ContextPack,
) -> AppResult<()> {
    match record_event(
        recorder,
        options,
        HarnessEvent::ContextBuilt {
            run_id: run_id.clone(),
            turn_id,
            context,
        },
    ) {
        Ok(_) => Ok(()),
        Err(AppError::Core(CoreError::ContextBudgetExceeded { used, budget })) => {
            let error = CoreError::ContextBudgetExceeded { used, budget };
            record_event(
                recorder,
                options,
                HarnessEvent::RunFailed {
                    run_id: run_id.clone(),
                    reason: error.to_string(),
                },
            )?;
            Err(AppError::Core(error))
        }
        Err(error) => Err(error),
    }
}

fn check_cancel(
    recorder: &mut EventRecorder,
    options: &RunOptions,
    run_id: &RunId,
) -> AppResult<()> {
    if options
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.load(Ordering::SeqCst))
    {
        let reason = "run canceled".to_string();
        record_event(
            recorder,
            options,
            HarnessEvent::RunFailed {
                run_id: run_id.clone(),
                reason: reason.clone(),
            },
        )?;
        return Err(AppError::RunFailed(reason));
    }
    Ok(())
}

fn execute_and_record_tool(
    recorder: &mut EventRecorder,
    options: &RunOptions,
    run_id: &RunId,
    workspace_root: &std::path::Path,
    call_id: ToolCallId,
    tool_name: &str,
    input: Value,
) -> AppResult<ToolMessage> {
    check_cancel(recorder, options, run_id)?;
    record_event(
        recorder,
        options,
        HarnessEvent::ToolStarted {
            run_id: run_id.clone(),
            call_id: call_id.clone(),
        },
    )?;

    match execute_tool(workspace_root, call_id.clone(), tool_name, input) {
        Ok(result) => {
            let content = serde_json::to_string(&result.data)?;
            record_event(
                recorder,
                options,
                HarnessEvent::ToolFinished {
                    run_id: run_id.clone(),
                    result: result.clone(),
                },
            )?;
            Ok(ToolMessage {
                content,
                is_error: false,
            })
        }
        Err(error) => {
            let reason = error.to_string();
            record_event(
                recorder,
                options,
                HarnessEvent::ToolFailed {
                    run_id: run_id.clone(),
                    call_id,
                    reason: reason.clone(),
                },
            )?;
            Ok(ToolMessage {
                content: reason,
                is_error: true,
            })
        }
    }
}

fn proposals_from_response(response: &ModelResponse) -> AppResult<Vec<ToolProposal>> {
    response
        .tool_uses()
        .into_iter()
        .map(|(_, name, input)| {
            Ok(ToolProposal {
                tool: ToolName::new(name)?,
                input,
            })
        })
        .collect()
}

fn tool_call(call_id: ToolCallId, name: &str, input: Value) -> AppResult<ToolCall> {
    Ok(ToolCall {
        id: call_id,
        tool: ToolName::new(name)?,
        effect: effect_for_tool(name),
        input,
    })
}

fn evaluate_policy(enabled_tools: &[String], call: &ToolCall) -> PolicyDecision {
    if enabled_tools
        .iter()
        .any(|enabled| enabled == call.tool.as_str())
    {
        call.effect.default_policy()
    } else {
        PolicyDecision::Deny {
            reason: format!("tool is not enabled: {}", call.tool),
        }
    }
}

fn context_pack(request: &ModelRequest, token_budget: u32) -> AppResult<ContextPack> {
    let messages = serde_json::to_string(&request.messages)?;
    let tools = serde_json::to_string(&request.tools)?;
    Ok(ContextPack {
        token_budget,
        fragments: vec![
            ContextFragment {
                lane: ContextLane::SystemContract,
                source: "system_prompt".into(),
                content: request.system.clone(),
                estimated_tokens: estimate_tokens(&request.system),
            },
            ContextFragment {
                lane: ContextLane::RecentTurns,
                source: "model.messages".into(),
                estimated_tokens: estimate_tokens(&messages),
                content: messages,
            },
            ContextFragment {
                lane: ContextLane::ToolSchemas,
                source: "model.tools".into(),
                estimated_tokens: estimate_tokens(&tools),
                content: tools,
            },
        ],
    })
}

fn token_limit_field(kind: &ProviderKind) -> TokenLimitField {
    match kind {
        ProviderKind::OpenAi => TokenLimitField::MaxCompletionTokens,
        ProviderKind::OpenRouter => TokenLimitField::MaxTokens,
    }
}

fn estimate_tokens(content: &str) -> u32 {
    let estimate = (content.chars().count() / 4).saturating_add(1);
    estimate.try_into().unwrap_or(u32::MAX)
}

pub fn new_run_id() -> AppResult<RunId> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    Ok(RunId::new(format!(
        "run_{}_{}",
        millis,
        std::process::id()
    ))?)
}

pub fn new_session_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("session_{}_{}", millis, std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use platonic_core::{EffectClass, RunPhase, RunReadback};
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn yolo_auto_grants_required_approval() {
        let policy = PolicyDecision::RequireApproval {
            reason: "requires approval".into(),
        };

        assert_eq!(
            ApprovalMode::AutoApprove.auto_grant_actor(&policy),
            Some("yolo")
        );
        assert_eq!(ApprovalMode::Prompt.auto_grant_actor(&policy), None);
        assert_eq!(
            (ApprovalMode::Deny { actor: "daemon" }).auto_grant_actor(&policy),
            None
        );
    }

    #[test]
    fn deny_mode_marks_required_approval_as_denied() {
        let policy = PolicyDecision::RequireApproval {
            reason: "requires approval".into(),
        };

        assert_eq!(
            (ApprovalMode::Deny { actor: "daemon" }).deny_actor(&policy),
            Some("daemon")
        );
        assert_eq!(ApprovalMode::Prompt.deny_actor(&policy), None);
    }

    #[test]
    fn yolo_does_not_auto_grant_denials() {
        let policy = PolicyDecision::Deny {
            reason: "disabled".into(),
        };

        assert_eq!(ApprovalMode::AutoApprove.auto_grant_actor(&policy), None);
    }

    #[test]
    fn disabled_tools_still_deny() {
        let call = ToolCall {
            id: ToolCallId::new("call_1").unwrap(),
            tool: ToolName::new("file.write").unwrap(),
            effect: EffectClass::WorkspaceWrite,
            input: json!({"path": "out.txt", "content": "hello"}),
        };

        assert!(matches!(
            evaluate_policy(&["file.read".into()], &call),
            PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn enabled_file_read_is_allowed() {
        let call = ToolCall {
            id: ToolCallId::new("call_1").unwrap(),
            tool: ToolName::new("file.read").unwrap(),
            effect: EffectClass::ReadOnly,
            input: json!({"path": "README.md"}),
        };

        assert_eq!(
            evaluate_policy(&["file.read".into()], &call),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn enabled_file_list_is_allowed() {
        let call = ToolCall {
            id: ToolCallId::new("call_1").unwrap(),
            tool: ToolName::new("file.list").unwrap(),
            effect: EffectClass::ReadOnly,
            input: json!({"path": "."}),
        };

        assert_eq!(
            evaluate_policy(&["file.list".into()], &call),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn enabled_file_write_requires_approval() {
        let call = ToolCall {
            id: ToolCallId::new("call_1").unwrap(),
            tool: ToolName::new("file.write").unwrap(),
            effect: EffectClass::WorkspaceWrite,
            input: json!({"path": "out.txt", "content": "hello"}),
        };

        assert!(matches!(
            evaluate_policy(&["file.write".into()], &call),
            PolicyDecision::RequireApproval { .. }
        ));
    }

    #[test]
    fn enabled_file_edit_requires_approval() {
        let call = ToolCall {
            id: ToolCallId::new("call_1").unwrap(),
            tool: ToolName::new("file.edit").unwrap(),
            effect: EffectClass::WorkspaceWrite,
            input: json!({"path": "out.txt", "content": "hello"}),
        };

        assert!(matches!(
            evaluate_policy(&["file.edit".into()], &call),
            PolicyDecision::RequireApproval { .. }
        ));
    }

    #[test]
    fn session_hydration_includes_prior_turns_and_current_question() {
        let config = Config::default();
        let tools = tool_specs(&config.tools.enabled);
        let turns = vec![SessionTurn {
            question: "first question".into(),
            final_answer: "first answer".into(),
        }];

        let messages = hydrated_messages(&turns, "second question", &config, &tools).unwrap();

        assert_eq!(messages.len(), 3);
        assert_eq!(text(&messages[0]), "first question");
        assert_eq!(text(&messages[1]), "first answer");
        assert_eq!(text(&messages[2]), "second question");
    }

    #[test]
    fn session_hydration_drops_oldest_turns_with_marker() {
        let mut config = Config::default();
        config.limits.token_budget = 1_000;
        let tools = tool_specs(&config.tools.enabled);
        let turns = vec![
            SessionTurn {
                question: "old question ".repeat(400),
                final_answer: "old answer ".repeat(400),
            },
            SessionTurn {
                question: "recent question".into(),
                final_answer: "recent answer".into(),
            },
        ];

        let messages = hydrated_messages(&turns, "current question", &config, &tools).unwrap();
        let serialized = serde_json::to_string(&messages).unwrap();

        assert!(serialized.contains(SESSION_TRUNCATION_MARKER));
        assert!(!serialized.contains("old question"));
        assert!(serialized.contains("recent question"));
        assert!(serialized.contains("current question"));
    }

    #[test]
    fn jsonl_context_budget_abort_records_terminal_run_failed() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("plato.toml");
        write_over_budget_config(&config_path);
        let ledger_path = dir.path().join("events.jsonl");

        let err = run_question(over_budget_options(
            &config_path,
            RunLedger::Jsonl(ledger_path.clone()),
            dir.path().to_path_buf(),
            "run_budget_jsonl",
        ))
        .unwrap_err();

        assert_context_budget_error(&err);
        let records = crate::ledger::read_records(&ledger_path).unwrap();
        assert_context_budget_terminal_records(&records);
        let replay = crate::replay::replay_file(&ledger_path).unwrap();
        assert!(replay.contains("final_phase: Failed"));
    }

    #[test]
    fn sqlite_context_budget_abort_records_terminal_run_failed() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("plato.toml");
        write_over_budget_config(&config_path);
        let ledger_path = dir.path().join("events.db");

        let err = run_question(over_budget_options(
            &config_path,
            RunLedger::Sqlite(ledger_path.clone()),
            dir.path().to_path_buf(),
            "run_budget_sqlite",
        ))
        .unwrap_err();

        assert_context_budget_error(&err);
        let records =
            crate::ledger::read_sqlite_records(&ledger_path, Some("run_budget_sqlite")).unwrap();
        assert_context_budget_terminal_records(&records);
        let replay = crate::replay::replay_sqlite(&ledger_path, Some("run_budget_sqlite")).unwrap();
        assert!(replay.contains("final_phase: Failed"));
    }

    fn write_over_budget_config(path: &Path) {
        std::fs::write(
            path,
            r#"
[provider]
api_key_env = "PATH"
base_url = "https://example.invalid"
timeout_ms = 1

[limits]
token_budget = 1
max_output_tokens = 1

[tools]
enabled = ["file.read"]
"#,
        )
        .unwrap();
    }

    fn over_budget_options(
        config_path: &Path,
        ledger: RunLedger,
        workspace_root: PathBuf,
        run_id: &str,
    ) -> RunOptions {
        RunOptions {
            question: "hello".into(),
            config_path: Some(config_path.to_path_buf()),
            ledger,
            workspace_root,
            approval_mode: ApprovalMode::Deny { actor: "test" },
            run_id: Some(RunId::new(run_id).unwrap()),
            session: None,
            event_sender: None,
            cancel: None,
        }
    }

    fn assert_context_budget_error(error: &AppError) {
        assert!(
            error.to_string().contains("context budget exceeded: used "),
            "{error}"
        );
    }

    fn assert_context_budget_terminal_records(records: &[RecordedEvent]) {
        assert_eq!(records.len(), 2);
        assert!(matches!(records[0].event, HarnessEvent::RunStarted { .. }));
        match &records[1].event {
            HarnessEvent::RunFailed { reason, .. } => {
                assert!(reason.contains("context budget exceeded: used "));
                assert!(reason.contains("budget 1"));
            }
            event => panic!("expected run_failed, got {event:?}"),
        }

        let readback = RunReadback::from_events(records).unwrap();
        match readback.final_phase {
            RunPhase::Failed { reason } => {
                assert!(reason.contains("context budget exceeded: used "));
                assert!(reason.contains("budget 1"));
            }
            phase => panic!("expected failed final phase, got {phase:?}"),
        }
    }

    fn text(message: &ModelMessage) -> &str {
        match &message.content[0] {
            ModelBlock::Text { text } => text,
            block => panic!("expected text block, got {block:?}"),
        }
    }
}
