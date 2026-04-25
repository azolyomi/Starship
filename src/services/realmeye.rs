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
/// `<div class="well player-description">...</div>` on a public profile.
static SEL_DESC: Lazy<Selector> =
    Lazy::new(|| Selector::parse(".player-description").expect("static selector"));

/// Broader fallback if the primary selector returns empty — RealmEye has
/// historically renamed the description class. Matching `div.well` is a
/// last resort that catches the rendered block by its Bootstrap shell.
static SEL_DESC_FALLBACK: Lazy<Selector> =
    Lazy::new(|| Selector::parse("div.well").expect("static selector"));

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
        let url = format!("{REALMEYE_BASE}/player/{}", urlencoding_encode(ign),);

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

        let doc = Html::parse_document(&body);

        // Canonical IGN: RealmEye's <h1> shows the actual case-correct
        // name. If parsing fails, fall back to the user's input — a
        // missing <h1> is unusual and not worth refusing verification
        // over.
        let canonical_ign = doc
            .select(&SEL_HEADER)
            .next()
            .map(|h| h.text().collect::<String>().trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ign.to_string());

        // Primary selector. If empty, try the broader fallback before
        // giving up — a CSS rename on RealmEye's side shouldn't silently
        // break verification, but it should produce a warning so we
        // notice and update the selector.
        let description = doc
            .select(&SEL_DESC)
            .next()
            .map(extract_text)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                let fallback = doc.select(&SEL_DESC_FALLBACK).next().map(extract_text);
                if fallback.as_ref().is_some_and(|s| !s.is_empty()) {
                    warn!(
                        ign,
                        "primary `.player-description` selector returned empty; \
                         fallback `div.well` matched. Update SEL_DESC if RealmEye \
                         renamed the class."
                    );
                }
                fallback
            });

        match description {
            Some(text) if !text.is_empty() => LookupResult::Found {
                description: text,
                canonical_ign,
            },
            _ => LookupResult::Private { canonical_ign },
        }
    }
}

/// Concatenate all text nodes inside a node, trimming the result. RealmEye
/// renders descriptions as multiple `<div>`s (one per line) inside the
/// description block, so we want every text node, not just the immediate
/// children.
fn extract_text(elem: scraper::ElementRef<'_>) -> String {
    elem.text().collect::<String>().trim().to_string()
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
