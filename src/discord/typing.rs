//! Discord typing notifications with live channel authorization.

use crate::{config::LoadedConfig, gate::OutboundGate, state::State};
use serenity::{http::Http, model::id::ChannelId};
use std::{future::Future, sync::Arc};
use thiserror::Error;

/// Dependencies for an authorized typing notification.
pub struct TypingCtx {
    http: Arc<Http>,
    state: State,
    config: Arc<LoadedConfig>,
}
impl TypingCtx {
    /// Supplies the Discord connection and current authorization state.
    pub fn new(http: Arc<Http>, state: State, config: Arc<LoadedConfig>) -> Self {
        Self {
            http,
            state,
            config,
        }
    }
}
/// A Discord typing transport failure with its original cause.
#[derive(Debug, Error)]
#[error("{source}")]
pub struct TypingFailure {
    #[source]
    source: Box<serenity::Error>,
}
/// Failure to authorize or send a typing notification.
#[derive(Debug, Error)]
pub enum TypingError {
    /// Current channel policy rejects this destination.
    #[error("channel not in allowlist")]
    Denied,
    /// Discord rejected the typing request or could not be reached.
    #[error(transparent)]
    Transport(TypingFailure),
}
trait TypingTransport: Sync {
    fn broadcast_typing(
        &self,
        channel: ChannelId,
    ) -> impl Future<Output = Result<(), serenity::Error>> + Send;
}
impl TypingTransport for Http {
    async fn broadcast_typing(&self, channel: ChannelId) -> Result<(), serenity::Error> {
        Http::broadcast_typing(self, channel).await
    }
}
/// Sends typing after checking current DM and thread authorization.
pub async fn send_typing(ctx: &TypingCtx, channel: ChannelId) -> Result<(), TypingError> {
    send_authorized_typing(ctx.http.as_ref(), &ctx.state, &ctx.config, channel).await
}
async fn send_authorized_typing(
    transport: &impl TypingTransport,
    state: &State,
    config: &LoadedConfig,
    channel: ChannelId,
) -> Result<(), TypingError> {
    let allowed = {
        let state = state.read().await;
        OutboundGate::check_channel_with_threads(
            config,
            channel.get(),
            &state.dm_channel_ids,
            &state.thread_parents,
        )
    };
    if !allowed {
        return Err(TypingError::Denied);
    }
    transport.broadcast_typing(channel).await.map_err(|source| {
        TypingError::Transport(TypingFailure {
            source: Box::new(source),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ChannelConfig, Config},
        state::new_state,
    };
    use std::{error::Error as _, sync::Mutex};

    struct FakeTyping {
        calls: Mutex<Vec<ChannelId>>,
        fail: bool,
    }
    impl TypingTransport for FakeTyping {
        async fn broadcast_typing(&self, channel: ChannelId) -> Result<(), serenity::Error> {
            self.calls.lock().unwrap().push(channel);
            if self.fail {
                Err(serenity::Error::Other("typing unavailable"))
            } else {
                Ok(())
            }
        }
    }
    #[tokio::test]
    async fn typing_checks_live_dm_and_thread_authorization() {
        let state = new_state();
        let config = LoadedConfig::from_raw(Config {
            channels: vec![ChannelConfig {
                id: "500".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let transport = FakeTyping {
            calls: Mutex::new(Vec::new()),
            fail: false,
        };
        assert!(matches!(
            send_authorized_typing(&transport, &state, &config, ChannelId::new(999)).await,
            Err(TypingError::Denied)
        ));
        assert!(transport.calls.lock().unwrap().is_empty());
        send_authorized_typing(&transport, &state, &config, ChannelId::new(500))
            .await
            .unwrap();
        {
            let mut state = state.write().await;
            state.dm_channel_ids.insert(600);
            state.thread_parents.insert(700, Some(500));
        }
        for channel in [600, 700] {
            send_authorized_typing(&transport, &state, &config, ChannelId::new(channel))
                .await
                .unwrap();
        }
        {
            let mut state = state.write().await;
            state.dm_channel_ids.remove(&600);
            state.thread_parents.insert(700, Some(999));
        }
        for channel in [600, 700] {
            assert!(matches!(
                send_authorized_typing(&transport, &state, &config, ChannelId::new(channel)).await,
                Err(TypingError::Denied)
            ));
        }
        assert_eq!(
            *transport.calls.lock().unwrap(),
            [
                ChannelId::new(500),
                ChannelId::new(600),
                ChannelId::new(700)
            ]
        );
    }
    #[tokio::test]
    async fn typing_failure_retains_sdk_source_and_wire_text() {
        let state = new_state();
        state.write().await.dm_channel_ids.insert(600);
        let config = LoadedConfig::from_raw(Config::default());
        let transport = FakeTyping {
            calls: Mutex::new(Vec::new()),
            fail: true,
        };
        let error = send_authorized_typing(&transport, &state, &config, ChannelId::new(600))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "typing unavailable");
        let source = error
            .source()
            .unwrap()
            .downcast_ref::<Box<serenity::Error>>()
            .expect("the original boxed SDK cause");
        assert!(matches!(
            source.as_ref(),
            serenity::Error::Other("typing unavailable")
        ));
        assert_eq!(*transport.calls.lock().unwrap(), [ChannelId::new(600)]);
    }
}
