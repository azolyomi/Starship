//! Temp voice-channel lifecycle for runs with `requires_vc = true`.
//!
//! Phase 6 scope is deliberately narrow: create a VC when the run starts,
//! delete it when the run ends. No join enforcement, no membership
//! tracking, no songbird — just REST channel CRUD.
//!
//! The VC is placed in the same category as the tier's runs text channel
//! so it lives next to the raid message. If the runs channel has no
//! parent category the VC is created at the guild root.

use anyhow::Result;
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, ChannelType, CreateChannel, GuildId, Http};
use tracing::warn;

/// Create a temp voice channel for a run in the same category as the runs
/// text channel. Returns the new channel's id on success.
///
/// Callers should log-and-continue on error — a failed VC create is worse
/// than no VC at all, but it shouldn't nuke the whole raid post.
pub async fn create_temp_vc(
    ctx: &serenity::Context,
    guild_id: GuildId,
    runs_channel_id: ChannelId,
    name: &str,
) -> Result<ChannelId> {
    let parent_id = runs_channel_id
        .to_channel(ctx)
        .await
        .ok()
        .and_then(|c| c.guild())
        .and_then(|gc| gc.parent_id);

    let mut builder = CreateChannel::new(name).kind(ChannelType::Voice);
    if let Some(parent) = parent_id {
        builder = builder.category(parent);
    }

    let channel = guild_id.create_channel(&ctx.http, builder).await?;
    Ok(channel.id)
}

/// Best-effort deletion. Logs but never errors: once the run is ended,
/// we don't care whether the channel was already gone, deleted manually,
/// or we lost the Manage Channels permission.
pub async fn delete_temp_vc(http: &Http, channel_id: ChannelId) {
    if let Err(e) = channel_id.delete(http).await {
        warn!(
            channel_id = %channel_id,
            error = ?e,
            "failed to delete temp VC",
        );
    }
}
