//! Runtime adapters for the linear verified-action proof core.
//!
//! Network clients and shared state live here so compile-fail tests can exercise
//! the proof API itself without replacing production dependencies with stubs.

use std::{collections::HashSet, num::NonZeroU64, time::Duration};

use chrono::Utc;
use serenity::{
    http::Http,
    model::{channel::Message, event::MessageUpdateEvent},
};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use super::verified_action::{
    DiscordTransportVerifier, EventBinding, EventContext, FreshPolicySnapshot, PrincipalResolver,
    RepresentedPrincipal, ResolutionFailure, ResolutionFailureClass, TransportProvider, Unresolved,
    VerifiedAppAction,
};
use crate::{
    pluralkit::{PkResolveError, PkResolver, VerifiedPkFacts},
    state::{State, observe_webhook_creator},
};

const DISCORD_VERIFICATION_DEADLINE: Duration = Duration::from_secs(5);

/// One complete, internally cross-checked gateway update binding.
pub(crate) struct VerifiedUpdateCandidate {
    binding: EventBinding,
}

impl VerifiedUpdateCandidate {
    /// Derives a candidate only when every supplied gateway representation agrees.
    pub(crate) fn from_gateway(
        event: &MessageUpdateEvent,
        old: Option<&Message>,
        new: Option<&Message>,
    ) -> Result<Option<Self>, &'static str> {
        for message in [old, new].into_iter().flatten() {
            if message.id != event.id || message.channel_id != event.channel_id {
                return Err("message coordinates disagree");
            }
            if message.guild_id != event.guild_id {
                return Err("message context disagrees");
            }
            if let Some(event_author) = event.author.as_ref()
                && event_author.id != message.author.id
            {
                return Err("message author disagrees");
            }
        }

        if let (Some(event_content), Some(new_message)) = (event.content.as_ref(), new)
            && event_content != &new_message.content
        {
            return Err("message content disagrees");
        }
        if let (Some(old_message), Some(new_message)) = (old, new)
            && old_message.author.id != new_message.author.id
        {
            return Err("message author disagrees");
        }
        if let (Some(old_message), Some(new_message)) = (old, new)
            && old_message.webhook_id != new_message.webhook_id
        {
            return Err("webhook presence disagrees");
        }
        if let Some(event_webhook) = event.webhook_id {
            for message in [old, new].into_iter().flatten() {
                if message.webhook_id != event_webhook {
                    return Err("webhook presence disagrees");
                }
            }
        }

        let event_webhook = event.webhook_id.flatten();
        let mut webhook_id = event_webhook;
        for supplied in [
            old.and_then(|m| m.webhook_id),
            new.and_then(|m| m.webhook_id),
        ] {
            if let (Some(left), Some(right)) = (webhook_id, supplied)
                && left != right
            {
                return Err("webhook binding disagrees");
            }
            webhook_id = webhook_id.or(supplied);
        }
        if event.webhook_id == Some(None) && webhook_id.is_some() {
            return Err("webhook presence disagrees");
        }
        let Some(webhook_id) = webhook_id else {
            return Ok(None);
        };

        let context = event
            .guild_id
            .map_or(EventContext::DirectMessage, EventContext::Guild);
        let Some(binding) =
            EventBinding::from_verified_update(event.id, event.channel_id, context, webhook_id)
        else {
            return Err("zero update binding coordinate");
        };
        Ok(Some(Self { binding }))
    }
}

impl DiscordTransportVerifier {
    fn verify_observed_create(
        &self,
        event: Message,
        creator_user_id: Option<u64>,
    ) -> Option<VerifiedAppAction<Unresolved>> {
        let provider = TransportProvider::from_creator_user_id(creator_user_id)?;
        self.mint_verified(event, provider)
    }

    fn verify_observed_update(
        &self,
        candidate: VerifiedUpdateCandidate,
        creator_user_id: Option<u64>,
    ) -> Option<VerifiedAppAction<Unresolved>> {
        let provider = TransportProvider::from_creator_user_id(creator_user_id)?;
        Some(self.mint_verified_update(candidate.binding, provider))
    }

