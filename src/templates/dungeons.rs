// Compile-time built-in dungeon definitions.
// These are seeded as global templates (guild_id = NULL) at bot startup.
// Guild admins can create per-guild overrides via `/dungeon create`.
// Emoji logical names are populated by `starship sync-wiki`; they reference
// entries in bot_emoji but the templates exist even before the scraper runs.
//
// R4: `requires_confirmation` is always false. Item tracking is now native
// Discord reactions — the bot attaches the interest emoji (✅) + each
// required-item emoji to the message, and users click them directly. No
// per-user DB bookkeeping.

pub struct BuiltinReaction {
    pub name: &'static str,
    pub display_name: &'static str,
    pub emoji: &'static str,
    pub num_required: i32,
    pub requires_confirmation: bool,
    pub sort_order: i32,
}

pub struct BuiltinTemplate {
    pub name: &'static str,
    pub display_name: &'static str,
    pub emoji: &'static str,
    pub color: i32,
    pub message_title: &'static str,
    pub message_description: &'static str,
    pub requires_vc: bool,
    pub showcase_emoji: &'static [&'static str],
    pub reactions: &'static [BuiltinReaction],
}

// Shared "Reacts" interest reaction. Every dungeon leads with this at sort_order 0.
const REACTS: BuiltinReaction = BuiltinReaction {
    name: "interest",
    display_name: "Reacts",
    emoji: "✅",
    num_required: 1,
    requires_confirmation: false,
    sort_order: 0,
};

