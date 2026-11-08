use dione_mcp_tools::{
    McpTool, RegisteredTool, RegistrationErrorKind, ToolCallError, ToolCallErrorKind,
    ToolDefinition, ToolDiscoveryContext, ToolName, ToolRegistry,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{collections::BTreeSet, error::Error, sync::Arc};

struct Echo {
    prefix: String,
}
impl McpTool for Echo {
    fn present_definition(
        &self,
        definition: &ToolDefinition,
        context: ToolDiscoveryContext,
    ) -> Value {
        let mut result = json!(definition);
        result["description"] = json!(if context.evidence_markers_enabled {
            "markers enabled"
        } else {
            "markers disabled"
        });
        result
    }
    fn call<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<Value, ToolCallError>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            Ok(json!({"prefix": self.prefix, "argument": arguments["text"]}))
        })
    }
}
fn registration(name: &str) -> RegisteredTool {
    RegisteredTool::new(
        ToolDefinition::new(
            ToolName::new(name).unwrap(),
            "original description",
            json!({"type": "object"}),
        ),
        Arc::new(Echo {
            prefix: "borrowed handler".to_owned(),
        }),
    )
}
fn context(enabled: bool) -> ToolDiscoveryContext {
    ToolDiscoveryContext {
        evidence_markers_enabled: enabled,
        evidence_marker_version: dione_evidence::EvidenceMarkerVersion::V3,
    }
}
#[test]
fn names_and_serialized_definition_preserve_exact_contract() {
    for name in ["", "two words", "a/b", "é", "line\nbreak"] {
        assert!(ToolName::new(name).is_err(), "{name:?}");
    }
    assert!(ToolName::new(&"a".repeat(129)).is_err());
    assert!(ToolName::new(&"a".repeat(128)).is_ok());
    let name = ToolName::new("Extension.Valid-name_1").unwrap();
    assert_eq!(name.to_string(), "Extension.Valid-name_1");
    let definition = ToolDefinition::new(name, "literal description", json!({"type":"object"}));
    assert_eq!(
        json!(definition),
        json!({"name":"Extension.Valid-name_1", "description":"literal description", "inputSchema":{"type":"object"}})
    );
}
#[test]
fn reservation_precedes_duplicates_and_preserves_first_offender() {
    let reserved = BTreeSet::from(["reserved".to_owned()]);
    let error = ToolRegistry::new(
        &reserved,
        vec![registration("reserved"), registration("reserved")],
    )
    .err()
    .unwrap();
    assert_eq!(error.name.to_string(), "reserved");
    assert!(matches!(error.kind, RegistrationErrorKind::Reserved));
    assert_eq!(
        error.source().unwrap().to_string(),
        "name is reserved by a built-in or harness tool"
    );
    let error = ToolRegistry::new(
        &reserved,
        vec![
            registration("extension"),
            registration("extension"),
            registration("reserved"),
        ],
    )
    .err()
    .unwrap();
    assert_eq!(error.name.to_string(), "extension");
    assert!(matches!(error.kind, RegistrationErrorKind::Duplicate));
}
#[test]
fn discovery_is_lexical_contextual_and_preserves_malformed_envelopes() {
    let registry = ToolRegistry::new(
        &BTreeSet::new(),
        vec![registration("z"), registration("a").with_resident_access()],
    )
    .unwrap();
    assert!(!registry.resident_allowed("z"));
    assert!(registry.resident_allowed("a"));
    assert!(!registry.resident_allowed("missing"));
    for enabled in [false, true, false] {
        let mut response = json!({"tools":[{"name":"built_in"}]});
        registry
            .clone()
            .append_definitions(&mut response, context(enabled));
        assert_eq!(response["tools"][0], json!({"name":"built_in"}));
        assert_eq!(response["tools"][1]["name"], "a");
        assert_eq!(response["tools"][2]["name"], "z");
        assert_eq!(
            response["tools"][1]["description"],
            if enabled {
                "markers enabled"
            } else {
                "markers disabled"
            }
        );
        assert_eq!(
            response["tools"][1]["inputSchema"],
            json!({"type":"object"})
        );
    }
    for mut value in [
        json!({}),
        json!({"tools":null}),
        json!({"tools":{}}),
        json!({"tools":"raw"}),
    ] {
        let expected = if value == json!({}) {
            json!({"tools":null})
        } else {
            value.clone()
        };
        registry.append_definitions(&mut value, context(true));
        assert_eq!(value, expected);
    }
    let empty = ToolRegistry::default();
    assert!(empty.find("a").is_none());
    let mut response = json!({"tools":[]});
    empty.append_definitions(&mut response, context(false));
    assert_eq!(response, json!({"tools":[]}));
}
#[tokio::test]
async fn external_handler_borrows_arguments_and_returns_send_future() {
    fn assert_send<T: Send>(value: T) -> T {
        value
    }
    let registry = ToolRegistry::new(&BTreeSet::new(), vec![registration("echo")]).unwrap();
    let arguments = json!({"text":"literal borrowed argument"});
    let result = assert_send(registry.find("echo").unwrap().call(&arguments))
        .await
        .unwrap();
    assert_eq!(
        result,
        json!({"prefix":"borrowed handler", "argument":"literal borrowed argument"})
    );
}
#[test]
fn typed_error_categories_retain_original_sources() {
    for (kind, display) in [
        (
            ToolCallErrorKind::OwnerInput(Box::new(std::io::Error::other("literal cause"))),
            "literal cause",
        ),
        (
            ToolCallErrorKind::Arguments(Box::new(std::io::Error::other("literal cause"))),
            "tool call failed: invalid arguments: literal cause",
        ),
        (
            ToolCallErrorKind::Execution(Box::new(std::io::Error::other("literal cause"))),
            "tool call failed: execution failed: literal cause",
        ),
    ] {
        let error = ToolCallError { kind };
        assert_eq!(error.to_string(), display);
        let category = error.source().unwrap();
        assert_eq!(category.source().unwrap().to_string(), "literal cause");
        match error.kind {
            ToolCallErrorKind::OwnerInput(source)
            | ToolCallErrorKind::Arguments(source)
            | ToolCallErrorKind::Execution(source) => {
                assert_eq!(source.to_string(), "literal cause")
            }
        }
    }
}

