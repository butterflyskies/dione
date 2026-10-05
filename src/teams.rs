//! Hermetic feasibility proof for a Rust-native Microsoft Teams transport.
//!
//! The types in this module enforce the documented Bot Connector JWT/JWKS
//! boundary and preserve the correlated state needed to construct a reply.

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::Jwk};
#[cfg(test)]
use jsonwebtoken::{EncodingKey, Header, encode};
use reqwest::{Method, Url};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, Visitor},
};
#[cfg(test)]
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

#[cfg(test)]
const CONNECTOR_ISSUER: &str = "https://api.botframework.com";
#[cfg(test)]
const APP_ID: &str = "00000000-1111-2222-3333-444444444444";
#[cfg(test)]
const TENANT_ID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
#[cfg(test)]
const KEY_ID: &str = "fixture-rsa-1";
#[cfg(test)]
const NOW: u64 = 1_800_000_000;
const CLOCK_SKEW_SECONDS: u64 = 300;
#[cfg(test)]
const PRIVATE_KEY: &[u8] = include_bytes!("../tests/fixtures/teams/connector-private.pem");
#[cfg(test)]
const OPENID_DOCUMENT: &[u8] = include_bytes!("../tests/fixtures/teams/connector-openid.json");
#[cfg(test)]
const JWKS_DOCUMENT: &[u8] = include_bytes!("../tests/fixtures/teams/connector-jwks.json");

#[derive(Clone, Deserialize)]
pub(crate) struct FrozenOpenId {
    pub issuer: String,
    #[serde(rename = "id_token_signing_alg_values_supported")]
    pub signing_algorithms: Vec<Algorithm>,
    #[serde(skip)]
    pub keys: Vec<ConnectorSigningKey>,
}

#[derive(Clone)]
pub(crate) struct ConnectorSigningKey {
    pub jwk: Jwk,
    pub endorsements: BTreeSet<String>,
}

impl<'de> Deserialize<'de> for ConnectorSigningKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ConnectorKeyVisitor;

        impl<'de> Visitor<'de> for ConnectorKeyVisitor {
            type Value = ConnectorSigningKey;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a connector JWK with optional channel endorsements")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
                let mut jwk_fields = serde_json::Map::new();
                let mut endorsements = None;
                while let Some((key, value)) = access.next_entry::<String, serde_json::Value>()? {
                    if key == "endorsements" {
                        if endorsements.is_some() {
                            return Err(de::Error::duplicate_field("endorsements"));
                        }
                        endorsements =
                            Some(serde_json::from_value(value).map_err(de::Error::custom)?);
                    } else if jwk_fields.insert(key.clone(), value).is_some() {
                        return Err(de::Error::custom(format!("duplicate JWK field `{key}`")));
                    }
                }
                let jwk = serde_json::from_value(serde_json::Value::Object(jwk_fields))
                    .map_err(de::Error::custom)?;
                Ok(ConnectorSigningKey {
                    jwk,
                    endorsements: endorsements.unwrap_or_default(),
                })
            }
        }

        deserializer.deserialize_map(ConnectorKeyVisitor)
    }
}

#[derive(Deserialize)]
struct ConnectorJwks {
    keys: Vec<ConnectorSigningKey>,
}

#[derive(Clone)]
/// The app, tenant, service hosts, and channels allowed to authenticate Teams Activities.
pub struct AdmissionPolicy {
    pub app_id: String,
    pub tenant_id: String,
    pub allowed_service_hosts: BTreeSet<String>,
    pub channels: BTreeMap<String, ChannelPolicy>,
}

