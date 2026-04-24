// Interactive curator: walk each scraped dungeon, pick which reactions + drops
// to keep, write the selections to `data/curation.json`, then apply cleanup
// (delete removed Discord emojis + rows from `bot_emoji` / `dungeon_reactions`).
//
// Subsequent `sync-wiki` runs consult `curation.json` so removed entries don't
// come back. New dungeons or new drops that appear on RealmEye land in the
// snapshot and get surfaced on the next `curate` run.
//
// Run as:
//   starship curate              # prompt only uncurated dungeons
//   starship curate --recurate   # prompt every dungeon, pre-checked with current
//   starship curate --dry-run    # walk prompts + preview JSON, no writes/deletes

use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, MultiSelect};
use reqwest::Client;
use sqlx::PgPool;
use tracing::{info, warn};

use crate::config::Config;
use crate::curation::{Curation, DungeonCuration, WikiSnapshot};
use crate::db;
use crate::db::emoji::ApplicationEmojiClient;

pub async fn run(recurate: bool, dry_run: bool) -> Result<()> {
    let config = Config::from_env()?;

    let snapshot = WikiSnapshot::load()?;
    if snapshot.dungeons.is_empty() {
        bail!(
            "no wiki snapshot at data/wiki-snapshot.json — run `starship sync-wiki` \
             first to populate dungeons and drops"
        );
    }
    info!("loaded snapshot: {} dungeons", snapshot.dungeons.len());

    let mut curation = Curation::load()?;
    let pool = db::create_pool(&config.database_url).await?;

    let to_prompt: Vec<_> = snapshot
        .dungeons
        .iter()
        .enumerate()
        .filter(|(_, d)| recurate || !curation.is_curated(&d.name))
        .collect();

    if to_prompt.is_empty() {
        info!(
            "nothing to curate — {} dungeons already curated. Use `--recurate` to re-prompt.",
            snapshot.dungeons.len()
        );
    } else {
        info!(
            "{} dungeons to prompt ({} already curated will be left alone)",
            to_prompt.len(),
            snapshot.dungeons.len() - to_prompt.len()
        );
    }

    let theme = ColorfulTheme::default();
    let total = snapshot.dungeons.len();

    for (idx, dungeon) in to_prompt {
        let reactions = load_current_reactions(&pool, &dungeon.name).await?;
        let drops = &dungeon.drops;

        // Nothing to decide for a bare dungeon — auto-mark as curated with
        // empty allowlists so --recurate doesn't keep showing it.
        if reactions.is_empty() && drops.is_empty() {
            curation.dungeons.insert(
                dungeon.name.clone(),
                DungeonCuration {
                    reactions: vec![],
                    drops: vec![],
                },
            );
            continue;
        }

        println!(
            "\n── [{}/{}] {} ──────────────────────────",
            idx + 1,
            total,
            dungeon.display_name
        );

        let prior = curation.dungeons.get(&dungeon.name).cloned();

        // Reactions prompt.
        let new_reactions = if reactions.is_empty() {
            vec![]
        } else {
            let labels: Vec<String> = reactions
                .iter()
                .map(|r| format!("{} — {}", r.display_name, r.name))
                .collect();
            let defaults: Vec<bool> = reactions
                .iter()
                .map(|r| match &prior {
                    Some(p) => p.reactions.iter().any(|n| n == &r.name),
                    None => true,
                })
                .collect();
            let picks = MultiSelect::with_theme(&theme)
                .with_prompt("reactions to keep (space toggles, enter confirms)")
                .items(&labels)
                .defaults(&defaults)
                .interact()
                .context("reaction selection aborted")?;
            picks.into_iter().map(|i| reactions[i].name.clone()).collect()
        };

        // Drops prompt.
        let new_drops = if drops.is_empty() {
            vec![]
        } else {
            let labels: Vec<String> = drops.iter().map(|d| d.logical_name.clone()).collect();
            let defaults: Vec<bool> = drops
                .iter()
                .map(|d| match &prior {
                    Some(p) => p.drops.iter().any(|n| n == &d.logical_name),
                    None => true,
                })
                .collect();
            let picks = MultiSelect::with_theme(&theme)
                .with_prompt("drops to keep (space toggles, enter confirms)")
                .items(&labels)
                .defaults(&defaults)
                .interact()
                .context("drop selection aborted")?;
            picks.into_iter().map(|i| drops[i].logical_name.clone()).collect()
        };

        curation.dungeons.insert(
            dungeon.name.clone(),
            DungeonCuration {
                reactions: new_reactions,
                drops: new_drops,
            },
        );
    }

    if dry_run {
        info!("dry-run: curation.json preview:");
        let preview = serde_json::to_string_pretty(&curation)?;
        println!("{preview}");
        return Ok(());
    }

    curation.save()?;
    info!("wrote data/curation.json");

    apply_cleanup(&pool, &config, &curation, &snapshot).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Post-curation cleanup.
// ---------------------------------------------------------------------------

/// Apply the curation to DB + Discord: garbage-collect emojis no longer
/// wanted, and delete `dungeon_reactions` rows whose name is outside the
/// curated allowlist.
async fn apply_cleanup(
    pool: &PgPool,
    config: &Config,
    curation: &Curation,
    snapshot: &WikiSnapshot,
) -> Result<()> {
    let http = Client::builder()
        .user_agent(&config.realmeye_user_agent)
        .build()?;
    let emoji_api = ApplicationEmojiClient::new(
        http,
        &config.discord_token,
        config.discord_application_id,
    );

    // Compute the set of logical emoji names the current curation considers
    // "wanted". Uncurated dungeons contribute their full snapshot; curated
    // dungeons contribute only their allowlisted drops (and their key emoji
    // only if the `key` reaction is still in the allowlist). Portals are
    // always kept — the dungeon exists, so its portal is always relevant.
    let mut wanted = std::collections::HashSet::<String>::new();
    for d in &snapshot.dungeons {
        if let Some(p) = &d.portal {
            wanted.insert(p.logical_name.clone());
        }
        let (keep_key, drops_allow): (bool, Option<&Vec<String>>) =
            match curation.dungeons.get(&d.name) {
                Some(c) => (c.reactions.iter().any(|r| r == "key"), Some(&c.drops)),
                None => (true, None),
            };
        if keep_key {
            if let Some(k) = &d.key {
                wanted.insert(k.logical_name.clone());
            }
        }
        match drops_allow {
            Some(list) => {
                for n in list {
                    wanted.insert(n.clone());
                }
            }
            None => {
                for drop in &d.drops {
                    wanted.insert(drop.logical_name.clone());
                }
            }
        }
    }

    // Delete unwanted emojis. One API call per deletion + a small sleep to
    // stay well under Discord's global rate limit.
    let all_emojis = db::emoji::get_all(pool).await?;
    let mut discord_deleted = 0usize;
    let mut discord_failed = 0usize;
    let mut rows_deleted = 0usize;
    for e in all_emojis {
        if wanted.contains(&e.logical_name) {
            continue;
        }
        info!(
            "removing unused emoji: {} (discord_id={})",
            e.logical_name, e.discord_emoji_id
        );
        match emoji_api.delete(e.discord_emoji_id as u64).await {
            Ok(()) => discord_deleted += 1,
            Err(err) => {
                warn!("  discord delete failed for {}: {err:#}", e.logical_name);
                discord_failed += 1;
            }
        }
        sqlx::query("DELETE FROM bot_emoji WHERE id = $1")
            .bind(e.id)
            .execute(pool)
            .await?;
        rows_deleted += 1;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    info!(
        "emoji cleanup: {discord_deleted} deleted on Discord, {discord_failed} failed, \
         {rows_deleted} bot_emoji rows removed"
    );

    // Delete dungeon_reactions rows outside the allowlist (curated dungeons
    // only — uncurated dungeons keep whatever sync-wiki or seed_builtins put
    // there).
    let mut reactions_deleted = 0u64;
    for (name, c) in &curation.dungeons {
        let template_id: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM dungeon_templates WHERE guild_id IS NULL AND name = $1",
        )
        .bind(name)
        .fetch_optional(pool)
        .await?;
        let template_id = match template_id {
            Some(id) => id,
            None => continue,
        };

        let result = sqlx::query(
            "DELETE FROM dungeon_reactions \
             WHERE dungeon_template_id = $1 AND NOT (name = ANY($2))",
        )
        .bind(template_id)
        .bind(&c.reactions)
        .execute(pool)
        .await?;
        reactions_deleted += result.rows_affected();
    }
    info!("deleted {reactions_deleted} dungeon_reactions rows outside curation allowlists");

    Ok(())
}

// ---------------------------------------------------------------------------
// DB queries.
// ---------------------------------------------------------------------------

struct ReactionRow {
    name: String,
    display_name: String,
}

async fn load_current_reactions(pool: &PgPool, dungeon_name: &str) -> Result<Vec<ReactionRow>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT r.name, r.display_name
        FROM dungeon_reactions r
        JOIN dungeon_templates t ON r.dungeon_template_id = t.id
        WHERE t.guild_id IS NULL AND t.name = $1
        ORDER BY r.sort_order, r.id
        "#,
    )
    .bind(dungeon_name)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, display_name)| ReactionRow { name, display_name })
        .collect())
}
