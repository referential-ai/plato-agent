use crate::{
    AppError, AppResult,
    anthropic::{AnthropicClient, AnthropicMessage, AnthropicResponse, system_prompt},
    config::Config,
    ledger::EventRecorder,
    tools::{ApprovalOutcome, ask_for_approval, effect_for_tool, execute_tool},
};
use platonic_core::{
    ActorId, AgentId, ContextFragment, ContextLane, ContextPack, HarnessEvent, Message,
    MessageRole, ModelName, ModelUsage, PolicyDecision, RunId, ToolCall, ToolCallId, ToolName,
    ToolProposal, TurnId,
};
use serde_json::Value;
use std::path::PathBuf;

const MAX_TURNS: u32 = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunOptions {
    pub question: String,
    pub config_path: PathBuf,
    pub events_path: PathBuf,
    pub workspace_root: PathBuf,
}

pub fn run_question(options: RunOptions) -> AppResult<()> {
    if options.question.trim().is_empty() {
        return Err(AppError::EmptyQuestion);
    }

    let config = Config::load(&options.config_path)?;
    let client = AnthropicClient::from_env(&config.provider.api_key_env)?;
    let mut recorder = EventRecorder::create(&options.events_path)?;
    let run_id = RunId::new(new_run_id())?;
    let agent_id = AgentId::new("plato")?;
    let model = ModelName::new(config.provider.model.clone())?;
    let actor_id = ActorId::new("stdin")?;

    recorder.record(HarnessEvent::RunStarted {
        run_id: run_id.clone(),
        agent_id,
    })?;

    let mut messages = vec![AnthropicMessage::user_text(options.question.clone())];
    let mut next_context = options.question;

    for turn_index in 0..MAX_TURNS {
        let turn_id = TurnId::new(format!("turn_{}", turn_index + 1))?;
        let context = context_pack(&next_context, config.limits.token_budget);
        recorder.record(HarnessEvent::ContextBuilt {
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
            context,
        })?;
        recorder.record(HarnessEvent::ModelRequested {
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
            step: turn_index,
            model: model.clone(),
        })?;

        let response = match client.send(
            model.as_str(),
            system_prompt(),
            &messages,
            &config.tools.enabled,
        ) {
            Ok(response) => response,
            Err(error) => {
                recorder.record(HarnessEvent::RunFailed {
                    run_id,
                    reason: error.to_string(),
                })?;
                return Err(error);
            }
        };

        let proposals = proposals_from_response(&response)?;
        recorder.record(HarnessEvent::ModelResponded {
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
            step: turn_index,
            output: Message {
                role: MessageRole::Assistant,
                content: response.text(),
            },
            proposed_calls: proposals.clone(),
            usage: ModelUsage {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
            },
        })?;

        let tool_uses = response.tool_uses();
        if tool_uses.is_empty() {
            println!("{}", response.text());
            recorder.record(HarnessEvent::RunFinished { run_id })?;
            return Ok(());
        }

        if tool_uses.len() > 1 {
            let reason = "model requested multiple tools in one response".to_string();
            recorder.record(HarnessEvent::RunFailed { run_id, reason })?;
            return Err(AppError::RunFailed(
                "model requested multiple tools in one response".into(),
            ));
        }

        if !response.text().trim().is_empty() {
            eprintln!("{}", response.text());
        }

        messages.push(AnthropicMessage::assistant_blocks(response.content.clone()));
        let (tool_use_id, tool_name, input) = tool_uses.into_iter().next().expect("checked len");
        let call_id = ToolCallId::new(tool_use_id.clone())?;
        let call = tool_call(call_id.clone(), &tool_name, input.clone())?;
        recorder.record(HarnessEvent::ToolCallProposed {
            run_id: run_id.clone(),
            turn_id,
            call: call.clone(),
        })?;

        let policy = if config
            .tools
            .enabled
            .iter()
            .any(|enabled| enabled == &tool_name)
        {
            call.effect.default_policy()
        } else {
            PolicyDecision::Deny {
                reason: format!("tool is not enabled: {tool_name}"),
            }
        };
        recorder.record(HarnessEvent::PolicyEvaluated {
            run_id: run_id.clone(),
            call_id: call_id.clone(),
            decision: policy.clone(),
        })?;

        let tool_message = match policy {
            PolicyDecision::Allow => execute_and_record_tool(
                &mut recorder,
                &run_id,
                &options.workspace_root,
                call_id.clone(),
                &tool_name,
                input,
            )?,
            PolicyDecision::RequireApproval { reason: _ } => {
                match ask_for_approval(&tool_name, &call.input)? {
                    ApprovalOutcome::Granted => {
                        recorder.record(HarnessEvent::ApprovalGranted {
                            run_id: run_id.clone(),
                            call_id: call_id.clone(),
                            actor_id: actor_id.clone(),
                        })?;
                        execute_and_record_tool(
                            &mut recorder,
                            &run_id,
                            &options.workspace_root,
                            call_id.clone(),
                            &tool_name,
                            input,
                        )?
                    }
                    ApprovalOutcome::Denied { reason } => {
                        recorder.record(HarnessEvent::ApprovalDenied {
                            run_id: run_id.clone(),
                            call_id,
                            actor_id: actor_id.clone(),
                            reason: reason.clone(),
                        })?;
                        ToolMessage {
                            content: reason,
                            is_error: true,
                        }
                    }
                }
            }
            PolicyDecision::Deny { reason } => ToolMessage {
                content: reason,
                is_error: true,
            },
        };

        next_context = format!("Tool result for {tool_use_id}: {}", tool_message.content);
        messages.push(AnthropicMessage::tool_result(
            tool_use_id,
            tool_message.content,
            tool_message.is_error,
        ));
    }

    recorder.record(HarnessEvent::RunFailed {
        run_id,
        reason: format!("exceeded maximum turn count of {MAX_TURNS}"),
    })?;
    Err(AppError::RunFailed(format!(
        "exceeded maximum turn count of {MAX_TURNS}"
    )))
}

