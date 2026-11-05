//! Principal-bound publication tools. Authentication never comes from tool arguments.

use super::{
    AuthenticatedRecipient, ClaimGrant, EventKey, OwnerError, PublicationError,
    PublicationErrorKind, RookHandle, TextPublication,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishArguments {
    grant: ClaimGrant,
    request: TextPublication,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArguments {
    event: EventKey,
}

/// A narrow text-publication dispatcher bound to one authenticated connection.
#[derive(Clone)]
pub struct PublicationTools {
    recipient: AuthenticatedRecipient,
    owner: RookHandle,
}
impl PublicationTools {
    /// Bind trusted connection authentication to an owner without exposing operator tools.
    pub fn new(recipient: AuthenticatedRecipient, owner: RookHandle) -> Self {
        Self { recipient, owner }
    }

    /// Advertise exact text publication and status under the installed adapter's limits.
    pub fn definitions(&self) -> Vec<Value> {
        let capabilities = self.owner.publication_capabilities();
        let mut definitions = vec![json!({
            "name":"rook_publication_status",
            "description":"Inspect the retained default-reply slot. Unknown requires investigation and never permits resend.",
            "inputSchema":{"type":"object","additionalProperties":false,"required":["event"],
                "properties":{"event":{"$ref":"#/$defs/event"}},"$defs":super::tools::identity_definitions()}
        })];
        if capabilities.text {
            let mut defs = super::tools::identity_definitions();
            defs["target"] = json!({"oneOf":[
                {"type":"object","additionalProperties":false,"required":["kind","target"],"properties":{"kind":{"const":"room"},"target":{"$ref":"#/$defs/room"}}},
                {"type":"object","additionalProperties":false,"required":["kind","target"],"properties":{"kind":{"const":"reply"},"target":{"$ref":"#/$defs/message"}}}
            ]});
            defs["mentions"] = json!({"oneOf":[
                {"type":"object","additionalProperties":false,"required":["kind"],"properties":{"kind":{"const":"suppress_all"}}},
                {"type":"object","additionalProperties":false,"required":["kind","users","reply_author"],"properties":{"kind":{"const":"explicit_users"},"users":{"type":"array","maxItems":100,"uniqueItems":true,"items":{"$ref":"#/$defs/room"}},"reply_author":{"type":"boolean"}}}
            ]});
            defs["grant"] = json!({"type":"object","additionalProperties":false,"required":["token","generation","authorization_revision","expires_at"],"properties":{
                "token":{"type":"string","pattern":"^[0-9a-fA-F]{64}$"},"generation":{"type":"integer","minimum":0},"authorization_revision":{"type":"integer","minimum":0},"expires_at":{"type":"string","format":"date-time"}
            }});
            definitions.push(json!({
                "name":"rook_publish_text",
                "description":"Reserve one text message in the event's default-reply slot. Retrying a matching key or request returns the same slot. Mentions are explicit and attachments are unsupported.",
                "inputSchema":{"type":"object","additionalProperties":false,"required":["grant","request"],"properties":{
                    "grant":{"$ref":"#/$defs/grant"},
                    "request":{"type":"object","additionalProperties":false,"required":["event","key","target","text"],"properties":{
                        "event":{"$ref":"#/$defs/event"},"key":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[!-~]+$"},"target":{"$ref":"#/$defs/target"},
                        "text":{"type":"string","minLength":1,"maxLength":capabilities.max_text_bytes.min(32768),"description":"UTF-8 bytes are additionally limited; native prepared single-message limits also apply."},"mentions":{"$ref":"#/$defs/mentions"}
                    }}
                },"$defs":defs}
            }));
        }
        definitions
    }

    /// Execute under this connection's fixed principal. Extra fields are rejected.
    pub async fn call(&self, name: &str, arguments: Value) -> Result<Value, OwnerError> {
        match name {
            "rook_publish_text" => {
                let arguments: PublishArguments =
                    serde_path_to_error::deserialize(arguments).map_err(decoding_error)?;
                let status = self
                    .owner
                    .publish(self.recipient.clone(), arguments.grant, arguments.request)
                    .await?;
                serde_json::to_value(status).map_err(encoding_error)
            }
            "rook_publication_status" => {
                let arguments: StatusArguments =
                    serde_path_to_error::deserialize(arguments).map_err(decoding_error)?;
                let status = self
                    .owner
                    .publication_status(self.recipient.clone(), arguments.event)
                    .await?;
                serde_json::to_value(status).map_err(encoding_error)
            }
            _ => Err(PublicationError::new(PublicationErrorKind::Unsupported).into()),
        }
    }
}

fn encoding_error(error: serde_json::Error) -> OwnerError {
    PublicationError::new(PublicationErrorKind::Encoding(error)).into()
}

fn decoding_error(error: serde_path_to_error::Error<serde_json::Error>) -> OwnerError {
    PublicationError::new(PublicationErrorKind::Decode(error)).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_serialization_failure_retains_encoding_category() {
        struct Rejected;
        impl serde::Serialize for Rejected {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("private-output-sentinel"))
            }
        }
        let error = encoding_error(serde_json::to_value(Rejected).unwrap_err());
        assert!(matches!(
            &error.kind,
            super::super::OwnerErrorKind::Publication(publication)
                if matches!(&publication.kind, PublicationErrorKind::Encoding(_))
        ));
        assert!(!error.to_string().contains("private-output-sentinel"));
    }
}