    /// Verifies one Discord webhook event and binds proof to its exact coordinates.
    ///
    /// The creator cache stores only positive Discord facts. Provider
    /// classification is repeated for each event so cached observations cannot
    /// become durable authorization verdicts.
    pub(crate) async fn verify(
        &self,
        http: &Http,
        state: &State,
        event: Message,
    ) -> Option<VerifiedAppAction<Unresolved>> {
        tokio::time::timeout(
            DISCORD_VERIFICATION_DEADLINE,
            self.verify_within_deadline(http, state, event),
        )
        .await
        .ok()
        .flatten()
    }

    async fn verify_within_deadline(
        &self,
        http: &Http,
        state: &State,
        event: Message,
    ) -> Option<VerifiedAppAction<Unresolved>> {
        let webhook_id = event.webhook_id?;
        if event.id.get() == 0
            || event.channel_id.get() == 0
            || webhook_id.get() == 0
            || event.guild_id.is_some_and(|guild_id| guild_id.get() == 0)
        {
            return None;
        }

        let creator_user_id = observe_webhook_creator(http, state, webhook_id).await?;

        self.verify_observed_create(event, Some(creator_user_id))
    }

    /// Verifies a complete cross-checked gateway update candidate.
    pub(crate) async fn verify_update(
        &self,
        http: &Http,
        state: &State,
        candidate: VerifiedUpdateCandidate,
    ) -> Option<VerifiedAppAction<Unresolved>> {
        tokio::time::timeout(DISCORD_VERIFICATION_DEADLINE, async {
            let creator_user_id =
                observe_webhook_creator(http, state, candidate.binding.webhook_id()).await?;
            self.verify_observed_update(candidate, Some(creator_user_id))
        })
        .await
        .ok()
        .flatten()
    }
}

/// Builds the gate input from one already-selected policy in one loaded snapshot.
pub(crate) fn fresh_policy_snapshot(
    config: &crate::config::LoadedConfig,
    gate_channel_id: u64,
) -> Option<FreshPolicySnapshot> {
    let policy = config.channel_policy(gate_channel_id)?;
    let allowed_discord_users = policy
        .allow_from
        .iter()
        .filter_map(|id| NonZeroU64::new(*id).map(serenity::model::id::UserId::from))
        .collect::<HashSet<_>>();
    let allowed_systems = policy
        .allow_pk_systems
        .iter()
        .filter_map(|id| Uuid::parse_str(id).ok())
        .collect::<HashSet<_>>();
    let allowed_members = policy
        .allow_pk_members
        .iter()
        .filter_map(|id| Uuid::parse_str(id).ok())
        .collect::<HashSet<_>>();
    let restriction_intended = policy.has_identity_filter()
        || policy.allow_from.iter().any(|id| *id == 0)
        || policy.raw_had_identity_entries();
    let fingerprint = policy_fingerprint(
        gate_channel_id,
        restriction_intended,
        &allowed_discord_users,
        &allowed_systems,
        &allowed_members,
    );

    Some(FreshPolicySnapshot::from_parts(
        restriction_intended,
        allowed_discord_users,
        allowed_systems,
        allowed_members,
        config.generation(),
        fingerprint,
        Utc::now(),
    ))
}

fn policy_fingerprint(
    gate_channel_id: u64,
    restriction_intended: bool,
    users: &HashSet<serenity::model::id::UserId>,
    systems: &HashSet<Uuid>,
    members: &HashSet<Uuid>,
) -> [u8; 32] {
    let mut users = users.iter().map(|id| id.get()).collect::<Vec<_>>();
    users.sort_unstable();
    let mut systems = systems
        .iter()
        .map(Uuid::as_bytes)
        .copied()
        .collect::<Vec<_>>();
    systems.sort_unstable();
    let mut members = members
        .iter()
        .map(Uuid::as_bytes)
        .copied()
        .collect::<Vec<_>>();
    members.sort_unstable();

    let mut digest = Sha256::new();
    digest.update(b"dione-verified-action-policy-v1\0");
    digest.update(gate_channel_id.to_be_bytes());
    digest.update([u8::from(restriction_intended)]);
    for user in users {
        digest.update(b"u");
        digest.update(user.to_be_bytes());
    }
    for system in systems {
        digest.update(b"s");
        digest.update(system);
    }
    for member in members {
        digest.update(b"m");
        digest.update(member);
    }
    digest.finalize().into()
}