struct DefaultPresentation;
impl McpTool for DefaultPresentation {
    fn call<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<Value, ToolCallError>> {
        Box::pin(async move { Ok(arguments.clone()) })
    }
}
#[test]
fn default_handler_presents_exact_definition() {
    let registry = ToolRegistry::new(
        &BTreeSet::new(),
        vec![RegisteredTool::new(
            ToolDefinition::new(
                ToolName::new("default_tool").unwrap(),
                "default description",
                json!({"type":"object", "required":["message"]}),
            ),
            Arc::new(DefaultPresentation),
        )],
    )
    .unwrap();
    let mut response = json!({"tools":[]});
    registry.append_definitions(&mut response, context(false));
    assert_eq!(
        response,
        json!({"tools":[{"name":"default_tool", "description":"default description", "inputSchema":{"type":"object", "required":["message"]}}]})
    );
}

#[test]
fn selected_version_and_enable_gate_are_forwarded_together() {
    struct VersionEcho;
    impl McpTool for VersionEcho {
        fn present_definition(
            &self,
            definition: &ToolDefinition,
            context: ToolDiscoveryContext,
        ) -> Value {
            let mut value = json!(definition);
            value["transport"] = json!(context.evidence_marker_version.transport());
            value["enabled"] = json!(context.evidence_markers_enabled);
            value
        }
        fn call<'a>(
            &'a self,
            _arguments: &'a Value,
        ) -> BoxFuture<'a, Result<Value, ToolCallError>> {
            Box::pin(async { Ok(json!({})) })
        }
    }
    let registry = ToolRegistry::new(
        &BTreeSet::new(),
        vec![RegisteredTool::new(
            ToolDefinition::new(ToolName::new("version_probe").unwrap(), "probe", json!({})),
            Arc::new(VersionEcho),
        )],
    )
    .unwrap();
    for (version, expected) in [
        (
            dione_evidence::EvidenceMarkerVersion::V2,
            "terminal-visible-role-suffix-v2-after-hooks",
        ),
        (
            dione_evidence::EvidenceMarkerVersion::V3,
            "terminal-visible-role-suffix-v3-after-hooks",
        ),
    ] {
        for enabled in [false, true] {
            let mut response = json!({"tools": []});
            registry.append_definitions(
                &mut response,
                ToolDiscoveryContext {
                    evidence_markers_enabled: enabled,
                    evidence_marker_version: version,
                },
            );
            assert_eq!(response["tools"][0]["transport"], expected);
            assert_eq!(response["tools"][0]["enabled"], enabled);
        }
    }
}
