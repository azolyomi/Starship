//! Small string helpers shared across handlers and commands.

/// Convert a snake_case logical name into a Title-Cased display name.
/// Used for auto-deriving reaction display names from `bot_emoji.logical_name`
/// when an admin adds a reaction via the `/dungeon edit` multi-select.
///
/// Examples:
/// - `lost_halls_key` → `"Lost Halls Key"`
/// - `class_wizard`   → `"Class Wizard"`
/// - `vial_of_pure_darkness` → `"Vial Of Pure Darkness"`
pub fn snake_to_title(s: &str) -> String {
    s.split('_')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut chars = p.chars();
            match chars.next() {
                Some(c) => c
                    .to_uppercase()
                    .chain(chars.flat_map(|c| c.to_lowercase()))
                    .collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalise a display name to a logical slug. Apostrophes are
/// *stripped* (not replaced with underscores) so "Oryx's Sanctuary"
/// collapses cleanly to "oryxs_sanctuary" rather than "oryx_s_sanctuary"
/// — a single-letter segment would break emoji name lookups.
///
/// Used by `sync-wiki` to derive `bot_emoji.logical_name` and by
/// `/dungeon create` so users can type a friendly display name ("MBC
/// Skip") and the bot derives the internal slug ("mbc_skip").
pub fn slug_from_display(name: &str) -> String {
    let stripped: String = name
        .chars()
        .filter(|c| *c != '\'' && *c != '\u{2019}')
        .collect();
    stripped
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_to_title_handles_common_cases() {
        assert_eq!(snake_to_title("lost_halls_key"), "Lost Halls Key");
        assert_eq!(snake_to_title("class_wizard"), "Class Wizard");
        assert_eq!(
            snake_to_title("vial_of_pure_darkness"),
            "Vial Of Pure Darkness"
        );
        assert_eq!(snake_to_title(""), "");
        assert_eq!(snake_to_title("__a__b__"), "A B");
    }

    #[test]
    fn slug_strips_straight_apostrophe() {
        assert_eq!(slug_from_display("Oryx's Sanctuary"), "oryxs_sanctuary");
        assert_eq!(slug_from_display("Pirate's Cave"), "pirates_cave");
    }

    #[test]
    fn slug_strips_curly_apostrophe() {
        assert_eq!(
            slug_from_display("Oryx\u{2019}s Sanctuary"),
            "oryxs_sanctuary"
        );
    }

    #[test]
    fn slug_basic_whitespace_and_punct() {
        assert_eq!(slug_from_display("Snake Pit"), "snake_pit");
        assert_eq!(slug_from_display("D.O.G. Realm"), "d_o_g_realm");
    }

    #[test]
    fn slug_collapses_multiple_separators() {
        assert_eq!(slug_from_display("  Lost   Halls  "), "lost_halls");
        assert_eq!(slug_from_display("Lost---Halls"), "lost_halls");
    }

    #[test]
    fn slug_empty_when_no_alphanumerics() {
        assert_eq!(slug_from_display(""), "");
        assert_eq!(slug_from_display("---"), "");
    }
}