/// Runtime owner of the fixed PluralKit fact source and typestate transition.
pub(crate) struct BoundPrincipalResolver<'a> {
    transition: PrincipalResolver,
    pluralkit: &'a PkResolver,
}

impl<'a> BoundPrincipalResolver<'a> {
    /// Binds the transition authority to one configured fact source.
    pub(crate) fn new(pluralkit: &'a PkResolver) -> Self {
        Self {
            transition: PrincipalResolver::new(),
            pluralkit,
        }
    }

    /// Consumes one verified action and returns exactly one sealed outcome.
    pub(crate) async fn resolve(
        &self,
        action: VerifiedAppAction<Unresolved>,
    ) -> super::verified_action::VerifiedAppAction<super::verified_action::Resolved> {
        let session = self.transition.begin(action);
        let facts = match session.provider() {
            TransportProvider::PluralKit => {
                self.pluralkit
                    .resolve_verified_facts(session.binding())
                    .await
            }
        };

        resolve_from_facts(&self.transition, session, facts)
    }
}

fn resolve_from_facts(
    transition: &PrincipalResolver,
    session: super::verified_action::ResolutionSession,
    facts: Result<VerifiedPkFacts, PkResolveError>,
) -> super::verified_action::VerifiedAppAction<super::verified_action::Resolved> {
    match facts {
        Ok(VerifiedPkFacts::AppOnly) => transition.finish_app_only(session),
        Ok(VerifiedPkFacts::Represented {
            discord_user_id,
            system_id,
            member_id,
        }) => transition.finish_represented(
            session,
            RepresentedPrincipal::from_verified_facts(discord_user_id, system_id, member_id),
        ),
        Err(error) => transition.finish_unavailable(
            session,
            ResolutionFailure::from_class(classify_pk_error(error)),
        ),
    }
}

#[cfg(test)]
pub(super) async fn resolve_test_create_with_sources<CF, FF>(
    event: Message,
    creator: CF,
    facts: FF,
) -> Option<super::verified_action::VerifiedAppAction<super::verified_action::Resolved>>
where
    CF: std::future::Future<Output = Option<u64>>,
    FF: std::future::Future<Output = Result<VerifiedPkFacts, PkResolveError>>,
{
    let verifier = DiscordTransportVerifier::new();
    let action = verifier.verify_observed_create(event, creator.await)?;
    let transition = PrincipalResolver::new();
    let session = transition.begin(action);
    Some(resolve_from_facts(&transition, session, facts.await))
}

#[cfg(test)]
pub(super) async fn resolve_test_update_with_sources<CF, FF>(
    candidate: VerifiedUpdateCandidate,
    creator: CF,
    facts: FF,
) -> Option<super::verified_action::VerifiedAppAction<super::verified_action::Resolved>>
where
    CF: std::future::Future<Output = Option<u64>>,
    FF: std::future::Future<Output = Result<VerifiedPkFacts, PkResolveError>>,
{
    let verifier = DiscordTransportVerifier::new();
    let action = verifier.verify_observed_update(candidate, creator.await)?;
    let transition = PrincipalResolver::new();
    let session = transition.begin(action);
    Some(resolve_from_facts(&transition, session, facts.await))
}

