//! Central caps for user-supplied data. Each is enforced at the relevant
//! write path (slash command, modal submit, multi-select toggle) and
//! rejected with a friendly user-facing message before any DB write.
//!
//! Tuning these is cheap — bump the constant, recompile, ship.

/// Maximum number of guild-specific dungeon templates per guild. Globals
/// don't count. Bounds storage growth and prevents abuse (e.g. a script
/// spamming /dungeon create to pile up rows).
pub const CUSTOM_DUNGEONS_PER_GUILD: i64 = 150;

/// Maximum reactions on a single dungeon template (across all categories
/// combined). Discord caps message reactions at 20; beyond that the
/// headcount embed can't render them anyway.
pub const REACTIONS_PER_TEMPLATE: usize = 20;

/// Maximum length of a dungeon template's `name` slug. Slug is derived
/// from the user-supplied display name; this is a safety net for
/// pathological inputs.
pub const TEMPLATE_NAME_MAX: usize = 40;

/// Maximum length of a dungeon template's `display_name`. Discord
/// StringSelect option labels max out at 100 chars; leaving margin for
/// emoji prefixes.
pub const DISPLAY_NAME_MAX: usize = 80;

/// Maximum length of `message_description`. Stays well under Discord's
/// 4096-char embed-description budget.
pub const DESCRIPTION_MAX: usize = 400;

/// Maximum length of a reaction's `display_name`. Headcount embed lists
/// multiple reactions per row; long names break layout.
pub const REACTION_DISPLAY_NAME_MAX: usize = 40;
