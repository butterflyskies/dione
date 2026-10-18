//! Authorized rendering and delivery of a math attachment.

use crate::{
    discord::messaging::{self, MessagingCtx, MessagingError, OutboundSurface},
    pre_send::HookName,
    rendering::{self, RenderError},
};
use serenity::{
    builder::CreateAttachment,
    model::id::{ChannelId, MessageId},
};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("failed to render to channel {channel_id}: {kind}")]
pub(crate) struct RenderToChannelError {
    pub(crate) channel_id: ChannelId,
    #[source]
    pub(crate) kind: RenderToChannelErrorKind,
}

#[derive(Debug, Error)]
pub(crate) enum RenderToChannelErrorKind {
    #[error(transparent)]
    Render(RenderError),
    #[error(transparent)]
    Messaging(Box<MessagingError>),
}

pub(crate) async fn render_to_channel(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    latex: &str,
    caption: Option<&str>,
    no_rly_hooks: &[HookName],
) -> Result<MessageId, RenderToChannelError> {
    let messaging_error = |source| RenderToChannelError {
        channel_id,
        kind: RenderToChannelErrorKind::Messaging(Box::new(source)),
    };
    if let Some(error) = messaging::reject_captionless_hook_overrides(caption, no_rly_hooks) {
        return Err(messaging_error(error));
    }
    messaging::check_outbound(ctx, channel_id)
        .await
        .map_err(messaging_error)?;
    let png = rendering::render_png(latex).map_err(|source| RenderToChannelError {
        channel_id,
        kind: RenderToChannelErrorKind::Render(source),
    })?;
    let attachment = CreateAttachment::bytes(png.as_bytes(), "math.png");
    messaging::send_attachment_with_hook_overrides(
        ctx,
        channel_id,
        attachment,
        caption,
        no_rly_hooks,
        OutboundSurface::RenderLatexCaption,
    )
    .await
    .map_err(messaging_error)
}