fn classify_pk_error(error: PkResolveError) -> ResolutionFailureClass {
    match error {
        PkResolveError::Timeout => ResolutionFailureClass::Deadline,
        PkResolveError::HttpError(_) => ResolutionFailureClass::Network,
        PkResolveError::InvalidResponse(_) | PkResolveError::BindingMismatch { .. } => {
            ResolutionFailureClass::InvalidResponse
        }
        PkResolveError::NotFound
        | PkResolveError::RateLimited
        | PkResolveError::SemaphoreClosed => ResolutionFailureClass::Provider,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn message(
        id: u64,
        channel_id: u64,
        guild_id: Option<u64>,
        webhook_id: Option<u64>,
    ) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": id.to_string(),
            "channel_id": channel_id.to_string(),
            "guild_id": guild_id.map(|id| id.to_string()),
            "author": {"id":"7","username":"proxy","discriminator":"0","bot":false},
            "content": "edited",
            "timestamp": "2026-08-16T00:00:00Z",
            "edited_timestamp": "2026-08-16T00:00:01Z",
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": false,
            "type": 0,
            "webhook_id": webhook_id.map(|id| id.to_string())
        }))
        .expect("valid message")
    }

    fn update(guild_id: Option<u64>, webhook: Option<serde_json::Value>) -> MessageUpdateEvent {
        let mut value = serde_json::json!({
            "id":"10",
            "channel_id":"20",
            "guild_id":guild_id.map(|id| id.to_string()),
            "author":{"id":"7","username":"proxy","discriminator":"0","bot":false},
            "content":"edited",
            "edited_timestamp":"2026-08-16T00:00:01Z"
        });
        if let Some(webhook) = webhook {
            value["webhook_id"] = webhook;
        }
        serde_json::from_value(value).expect("valid update")
    }

    #[test]
    fn provider_failures_never_become_app_only() {
        assert_eq!(
            classify_pk_error(PkResolveError::NotFound),
            ResolutionFailureClass::Provider
        );
        assert_eq!(
            classify_pk_error(PkResolveError::Timeout),
            ResolutionFailureClass::Deadline
        );
        assert_eq!(
            classify_pk_error(PkResolveError::InvalidResponse("bad".to_string())),
            ResolutionFailureClass::InvalidResponse
        );
    }

    #[test]
    fn raw_update_can_form_complete_candidate_without_new_message() {
        let event = update(None, Some(serde_json::json!("40")));
        assert!(
            VerifiedUpdateCandidate::from_gateway(&event, None, None)
                .expect("consistent")
                .is_some()
        );
    }

    #[test]
    fn webhook_transport_candidate_does_not_trust_author_bot_flag() {
        let event = message(10, 20, Some(30), Some(40));
        assert!(!event.author.bot);
        let action = DiscordTransportVerifier::new()
            .mint_verified(event, TransportProvider::PluralKit)
            .expect("webhook binding mints independently of author.bot");
        assert_eq!(action.binding().webhook_id().get(), 40);
    }

    #[test]
    fn missing_webhook_stays_passive_and_context_conflicts_fail_closed() {
        let event = update(None, None);
        assert!(
            VerifiedUpdateCandidate::from_gateway(&event, None, None)
                .expect("consistent")
                .is_none()
        );
        let guild_message = message(10, 20, Some(30), Some(40));
        assert!(VerifiedUpdateCandidate::from_gateway(&event, None, Some(&guild_message)).is_err());
    }

    #[test]
    fn full_old_new_webhook_presence_disagreement_fails_closed() {
        let event = update(Some(30), None);
        let old = message(10, 20, Some(30), Some(40));
        let new = message(10, 20, Some(30), None);
        assert!(VerifiedUpdateCandidate::from_gateway(&event, Some(&old), Some(&new)).is_err());

        let event = update(Some(30), Some(serde_json::json!("40")));
        assert!(VerifiedUpdateCandidate::from_gateway(&event, None, Some(&new)).is_err());
    }

    #[test]
    fn omitted_event_author_does_not_hide_old_new_author_conflict() {
        let mut event = update(Some(30), None);
        event.author = None;
        let old = message(10, 20, Some(30), Some(40));
        let mut new = message(10, 20, Some(30), Some(40));
        new.author.id = serenity::model::id::UserId::new(8);
        assert!(VerifiedUpdateCandidate::from_gateway(&event, Some(&old), Some(&new)).is_err());
    }

    fn config_with_channel(channel: crate::config::ChannelConfig) -> crate::config::LoadedConfig {
        let mut raw = crate::config::Config::default();
        raw.channels.push(channel);
        crate::config::LoadedConfig::from_raw(raw)
    }

    #[test]
    fn malformed_and_zero_user_selectors_preserve_fail_closed_intent() {
        for selector in ["not-a-snowflake", "0"] {
            let config = config_with_channel(crate::config::ChannelConfig {
                id: "20".into(),
                require_mention: false,
                allow_from: vec![selector.into()],
                ..Default::default()
            });
            let snapshot = fresh_policy_snapshot(&config, 20).expect("configured channel");
            assert!(snapshot.restriction_intended());
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn arbitrary_malformed_identity_selectors_deny_unresolved_classes(
            invalid_users in proptest::collection::vec(".{0,24}", 1..6),
            invalid_systems in proptest::collection::vec(".{0,24}", 1..6),
            invalid_members in proptest::collection::vec(".{0,24}", 1..6),
        ) {
            let channels = [
                crate::config::ChannelConfig {
                    id: "20".into(),
                    allow_from: invalid_users
                        .into_iter()
                        .map(|value| format!("x{value}"))
                        .collect(),
                    ..Default::default()
                },
                crate::config::ChannelConfig {
                    id: "20".into(),
                    allow_pk_systems: invalid_systems
                        .into_iter()
                        .map(|value| format!("not-uuid-{value}"))
                        .collect(),
                    ..Default::default()
                },
                crate::config::ChannelConfig {
                    id: "20".into(),
                    allow_pk_members: invalid_members
                        .into_iter()
                        .map(|value| format!("not-uuid-{value}"))
                        .collect(),
                    ..Default::default()
                },
            ];
            for channel in channels {
                let config = config_with_channel(channel);
                let app_policy = fresh_policy_snapshot(&config, 20).expect("configured channel");
                prop_assert!(app_policy.restriction_intended());
                let verifier = DiscordTransportVerifier::new();
                let resolver = PrincipalResolver::new();
                let app_action = verifier
                    .mint_verified(message(10, 20, Some(30), Some(40)), TransportProvider::PluralKit)
                    .expect("verified webhook action");
                let app_action = resolver.finish_app_only(resolver.begin(app_action));
                prop_assert!(matches!(
                    super::super::verified_action::VerifiedActionGate::evaluate(
                        app_action,
                        app_policy,
                    ),
                    super::super::verified_action::VerifiedGateVerdict::Deny
                ));

                let unavailable_policy =
                    fresh_policy_snapshot(&config, 20).expect("configured channel");
                let unavailable_action = verifier
                    .mint_verified(message(11, 20, Some(30), Some(40)), TransportProvider::PluralKit)
                    .expect("verified webhook action");
                let unavailable_action = resolver.finish_unavailable(
                    resolver.begin(unavailable_action),
                    ResolutionFailure::from_class(ResolutionFailureClass::Network),
                );
                prop_assert!(matches!(
                    super::super::verified_action::VerifiedActionGate::evaluate(
                        unavailable_action,
                        unavailable_policy,
                    ),
                    super::super::verified_action::VerifiedGateVerdict::Deny
                ));
            }
        }
    }

    #[test]
    fn fingerprint_is_stable_across_selector_order_but_generation_advances() {
        let channel = |users: &[&str]| crate::config::ChannelConfig {
            id: "20".into(),
            require_mention: false,
            allow_from: users.iter().map(|id| (*id).to_owned()).collect(),
            ..Default::default()
        };
        let first_config = config_with_channel(channel(&["8", "7"]));
        let second_config = config_with_channel(channel(&["7", "8"]));
        let first = fresh_policy_snapshot(&first_config, 20).expect("policy");
        let second = fresh_policy_snapshot(&second_config, 20).expect("policy");
        assert_eq!(first.fingerprint_bytes(), second.fingerprint_bytes());
        assert!(second_config.generation() > first_config.generation());
    }
}