#[derive(Clone)]
/// Whether a channel requires an endorsement on its connector signing key.
pub struct ChannelPolicy {
    pub requires_key_endorsement: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ConnectorClaims {
    iss: String,
    aud: String,
    exp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    nbf: Option<u64>,
    #[serde(rename = "serviceurl")]
    service_url: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Activity {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    service_url: String,
    channel_id: String,
    from: ChannelAccount,
    recipient: ChannelAccount,
    conversation: Conversation,
    channel_data: TeamsChannelData,
    text: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct ChannelAccount {
    id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Conversation {
    id: String,
}

#[derive(Clone, Debug, Deserialize)]
struct TeamsChannelData {
    tenant: Tenant,
}

#[derive(Clone, Debug, Deserialize)]
struct Tenant {
    id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuthenticatedConversationReference {
    pub(crate) service_url: Url,
    pub(crate) channel_id: String,
    pub(crate) conversation_id: String,
    pub(crate) incoming_activity_id: String,
    pub(crate) bot_id: String,
    pub(crate) sender_id: String,
    pub(crate) dione_authorized_tenant_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuthenticatedTeamsEnvelope {
    pub(crate) reference: AuthenticatedConversationReference,
    pub(crate) text: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReplyActivity {
    #[serde(rename = "type")]
    kind: &'static str,
    from: ChannelAccount,
    recipient: ChannelAccount,
    conversation: Conversation,
    reply_to_id: String,
    text: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CorrelatedReplyRequest {
    pub(crate) method: Method,
    pub(crate) url: Url,
    pub(crate) body: ReplyActivity,
}

#[derive(Debug, Error)]
/// A failed connector metadata, token, Activity, or reply-correlation check.
pub enum ProbeError {
    #[error("connector metadata JSON is invalid")]
    InvalidMetadata(#[source] serde_json::Error),
    #[error("authorization header is not a bearer token")]
    MissingBearer,
    #[error("token header is invalid")]
    InvalidHeader(#[source] jsonwebtoken::errors::Error),
    #[error("token algorithm is not advertised by connector metadata")]
    UnsupportedAlgorithm,
    #[error("token has no key id")]
    MissingKeyId,
    #[error("token key id is absent from connector metadata")]
    UnknownKeyId,
    #[error("activity channel is absent from Dione policy")]
    ChannelDenied,
    #[error("connector signing key is not endorsed for the activity channel")]
    ChannelNotEndorsed,
    #[error("connector signing key is invalid")]
    InvalidKey(#[source] jsonwebtoken::errors::Error),
    #[error("connector token is invalid")]
    InvalidToken(#[source] jsonwebtoken::errors::Error),
    #[error("connector token is expired")]
    Expired,
    #[error("connector token is not yet valid")]
    NotYetValid,
    #[error("activity JSON is invalid")]
    InvalidActivity(#[source] serde_json::Error),
    #[error("token and activity service URLs differ")]
    ServiceUrlMismatch,
    #[error("service URL is not valid HTTPS")]
    InvalidServiceUrl,
    #[error("service URL host is denied by Dione policy")]
    ServiceHostDenied,
    #[error("activity is not a Teams message")]
    UnsupportedActivity,
    #[error("activity tenant is denied by Dione policy")]
    TenantDenied,
    #[error("activity has no message text")]
    MissingText,
    #[error("reply URL cannot be constructed from the authenticated reference")]
    InvalidReplyUrl,
}

pub(crate) fn parse_connector_metadata(
    openid_document: &[u8],
    jwks_document: &[u8],
) -> Result<FrozenOpenId, ProbeError> {
    let mut metadata: FrozenOpenId =
        serde_json::from_slice(openid_document).map_err(ProbeError::InvalidMetadata)?;
    metadata.keys = serde_json::from_slice::<ConnectorJwks>(jwks_document)
        .map_err(ProbeError::InvalidMetadata)?
        .keys;
    Ok(metadata)
}

pub(crate) fn authenticate_activity(
    authorization: &str,
    raw_activity: &[u8],
    metadata: &FrozenOpenId,
    policy: &AdmissionPolicy,
    now: u64,
) -> Result<AuthenticatedTeamsEnvelope, ProbeError> {
    let token = authorization
        .strip_prefix("Bearer ")
        .filter(|token| !token.is_empty())
        .ok_or(ProbeError::MissingBearer)?;
    let header = decode_header(token).map_err(ProbeError::InvalidHeader)?;
    if !metadata.signing_algorithms.contains(&header.alg) {
        return Err(ProbeError::UnsupportedAlgorithm);
    }
    let key_id = header.kid.ok_or(ProbeError::MissingKeyId)?;
    let signing_key = metadata
        .keys
        .iter()
        .find(|key| key.jwk.common.key_id.as_deref() == Some(key_id.as_str()))
        .ok_or(ProbeError::UnknownKeyId)?;
    let decoding_key = DecodingKey::from_jwk(&signing_key.jwk).map_err(ProbeError::InvalidKey)?;

    let mut validation = Validation::new(header.alg);
    validation.set_required_spec_claims(&["exp", "iss", "aud"]);
    validation.set_issuer(&[metadata.issuer.as_str()]);
    validation.set_audience(&[policy.app_id.as_str()]);
    // The production seam needs an injectable clock. Keep signature,
    // issuer, and audience verification in the crate, then enforce time here.
    validation.validate_exp = false;
    validation.validate_nbf = false;
    let claims = decode::<ConnectorClaims>(token, &decoding_key, &validation)
        .map_err(ProbeError::InvalidToken)?
        .claims;
    if now > claims.exp.saturating_add(CLOCK_SKEW_SECONDS) {
        return Err(ProbeError::Expired);
    }
    if claims
        .nbf
        .is_some_and(|not_before| now.saturating_add(CLOCK_SKEW_SECONDS) < not_before)
    {
        return Err(ProbeError::NotYetValid);
    }

    let activity: Activity =
        serde_json::from_slice(raw_activity).map_err(ProbeError::InvalidActivity)?;
    if claims.service_url != activity.service_url {
        return Err(ProbeError::ServiceUrlMismatch);
    }
    if activity.kind != "message" {
        return Err(ProbeError::UnsupportedActivity);
    }
    let channel_policy = policy
        .channels
        .get(&activity.channel_id)
        .ok_or(ProbeError::ChannelDenied)?;
    if channel_policy.requires_key_endorsement
        && !signing_key.endorsements.contains(&activity.channel_id)
    {
        return Err(ProbeError::ChannelNotEndorsed);
    }
    if activity.channel_data.tenant.id != policy.tenant_id {
        return Err(ProbeError::TenantDenied);
    }

    let service_url =
        Url::parse(&activity.service_url).map_err(|_| ProbeError::InvalidServiceUrl)?;
    if service_url.scheme() != "https"
        || service_url.cannot_be_a_base()
        || !service_url.username().is_empty()
        || service_url.password().is_some()
        || service_url.query().is_some()
        || service_url.fragment().is_some()
        || service_url.port_or_known_default() != Some(443)
    {
        return Err(ProbeError::InvalidServiceUrl);
    }
    if service_url
        .host_str()
        .is_none_or(|host| !policy.allowed_service_hosts.contains(host))
    {
        return Err(ProbeError::ServiceHostDenied);
    }
    let text = activity
        .text
        .filter(|text| !text.is_empty())
        .ok_or(ProbeError::MissingText)?;

    Ok(AuthenticatedTeamsEnvelope {
        reference: AuthenticatedConversationReference {
            service_url,
            channel_id: activity.channel_id,
            conversation_id: activity.conversation.id,
            incoming_activity_id: activity.id,
            bot_id: activity.recipient.id,
            sender_id: activity.from.id,
            // The connector authentication makes the request admissible; this
            // tenant value comes from its body and is separately authorized by
            // Dione policy. It is not represented as a JWT claim.
            dione_authorized_tenant_id: activity.channel_data.tenant.id,
        },
        text,
    })
}

pub(crate) fn correlated_reply(
    envelope: &AuthenticatedTeamsEnvelope,
    text: impl Into<String>,
) -> Result<CorrelatedReplyRequest, ProbeError> {
    let reference = &envelope.reference;
    let mut url = reference.service_url.clone();
    url.path_segments_mut()
        .map_err(|_| ProbeError::InvalidReplyUrl)?
        .pop_if_empty()
        .extend([
            "v3",
            "conversations",
            reference.conversation_id.as_str(),
            "activities",
            reference.incoming_activity_id.as_str(),
        ]);
    Ok(CorrelatedReplyRequest {
        method: Method::POST,
        url,
        body: ReplyActivity {
            kind: "message",
            from: ChannelAccount {
                id: reference.bot_id.clone(),
            },
            recipient: ChannelAccount {
                id: reference.sender_id.clone(),
            },
            conversation: Conversation {
                id: reference.conversation_id.clone(),
            },
            reply_to_id: reference.incoming_activity_id.clone(),
            text: text.into(),
        },
    })
}

#[cfg(test)]
fn fixture_metadata() -> FrozenOpenId {
    parse_connector_metadata(OPENID_DOCUMENT, JWKS_DOCUMENT)
        .expect("frozen Connector metadata deserializes")
}

#[test]
fn connector_key_parser_preserves_jwk_and_optional_endorsements() {
    let endorsed = fixture_metadata();
    assert_eq!(endorsed.keys[0].jwk.common.key_id.as_deref(), Some(KEY_ID));
    assert!(endorsed.keys[0].endorsements.contains("msteams"));

    let mut jwks: Value = serde_json::from_slice(JWKS_DOCUMENT).expect("fixture JWKS parses");
    jwks["keys"][0]
        .as_object_mut()
        .expect("fixture key is an object")
        .remove("endorsements");
    let without_endorsements = parse_connector_metadata(
        OPENID_DOCUMENT,
        &serde_json::to_vec(&jwks).expect("fixture JWKS serializes"),
    )
    .expect("connector JWK without endorsements parses");
    assert_eq!(
        without_endorsements.keys[0].jwk.common.key_id.as_deref(),
        Some(KEY_ID)
    );
    assert!(without_endorsements.keys[0].endorsements.is_empty());
}

#[cfg(test)]
fn fixture_policy() -> AdmissionPolicy {
    AdmissionPolicy {
        app_id: APP_ID.to_owned(),
        tenant_id: TENANT_ID.to_owned(),
        // This is a Dione defense, not a Microsoft token-validation rule.
        allowed_service_hosts: BTreeSet::from(["smba.trafficmanager.net".to_owned()]),
        channels: BTreeMap::from([(
            "msteams".to_owned(),
            ChannelPolicy {
                requires_key_endorsement: true,
            },
        )]),
    }
}

#[cfg(test)]
fn fixture_claims() -> ConnectorClaims {
    ConnectorClaims {
        iss: CONNECTOR_ISSUER.to_owned(),
        aud: APP_ID.to_owned(),
        exp: NOW + 3_600,
        nbf: Some(NOW - 60),
        service_url: "https://smba.trafficmanager.net/amer/".to_owned(),
    }
}

#[cfg(test)]
fn fixture_activity() -> Value {
    json!({
        "type": "message",
        "id": "activity/with space",
        "serviceUrl": "https://smba.trafficmanager.net/amer/",
        "channelId": "msteams",
        "from": { "id": "teams-user-7" },
        "recipient": { "id": "teams-bot-9" },
        "conversation": { "id": "conversation/with space" },
        "channelData": { "tenant": { "id": TENANT_ID } },
        "text": "hello from Teams",
        "callerId": "ignored-not-authority"
    })
}

#[cfg(test)]
fn sign(claims: &ConnectorClaims, key_id: &str) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(key_id.to_owned());
    encode(
        &header,
        claims,
        &EncodingKey::from_rsa_pem(PRIVATE_KEY).expect("fixture key is valid"),
    )
    .expect("fixture token signs")
}

#[cfg(test)]
fn authenticate_fixture(
    claims: &ConnectorClaims,
    activity: &Value,
) -> Result<AuthenticatedTeamsEnvelope, ProbeError> {
    authenticate_activity(
        &format!("Bearer {}", sign(claims, KEY_ID)),
        &serde_json::to_vec(activity).expect("fixture activity serializes"),
        &fixture_metadata(),
        &fixture_policy(),
        NOW,
    )
}

#[test]
fn authenticated_activity_preserves_every_value_needed_for_a_correlated_reply() {
    let envelope = authenticate_fixture(&fixture_claims(), &fixture_activity())
        .expect("valid connector request authenticates");

    assert_eq!(envelope.text, "hello from Teams");
    assert_eq!(envelope.reference.channel_id, "msteams");
    assert_eq!(envelope.reference.dione_authorized_tenant_id, TENANT_ID);
    assert_eq!(envelope.reference.bot_id, "teams-bot-9");
    assert_eq!(envelope.reference.sender_id, "teams-user-7");

    let reply = correlated_reply(&envelope, "hello back").expect("reply route is constructible");
    assert_eq!(reply.method, Method::POST);
    assert_eq!(
        reply.url.as_str(),
        "https://smba.trafficmanager.net/amer/v3/conversations/conversation%2Fwith%20space/activities/activity%2Fwith%20space"
    );
    assert_eq!(reply.body.reply_to_id, "activity/with space");
    assert_eq!(reply.body.from.id, "teams-bot-9");
    assert_eq!(reply.body.recipient.id, "teams-user-7");
}

#[test]
fn issuer_audience_signature_and_key_id_are_enforced() {
    let activity = fixture_activity();

    let mut wrong_issuer = fixture_claims();
    wrong_issuer.iss = "https://issuer.example".to_owned();
    assert!(matches!(
        authenticate_fixture(&wrong_issuer, &activity),
        Err(ProbeError::InvalidToken(_))
    ));

    let mut wrong_audience = fixture_claims();
    wrong_audience.aud = "some-other-app".to_owned();
    assert!(matches!(
        authenticate_fixture(&wrong_audience, &activity),
        Err(ProbeError::InvalidToken(_))
    ));

    let unknown_key_token = sign(&fixture_claims(), "unknown-key");
    assert!(matches!(
        authenticate_activity(
            &format!("Bearer {unknown_key_token}"),
            &serde_json::to_vec(&activity).expect("fixture activity serializes"),
            &fixture_metadata(),
            &fixture_policy(),
            NOW,
        ),
        Err(ProbeError::UnknownKeyId)
    ));

    let mut tampered = sign(&fixture_claims(), KEY_ID).into_bytes();
    let last = tampered.last_mut().expect("fixture token is nonempty");
    *last = if *last == b'A' { b'B' } else { b'A' };
    assert!(matches!(
        authenticate_activity(
            &format!(
                "Bearer {}",
                String::from_utf8(tampered).expect("JWT remains ASCII")
            ),
            &serde_json::to_vec(&activity).expect("fixture activity serializes"),
            &fixture_metadata(),
            &fixture_policy(),
            NOW,
        ),
        Err(ProbeError::InvalidToken(_))
    ));

    let mut hmac_header = Header::new(Algorithm::HS256);
    hmac_header.kid = Some(KEY_ID.to_owned());
    let hmac_token = encode(
        &hmac_header,
        &fixture_claims(),
        &EncodingKey::from_secret(b"not-a-connector-key"),
    )
    .expect("fixture HMAC token signs");
    assert!(matches!(
        authenticate_activity(
            &format!("Bearer {hmac_token}"),
            &serde_json::to_vec(&activity).expect("fixture activity serializes"),
            &fixture_metadata(),
            &fixture_policy(),
            NOW,
        ),
        Err(ProbeError::UnsupportedAlgorithm)
    ));
}

#[test]
fn injected_clock_enforces_the_documented_five_minute_skew() {
    let activity = fixture_activity();
    let mut claims = fixture_claims();
    claims.exp = NOW - CLOCK_SKEW_SECONDS;
    assert!(authenticate_fixture(&claims, &activity).is_ok());

    claims.exp -= 1;
    assert!(matches!(
        authenticate_fixture(&claims, &activity),
        Err(ProbeError::Expired)
    ));

    claims = fixture_claims();
    claims.nbf = Some(NOW + CLOCK_SKEW_SECONDS + 1);
    assert!(matches!(
        authenticate_fixture(&claims, &activity),
        Err(ProbeError::NotYetValid)
    ));
}

#[test]
fn service_url_must_be_token_bound_and_allowed_by_dione_policy() {
    let mut activity = fixture_activity();
    activity["serviceUrl"] = json!("https://attacker.example/amer/");
    assert!(matches!(
        authenticate_fixture(&fixture_claims(), &activity),
        Err(ProbeError::ServiceUrlMismatch)
    ));

    let mut claims = fixture_claims();
    claims.service_url = "https://signed-but-not-allowed.example/".to_owned();
    activity["serviceUrl"] = json!(claims.service_url);
    assert!(matches!(
        authenticate_fixture(&claims, &activity),
        Err(ProbeError::ServiceHostDenied)
    ));

    for unsafe_suffix in ["?redirect=elsewhere", "#fragment"] {
        let mut claims = fixture_claims();
        claims.service_url.push_str(unsafe_suffix);
        activity["serviceUrl"] = json!(claims.service_url);
        assert!(matches!(
            authenticate_fixture(&claims, &activity),
            Err(ProbeError::InvalidServiceUrl)
        ));
    }
}

#[test]
fn tenant_and_activity_kind_are_dione_admission_checks() {
    let mut activity = fixture_activity();
    activity["channelData"]["tenant"]["id"] = json!("wrong-tenant");
    assert!(matches!(
        authenticate_fixture(&fixture_claims(), &activity),
        Err(ProbeError::TenantDenied)
    ));

    activity = fixture_activity();
    activity["type"] = json!("invoke");
    assert!(matches!(
        authenticate_fixture(&fixture_claims(), &activity),
        Err(ProbeError::UnsupportedActivity)
    ));
}

#[test]
fn key_endorsement_is_required_only_when_channel_policy_requires_it() {
    let activity = fixture_activity();
    let claims = fixture_claims();
    let token = sign(&claims, KEY_ID);
    let raw_activity = serde_json::to_vec(&activity).expect("fixture activity serializes");
    let mut metadata = fixture_metadata();
    metadata.keys[0].endorsements.clear();

    assert!(matches!(
        authenticate_activity(
            &format!("Bearer {token}"),
            &raw_activity,
            &metadata,
            &fixture_policy(),
            NOW,
        ),
        Err(ProbeError::ChannelNotEndorsed)
    ));

    let mut policy = fixture_policy();
    policy
        .channels
        .get_mut("msteams")
        .expect("fixture channel exists")
        .requires_key_endorsement = false;
    assert!(
        authenticate_activity(
            &format!("Bearer {token}"),
            &raw_activity,
            &metadata,
            &policy,
            NOW,
        )
        .is_ok()
    );
}
