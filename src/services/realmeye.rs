//! Minimal RealmEye client for verification.
//!
//! One operation: fetch a player profile and return their public
//! description text. The full wiki scraper at `cli::sync_wiki` does
//! something much heavier; we deliberately keep this isolated so a
//! verification request doesn't pull in scraper dependencies (image
//! processing, bag-tier classification, etc.).
//!
//! Network failure modes are mapped to a typed [`LookupResult`] so the
//! verification handler can render a precise user-facing message
//! ("RealmEye doesn't have a player called X" vs "your description is
//! private" vs "couldn't reach RealmEye"). The handler should not have
//! to inspect HTTP status codes.

use anyhow::Context as _;
use once_cell::sync::Lazy;
use reqwest::{Client, StatusCode};
use scraper::{Html, Selector};
use tracing::warn;

const REALMEYE_BASE: &str = "https://www.realmeye.com";

// Selectors are parsed once at first use — `Selector::parse` walks a state
// machine on every call, so lifting them out of hot paths matters even at
// human-paced traffic. The wiki scraper does the same (see
// `src/cli/sync_wiki.rs:46-72`).

/// Player description block. RealmEye renders it as
/// `<div class="well description" id="d">` with three child
/// `<div class="lineN description-line">` siblings holding each visible
/// line. We match the container both ways: `.well.description` for the
/// current rendering, and `.description` as a slightly looser fallback
/// in case the Bootstrap `well` class is dropped.
static SEL_DESC_CONTAINER: Lazy<Selector> = Lazy::new(|| {
    Selector::parse("div.well.description, div.description").expect("static selector")
});

/// One line inside the description block. Joining these with `\n`
/// preserves the visual line structure the user typed on RealmEye —
/// useful both for the substring code search and for any future
/// admin-facing display.
static SEL_DESC_LINE: Lazy<Selector> =
    Lazy::new(|| Selector::parse(".description-line").expect("static selector"));

/// Player display name: rendered in an `<h1>` at the top of the page.
/// RealmEye URL-canonicalises IGNs (case-insensitive routing), so we
/// persist the canonical form to keep `(guild, ign)` uniqueness honest.
static SEL_HEADER: Lazy<Selector> = Lazy::new(|| Selector::parse("h1").expect("static selector"));

/// Result of [`RealmEyeClient::lookup_player`]. The handler matches on this
/// to render an honest user-facing message — never expose raw HTTP errors.
#[derive(Debug)]
pub enum LookupResult {
    /// Page rendered, description visible.
    Found {
        description: String,
        canonical_ign: String,
    },
    /// 404 — no such player on RealmEye.
    NotFound,
    /// Page rendered but description block is absent / empty (privacy
    /// setting, or the profile has never been populated).
    Private { canonical_ign: String },
    /// 429, 503, or a Cloudflare interstitial. Treat as "try again later".
    Throttled,
    /// Network failure or unexpected status. The handler shows a generic
    /// "couldn't reach RealmEye" message and suggests admin /mv.
    TransportError(anyhow::Error),
}

#[derive(Clone)]
pub struct RealmEyeClient {
    http: Client,
}

impl RealmEyeClient {
    /// Build a client with the project-wide RealmEye User-Agent. Mirrors
    /// the wiki scraper at `src/cli/sync_wiki.rs:198-200`.
    pub fn new(user_agent: &str) -> anyhow::Result<Self> {
        let http = Client::builder()
            .user_agent(user_agent)
            .build()
            .context("building reqwest client for RealmEye")?;
        Ok(Self { http })
    }

    /// Fetch `/player/<ign>` and parse out the description + canonical
    /// IGN. The `ign` argument is URL-encoded — RealmEye accepts only
    /// `[A-Za-z]` so the encoder is a sanity belt rather than an
    /// architectural feature.
    pub async fn lookup_player(&self, ign: &str) -> LookupResult {
        let url = format!("{REALMEYE_BASE}/player/{}", urlencoding_encode(ign));

        let resp = match self.http.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return LookupResult::TransportError(e.into()),
        };

        match resp.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => return LookupResult::NotFound,
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                return LookupResult::Throttled;
            }
            other => {
                return LookupResult::TransportError(anyhow::anyhow!(
                    "unexpected status {other} from {url}"
                ));
            }
        }

        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => return LookupResult::TransportError(e.into()),
        };

        parse_player_page(&body, ign)
    }
}

