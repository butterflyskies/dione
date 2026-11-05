use dione_teams_identities::{
    ActivityId, ActorId, ApplicationId, BotId, ConversationId, IdentityErrorKind, IdentityKind,
    TenantId,
};
use serde::{Deserialize, Serialize};
use std::{any::TypeId, error::Error};

#[test]
fn external_conversions_and_validating_serde_preserve_literal_boundaries() {
    macro_rules! check {
        ($id:ty, $kind:expr) => {{
            for value in [
                " Mixed/CASE + opaque ".to_owned(),
                " ".to_owned(),
                "x".repeat(512),
                "é".repeat(256),
            ] {
                let id = <$id>::parse(value.clone()).unwrap();
                assert_eq!(id.as_str(), value);
                assert_eq!(id.to_string(), value);
                assert_eq!(value.parse::<$id>().unwrap(), id);
                assert_eq!(<$id>::try_from(value.clone()).unwrap(), id);
                assert_eq!(String::from(id.clone()), value);
                let encoded = serde_json::to_string(&value).unwrap();
                assert_eq!(serde_json::to_string(&id).unwrap(), encoded);
                assert_eq!(serde_json::from_str::<$id>(&encoded).unwrap(), id);
            }
            for (value, expected) in [
                (String::new(), IdentityErrorKind::Empty),
                ("x".repeat(513), IdentityErrorKind::TooLong),
                ("é".repeat(257), IdentityErrorKind::TooLong),
                ("x".repeat(512) + "\n", IdentityErrorKind::TooLong),
                ("a\n".to_owned(), IdentityErrorKind::ControlCharacter),
                ("a\0".to_owned(), IdentityErrorKind::ControlCharacter),
                ("a\u{7f}".to_owned(), IdentityErrorKind::ControlCharacter),
                ("a\u{85}".to_owned(), IdentityErrorKind::ControlCharacter),
            ] {
                let error = <$id>::parse(value.clone()).unwrap_err();
                assert_eq!(error.identity, $kind);
                assert_eq!(error.kind, expected);
                assert!(error.source().is_some());
                let encoded = serde_json::to_string(&value).unwrap();
                assert!(serde_json::from_str::<$id>(&encoded).is_err());
            }
            for encoded in ["0", "null", "[]", "{}"] {
                assert!(serde_json::from_str::<$id>(encoded).is_err());
            }
        }};
    }
    check!(TenantId, IdentityKind::Tenant);
    check!(ApplicationId, IdentityKind::Application);
    check!(BotId, IdentityKind::Bot);
    check!(ActorId, IdentityKind::Actor);
    check!(ConversationId, IdentityKind::Conversation);
    check!(ActivityId, IdentityKind::Activity);
}

#[test]
fn literal_legacy_identity_record_roundtrips_without_normalization() {
    #[derive(Deserialize, Serialize)]
    struct Record {
        tenant_id: TenantId,
        app_id: ApplicationId,
        bot_id: BotId,
        sender_id: ActorId,
        conversation_id: ConversationId,
        activity_id: ActivityId,
    }
    let literal = r#"{"tenant_id":" Tenant + A ","app_id":"App/CASE","bot_id":"28:Bot","sender_id":"29:User","conversation_id":"19:Room@thread.v2","activity_id":"Activity +/ 001"}"#;
    let record: Record = serde_json::from_str(literal).unwrap();
    assert_eq!(record.tenant_id.as_str(), " Tenant + A ");
    assert_eq!(record.app_id.as_str(), "App/CASE");
    assert_eq!(record.bot_id.as_str(), "28:Bot");
    assert_eq!(record.sender_id.as_str(), "29:User");
    assert_eq!(record.conversation_id.as_str(), "19:Room@thread.v2");
    assert_eq!(record.activity_id.as_str(), "Activity +/ 001");
    assert_eq!(serde_json::to_string(&record).unwrap(), literal);
}

#[test]
fn all_six_semantic_identity_types_are_distinct() {
    let types = [
        TypeId::of::<TenantId>(),
        TypeId::of::<ApplicationId>(),
        TypeId::of::<BotId>(),
        TypeId::of::<ActorId>(),
        TypeId::of::<ConversationId>(),
        TypeId::of::<ActivityId>(),
    ];
    for (index, current) in types.iter().enumerate() {
        for other in &types[index + 1..] {
            assert_ne!(current, other);
        }
    }
}
