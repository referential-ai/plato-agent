use crate::{
    AppError, AppResult,
    config::{Config, ProviderKind},
    ledger::EventRecorder,
    model::{
        ModelMessage, ModelRequest, ModelResponse, ModelStop, OpenAiCompatibleClient,
        TokenLimitField, system_prompt,
    },
    tool_catalog::{effect_for_tool, tool_specs},
    tools::{ApprovalOutcome, ask_for_approval, execute_tool},
};
use platonic_core::{
    ActorId, AgentId, ContextFragment, ContextLane, ContextPack, EffectClass, HarnessEvent,
    Message, MessageRole, ModelName, PolicyDecision, RecordedEvent, RunId, ToolCall, ToolCallId,
    ToolName, ToolProposal, TurnId,
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

const MAX_TURNS: u32 = 8;

#[derive(Clone, Debug)]
pub struct RunOptions {
    pub question: String,
    pub config_path: PathBuf,
    pub ledger: RunLedger,
    pub workspace_root: PathBuf,
    pub approval_mode: ApprovalMode,
    pub run_id: Option<RunId>,
    pub event_sender: Option<Sender<RecordedEvent>>,
    pub cancel: Option<Arc<AtomicBool>>,
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

pub fn run_question(options: RunOptions) -> AppResult<RunOutcome> {
    if options.question.trim().is_empty() {
        return Err(AppError::EmptyQuestion);
    }

    let config = Config::load(&options.config_path)?;
    let run_id = options.run_id.clone().unwrap_or(new_run_id()?);
    let client = OpenAiCompatibleClient::from_config(
        &config.provider.api_key_env,
        config.provider.base_url.clone(),
        config.provider.timeout_ms,
        config.provider.http_referer.clone(),
        config.provider.app_title.clone(),
        token_limit_field(&config.provider.kind),
    )?;
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

    let mut messages = vec![ModelMessage::user_text(options.question.clone())];
    let tools = tool_specs(&config.tools.enabled);

    for turn_index in 0..MAX_TURNS {
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
        record_event(
            &mut recorder,
            &options,
            HarnessEvent::ContextBuilt {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                context,
            },
        )?;
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
                record_event(
                    &mut recorder,
                    &options,
                    HarnessEvent::RunFailed {
                        run_id,
                        reason: error.to_string(),
                    },
                )?;
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

    record_event(
        &mut recorder,
        &options,
        HarnessEvent::RunFailed {
            run_id,
            reason: format!("exceeded maximum turn count of {MAX_TURNS}"),
        },
    )?;
    Err(AppError::RunFailed(format!(
        "exceeded maximum turn count of {MAX_TURNS}"
    )))
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

#[cfg(test)]
mod tests {
    use super::*;
    use platonic_core::EffectClass;
    use serde_json::json;

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
}