#[derive(Debug)]
struct ToolMessage {
    content: String,
    is_error: bool,
}

fn execute_and_record_tool(
    recorder: &mut EventRecorder,
    run_id: &RunId,
    workspace_root: &std::path::Path,
    call_id: ToolCallId,
    tool_name: &str,
    input: Value,
) -> AppResult<ToolMessage> {
    recorder.record(HarnessEvent::ToolStarted {
        run_id: run_id.clone(),
        call_id: call_id.clone(),
    })?;

    match execute_tool(workspace_root, call_id.clone(), tool_name, input) {
        Ok(result) => {
            let content = serde_json::to_string(&result.data)?;
            recorder.record(HarnessEvent::ToolFinished {
                run_id: run_id.clone(),
                result: result.clone(),
            })?;
            Ok(ToolMessage {
                content,
                is_error: false,
            })
        }
        Err(error) => {
            let reason = error.to_string();
            recorder.record(HarnessEvent::ToolFailed {
                run_id: run_id.clone(),
                call_id,
                reason: reason.clone(),
            })?;
            Ok(ToolMessage {
                content: reason,
                is_error: true,
            })
        }
    }
}

fn proposals_from_response(response: &AnthropicResponse) -> AppResult<Vec<ToolProposal>> {
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

fn context_pack(content: &str, token_budget: u32) -> ContextPack {
    ContextPack {
        token_budget,
        fragments: vec![ContextFragment {
            lane: ContextLane::CurrentTask,
            source: "cli".into(),
            content: content.into(),
            estimated_tokens: estimate_tokens(content),
        }],
    }
}

fn estimate_tokens(content: &str) -> u32 {
    let estimate = (content.chars().count() / 4).saturating_add(1);
    estimate.try_into().unwrap_or(u32::MAX)
}

fn new_run_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("run_{}_{}", millis, std::process::id())
}