/// Pure HTML → [`LookupResult`] parse. Split out from the network
/// fetcher so unit tests can exercise it against saved fixtures.
fn parse_player_page(body: &str, fallback_ign: &str) -> LookupResult {
    let doc = Html::parse_document(body);

    // Canonical IGN: RealmEye's <h1> shows the actual case-correct
    // name. If parsing fails, fall back to the user's input — a
    // missing <h1> is unusual and not worth refusing verification
    // over.
    let canonical_ign = doc
        .select(&SEL_HEADER)
        .next()
        .map(|h| h.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback_ign.to_string());

    let Some(container) = doc.select(&SEL_DESC_CONTAINER).next() else {
        // No container at all — either RealmEye changed the markup again
        // or the player is private in a way we haven't seen before.
        warn!(
            ign = fallback_ign,
            "no description container matched — RealmEye may have renamed the class"
        );
        return LookupResult::Private { canonical_ign };
    };

    // Prefer per-line extraction so multi-line descriptions keep their
    // line boundaries (collecting on the container concatenates text
    // nodes without separators). If the container is empty of
    // `.description-line` children — older or alternate templates —
    // fall back to the container's combined text.
    let lines: Vec<String> = container
        .select(&SEL_DESC_LINE)
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let description = if lines.is_empty() {
        container.text().collect::<String>().trim().to_string()
    } else {
        lines.join("\n")
    };

    if description.is_empty() {
        LookupResult::Private { canonical_ign }
    } else {
        LookupResult::Found {
            description,
            canonical_ign,
        }
    }
}

/// Minimal RFC 3986 path-segment encoder. `urlencoding` isn't a project dep
/// and pulling one in for a single helper is overkill — RealmEye IGNs are
/// `[A-Za-z]{1,12}` in practice, so percent-encoding only fires on
/// malformed input we'd reject anyway. Defensive belt against the user
/// pasting weird characters into the modal.
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Case-insensitive substring check for the verification code in a
/// description. A user might paste the code with surrounding text or
/// punctuation, so we don't insist on it being on its own line. The code
/// is six digits — collisions on natural English text are negligible.
pub fn description_contains_code(description: &str, code: &str) -> bool {
    description.contains(code)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Live RealmEye HTML for `realmeye.com/player/MasterJet` saved on
    /// 2026-04-25. The page's description was set to "230600" — the
    /// regression case from the e2e test where the bot reported
    /// CodeMissing despite the code being visible. Pin this so a future
    /// CSS rename on RealmEye's side fails CI loudly instead of silently
    /// breaking verification.
    const FIXTURE_MASTERJET: &str = include_str!("testdata/realmeye_masterjet.html");

    #[test]
    fn parses_canonical_ign_and_description() {
        let result = parse_player_page(FIXTURE_MASTERJET, "masterjet");
        match result {
            LookupResult::Found {
                description,
                canonical_ign,
            } => {
                assert_eq!(canonical_ign, "MasterJet");
                assert!(
                    description.contains("230600"),
                    "expected description to contain the code, got: {description:?}",
                );
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn description_contains_code_substring_match() {
        assert!(description_contains_code(
            "here is my code: 230600 thanks",
            "230600"
        ));
        assert!(description_contains_code("230600", "230600"));
        assert!(!description_contains_code("23060", "230600"));
        assert!(!description_contains_code("", "230600"));
    }

    #[test]
    fn missing_container_yields_private_not_found() {
        // Page with an h1 but no description container — what we'd see
        // if RealmEye renamed everything. canonical_ign is recovered
        // from the h1 so the user-facing error names the right player.
        let html = "<html><body><h1>Foo</h1></body></html>";
        match parse_player_page(html, "foo") {
            LookupResult::Private { canonical_ign } => assert_eq!(canonical_ign, "Foo"),
            other => panic!("expected Private, got {other:?}"),
        }
    }

    #[test]
    fn description_lines_join_with_newlines() {
        // Synthetic page mirroring RealmEye's `description-line`
        // structure. Verifies that multi-line descriptions don't get
        // their lines glued together — important for any future
        // display, and a defence against substring matches that should
        // not span line boundaries.
        let html = r#"
            <html><body>
              <h1>Foo</h1>
              <div class="well description">
                <div class="line1 description-line">first line</div>
                <div class="line2 description-line">second line</div>
                <div class="line3 description-line"></div>
              </div>
            </body></html>
        "#;
        match parse_player_page(html, "foo") {
            LookupResult::Found { description, .. } => {
                assert_eq!(description, "first line\nsecond line");
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }
}