pub const BUILTIN_TEMPLATES: &[BuiltinTemplate] = &[
    BuiltinTemplate {
        name: "oryxs_sanctuary",
        display_name: "Oryx's Sanctuary",
        emoji: "portal_sanctuary",
        color: 0xFF6B35,
        message_title: "Oryx's Sanctuary Headcount",
        message_description: "React to join. Rune + incantation holders: click your item so leaders know what's in.",
        requires_vc: true,
        showcase_emoji: &["marble_seal", "bloodshed_seal", "rainbow_seal"],
        reactions: &[
            REACTS,
            BuiltinReaction {
                name: "wine_cellar_incantation",
                display_name: "Wine Cellar Incantation",
                emoji: "wine_cellar_incantation",
                num_required: 1,
                requires_confirmation: false,
                sort_order: 1,
            },
            BuiltinReaction {
                name: "shield_rune",
                display_name: "Shield Rune",
                emoji: "shield_rune",
                num_required: 1,
                requires_confirmation: false,
                sort_order: 2,
            },
            BuiltinReaction {
                name: "sword_rune",
                display_name: "Sword Rune",
                emoji: "sword_rune",
                num_required: 1,
                requires_confirmation: false,
                sort_order: 3,
            },
            BuiltinReaction {
                name: "helmet_rune",
                display_name: "Helmet Rune",
                emoji: "helmet_rune",
                num_required: 1,
                requires_confirmation: false,
                sort_order: 4,
            },
        ],
    },
    BuiltinTemplate {
        name: "the_void",
        display_name: "The Void",
        emoji: "portal_void",
        color: 0x6A0DAD,
        message_title: "The Void Headcount",
        message_description: "React to join. Key + vial holders: click your item so leaders know what's in.",
        requires_vc: true,
        showcase_emoji: &["bow_of_the_void", "staff_of_the_vital_unity", "robe_of_the_mad_scientist"],
        reactions: &[
            REACTS,
            BuiltinReaction {
                name: "lost_halls_key",
                display_name: "Lost Halls Key",
                emoji: "lost_halls_key",
                num_required: 1,
                requires_confirmation: false,
                sort_order: 1,
            },
            BuiltinReaction {
                name: "vial_of_the_void",
                display_name: "Vial of the Void",
                emoji: "vial_of_the_void",
                num_required: 1,
                requires_confirmation: false,
                sort_order: 2,
            },
        ],
    },
    BuiltinTemplate {
        name: "the_shatters",
        display_name: "The Shatters",
        emoji: "portal_shatters",
        color: 0x4169E1,
        message_title: "The Shatters Headcount",
        message_description: "React to join. Key / tome holders: click your item.",
        requires_vc: true,
        showcase_emoji: &["the_forgotten_crown", "tome_of_the_rites", "sourcestone"],
        reactions: &[
            REACTS,
            BuiltinReaction {
                name: "key",
                display_name: "The Forgotten Crown",
                emoji: "the_forgotten_crown",
                num_required: 1,
                requires_confirmation: false,
                sort_order: 1,
            },
            BuiltinReaction {
                name: "tome",
                display_name: "Tome of the Rites",
                emoji: "tome_of_the_rites",
                num_required: 1,
                requires_confirmation: false,
                sort_order: 2,
            },
        ],
    },
    BuiltinTemplate {
        name: "lost_halls",
        display_name: "Lost Halls",
        emoji: "portal_lost_halls",
        color: 0x8B0000,
        message_title: "Lost Halls Headcount",
        message_description: "React to join. Key holders: click the key.",
        requires_vc: true,
        showcase_emoji: &["void_blade", "plague_poison", "crystal_wand"],
        reactions: &[
            REACTS,
            BuiltinReaction {
                name: "key",
                display_name: "Lost Halls Key",
                emoji: "lost_halls_key",
                num_required: 1,
                requires_confirmation: false,
                sort_order: 1,
            },
        ],
    },
    BuiltinTemplate {
        name: "cultist_hideout",
        display_name: "Cultist Hideout",
        emoji: "portal_cult",
        color: 0x9B59B6,
        message_title: "Cultist Hideout Headcount",
        message_description: "React to join. Key holders: click the key.",
        requires_vc: true,
        showcase_emoji: &["lament_of_the_deep", "daichi_the_ascended", "vesture_of_duality"],
        reactions: &[
            REACTS,
            BuiltinReaction {
                name: "lost_halls_key",
                display_name: "Lost Halls Key",
                emoji: "lost_halls_key",
                num_required: 1,
                requires_confirmation: false,
                sort_order: 1,
            },
        ],
    },
    BuiltinTemplate {
        name: "the_nest",
        display_name: "The Nest",
        emoji: "portal_nest",
        color: 0xF39C12,
        message_title: "The Nest Headcount",
        message_description: "React to join. Key holders: click the key.",
        requires_vc: false,
        showcase_emoji: &["hive_mind", "queen_bee_armor", "royal_honey"],
        reactions: &[
            REACTS,
            BuiltinReaction {
                name: "key",
                display_name: "Nest Key",
                emoji: "nest_key",
                num_required: 1,
                requires_confirmation: false,
                sort_order: 1,
            },
        ],
    },
    BuiltinTemplate {
        name: "fungal_cavern",
        display_name: "Fungal Cavern",
        emoji: "portal_fungal",
        color: 0x27AE60,
        message_title: "Fungal Cavern Headcount",
        message_description: "React to join. Key holders: click the key.",
        requires_vc: false,
        showcase_emoji: &["magnifying_glass", "mossy_protection", "fungal_spell"],
        reactions: &[
            REACTS,
            BuiltinReaction {
                name: "key",
                display_name: "Fungal Cavern Key",
                emoji: "fungal_cavern_key",
                num_required: 1,
                requires_confirmation: false,
                sort_order: 1,
            },
        ],
    },
    BuiltinTemplate {
        name: "crystal_cavern",
        display_name: "Crystal Cavern",
        emoji: "portal_crystal",
        color: 0x3498DB,
        message_title: "Crystal Cavern Headcount",
        message_description: "React to join. Key holders: click the key.",
        requires_vc: false,
        showcase_emoji: &["crystallised_frenzy_shard", "crystal_wand"],
        reactions: &[
            REACTS,
            BuiltinReaction {
                name: "key",
                display_name: "Crystal Cavern Key",
                emoji: "crystal_cavern_key",
                num_required: 1,
                requires_confirmation: false,
                sort_order: 1,
            },
        ],
    },
];
