//! Oh My Pi event projection over the shared MCP writer.
use crate::{
    OmpControl,
    types::{BindingId, SavedSessionId},
};
use dione_contracts::MessageRef;
use dione_durable::DeliveryToken;
use dione_harness::{
    AttentionPolicy, HarnessConnection, HarnessDelivery, HarnessError, HarnessProtocol,
    ProtocolOutput,
};
use dione_messages::delivery::Delivery;
use futures_util::future::BoxFuture;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use thiserror::Error;

const ATTENTION_POLICY: AttentionPolicy = AttentionPolicy::Direct;

struct OhMyPiAdapter {
    output: ProtocolOutput,
    control: Option<OmpControl>,
    codex_tool_names: &'static [&'static str],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindArguments {
    session_id: SavedSessionId,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnbindArguments {
    binding_id: BindingId,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryArguments {
    session_id: SavedSessionId,
    binding_id: BindingId,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AckArguments {
    binding_id: BindingId,
    delivery_token: DeliveryToken,
}

fn control_arguments<T: DeserializeOwned>(args: &Value) -> Result<T, HarnessError> {
    serde_path_to_error::deserialize(args).map_err(|_| {
        HarnessError::source(ToolArgumentError(
            "invalid private control arguments".to_owned(),
        ))
    })
}

#[derive(Debug, Error)]
#[error("cannot project Oh My Pi event: {kind}")]
struct ProjectionError {
    #[source]
    kind: ProjectionErrorKind,
}
#[derive(Debug, Error)]
enum ProjectionErrorKind {
    #[error("permission notifications are unsupported")]
    Permission,
}
fn notification(delivery: &Delivery) -> Result<Value, ProjectionError> {
    match dione_presentation::project(delivery) {
        dione_presentation::Presentation::Channel(channel) => Ok(
            json!({"jsonrpc":"2.0","method":"notifications/dione/event","params":{"version":1,"content":channel.content,"meta":channel.meta}}),
        ),
        dione_presentation::Presentation::Permission { .. } => Err(ProjectionError {
            kind: ProjectionErrorKind::Permission,
        }),
    }
}

#[derive(Debug, Error)]
#[error("{0}")]
struct ToolArgumentError(String);

/// Connects OMP notifications and private controls to one protocol writer.
pub fn connect(
    output: ProtocolOutput,
    control: Option<OmpControl>,
    codex_tool_names: &'static [&'static str],
) -> HarnessConnection {
    HarnessConnection::from_adapter(
        OhMyPiAdapter {
            output: output.clone(),
            control,
            codex_tool_names,
        },
        output,
    )
}

impl HarnessDelivery for OhMyPiAdapter {
    fn attention_policy(&self) -> AttentionPolicy {
        ATTENTION_POLICY
    }

    fn deliver<'a>(&'a self, value: &'a Delivery) -> BoxFuture<'a, Result<(), HarnessError>> {
        Box::pin(async move {
            let event = notification(value).map_err(HarnessError::source)?;
            self.output.write(&event).await.map_err(|error| {
                if error.completion_is_unknown() {
                    HarnessError::completion_unknown(error)
                } else {
                    HarnessError::source(error)
                }
            })?;
            Ok(())
        })
    }

    fn invalidate_attention(&self, _source: MessageRef) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async { Ok(()) })
    }
}

impl HarnessProtocol for OhMyPiAdapter {
    fn capabilities(&self) -> Value {
        json!({"tools": {}})
    }
    fn tools(&self) -> Vec<Value> {
        Vec::new()
    }
    fn resident_control(&self) -> bool {
        self.control.is_some()
    }
    fn permission_relay(&self) -> bool {
        false
    }
    fn stdio_initialized(&self) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async move {
            if let Some(control) = &self.control {
                control.initialized().await.map_err(HarnessError::source)?;
            }
            Ok(())
        })
    }
    fn stdio_disconnected(&self) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async move {
            if let Some(control) = &self.control {
                control
                    .disconnect(None)
                    .await
                    .map_err(HarnessError::source)?;
            }
            Ok(())
        })
    }
    fn private_control<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, HarnessError>>> {
        Box::pin(async move {
            let control = self.control.as_ref()?;
            Some(match name {
                "teams_bind_session" => async {
                    let args: BindArguments = control_arguments(args)?;
                    let binding = control.bind(args.session_id).await.map_err(HarnessError::source)?;
                    Ok(json!({"bound":true,"session_id":binding.session_id,"binding_id":binding.binding_id}))
                }.await,
                "teams_check_session" => async {
                    let args: RecoveryArguments = control_arguments(args)?;
                    let status = control.check(args.session_id, args.binding_id).await.map_err(HarnessError::source)?;
                    Ok(json!(status))
                }.await,
                "teams_recover_session" => async {
                    let args: RecoveryArguments = control_arguments(args)?;
                    let status = control.recover(args.session_id, args.binding_id).await.map_err(HarnessError::source)?;
                    Ok(json!(status))
                }.await,
                "teams_unbind_session" => async {
                    let args: UnbindArguments = control_arguments(args)?;
                    control.disconnect(Some(args.binding_id)).await.map_err(HarnessError::source)?;
                    Ok(json!({"bound":false}))
                }.await,
                "teams_ack_event" => async {
                    let args: AckArguments = control_arguments(args)?;
                    let acknowledged = control.ack(args.binding_id, args.delivery_token).await.map_err(HarnessError::source)?;
                    Ok(json!({"acknowledged":acknowledged}))
                }.await,
                _ => return None,
            })
        })
    }
    fn call<'a>(
        &'a self,
        name: &'a str,
        _args: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, HarnessError>>> {
        Box::pin(async move {
            self.codex_tool_names.contains(&name).then(|| {
                Err(HarnessError::source(ToolArgumentError(format!(
                    "{name} is only available in Codex mode"
                ))))
            })
        })
    }
}
