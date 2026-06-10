//! Link-cleaner chat bot plugin.
//!
//! Observes chat messages, extracts URLs, strips tracking / share query
//! parameters according to configurable rules, and replies into the same room
//! with the cleaned links. Modeled after the standalone Mumble link-bot.
//!
//! The bot is a plugin-owned participant (a roster member with no connection of
//! its own). It never consumes the original message — [`ServerPlugin::on_message`]
//! returns `Ok(false)` so normal chat delivery proceeds — and replies via the
//! room-targeted [`ServerCtx::post_chat_as`] capability so it can answer in the
//! room a message came from without relocating.
//!
//! ## Cleaning rules
//!
//! - `global_strip_params`: wildcard key patterns (e.g. `utm_*`, `fbclid`)
//!   removed from every URL's query.
//! - `domains.<host>`: per-domain override matched by exact host, `www.`-stripped
//!   host, base domain (last two labels), or a `*` wildcard pattern. Either
//!   `remove_all_params = true` (drop the whole query) or an `allowed_params`
//!   allow-list (keep only those keys).
//!
//! A URL is only echoed back when cleaning actually removed something.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use rumble_protocol::proto;
use tracing::{info, warn};
use url::Url;
use uuid::Uuid;

use crate::{
    plugin::{ParticipantHandle, PluginFactory, ServerCtx, ServerPlugin},
    state::ClientHandle,
};

/// Delay before posting the cleaned-links reply. The triggering message is
/// broadcast by the built-in handler *after* this plugin's `on_message` returns
/// `Ok(false)`; a short delay lets it land first so the reply appears below it.
const REPLY_DELAY: Duration = Duration::from_millis(75);

/// Per-domain cleaning rule. Doubles as the deserialized config shape.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DomainRule {
    /// Drop the entire query string for matching hosts.
    #[serde(default)]
    pub remove_all_params: bool,
    /// When `remove_all_params` is false and this is non-empty, keep only these
    /// query keys (after global strip patterns are applied).
    #[serde(default)]
    pub allowed_params: Vec<String>,
}

/// Configuration for the link-cleaner plugin (`[plugins.link-cleaner]`).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct LinkCleanerConfig {
    /// Display name of the bot participant.
    pub username: String,
    /// Roster label shown next to the bot (e.g. "bot").
    pub label: Option<String>,
    /// ACL groups for the bot participant (always implicitly in "default").
    pub groups: Vec<String>,
    /// Skip messages longer than this many bytes.
    pub max_message_length: usize,
    /// Wildcard query-key patterns stripped from every URL.
    pub global_strip_params: Vec<String>,
    /// Per-domain overrides keyed by host pattern.
    pub domains: HashMap<String, DomainRule>,
}

impl Default for LinkCleanerConfig {
    fn default() -> Self {
        Self {
            username: "LinkCleaner".to_string(),
            label: Some("bot".to_string()),
            groups: Vec::new(),
            max_message_length: 5000,
            global_strip_params: Vec::new(),
            domains: HashMap::new(),
        }
    }
}

/// Compiled cleaning rules.
struct LinkRules {
    global_strip: Vec<String>,
    domains: HashMap<String, DomainRule>,
}

impl LinkRules {
    /// Build from config, falling back to sensible built-in defaults when the
    /// config supplies no rules at all.
    fn from_config(cfg: &LinkCleanerConfig) -> Self {
        if cfg.global_strip_params.is_empty() && cfg.domains.is_empty() {
            Self::defaults()
        } else {
            Self {
                global_strip: cfg.global_strip_params.clone(),
                domains: cfg.domains.clone(),
            }
        }
    }

    /// Built-in default rules (a useful subset of the reference config).
    fn defaults() -> Self {
        let global_strip = ["utm_*", "gclid", "fbclid", "mc_eid", "mc_cid", "igshid"]
            .into_iter()
            .map(String::from)
            .collect();

        // (host pattern, remove_all, allowed keys)
        let table: &[(&str, bool, &[&str])] = &[
            ("youtube.com", false, &["v", "t"]),
            ("youtu.be", false, &["t"]),
            ("twitter.com", true, &[]),
            ("x.com", true, &[]),
            ("*.reddit.com", false, &["url"]),
            ("amazon.*", false, &["k", "s"]),
            ("facebook.com", true, &[]),
            ("instagram.com", true, &[]),
            ("tiktok.com", true, &[]),
        ];
        let domains = table
            .iter()
            .map(|(host, remove_all, allowed)| {
                (
                    (*host).to_string(),
                    DomainRule {
                        remove_all_params: *remove_all,
                        allowed_params: allowed.iter().map(|s| (*s).to_string()).collect(),
                    },
                )
            })
            .collect();

        Self { global_strip, domains }
    }

    /// Find a domain rule for `host` by exact / `www.`-stripped / base-domain /
    /// wildcard match, in that priority order.
    fn match_domain(&self, host: &str) -> Option<&DomainRule> {
        let mut candidates: Vec<String> = vec![host.to_string()];
        if let Some(stripped) = host.strip_prefix("www.") {
            candidates.push(stripped.to_string());
        }
        let labels: Vec<&str> = host.split('.').collect();
        if labels.len() >= 2 {
            candidates.push(labels[labels.len() - 2..].join("."));
        }

        for cand in &candidates {
            if let Some(rule) = self.domains.get(cand) {
                return Some(rule);
            }
            for (pattern, rule) in &self.domains {
                if pattern.contains('*') && glob_match(pattern, cand) {
                    return Some(rule);
                }
            }
        }
        None
    }

    /// Clean a single URL. Returns `Some(cleaned)` only when a parameter was
    /// actually removed.
    fn clean_url(&self, raw: &str) -> Option<String> {
        let mut url = Url::parse(raw).ok()?;
        url.query()?; // nothing to strip without a query
        let host = url.host_str()?.to_ascii_lowercase();
        let rule = self.match_domain(&host);

        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        if pairs.is_empty() {
            return None;
        }

        let mut changed = false;
        let kept: Vec<(String, String)> = if rule.is_some_and(|r| r.remove_all_params) {
            changed = true;
            Vec::new()
        } else {
            let allowed = rule.map(|r| r.allowed_params.as_slice()).unwrap_or(&[]);
            let use_allow = !allowed.is_empty();
            let mut kept = Vec::new();
            for (k, v) in pairs {
                if self.global_strip.iter().any(|p| glob_match(p, &k)) {
                    changed = true;
                    continue;
                }
                if use_allow {
                    if allowed.iter().any(|a| a == &k) {
                        kept.push((k, v));
                    } else {
                        changed = true;
                    }
                } else {
                    kept.push((k, v));
                }
            }
            kept
        };

        if !changed {
            return None;
        }

        if kept.is_empty() {
            url.set_query(None);
        } else {
            url.query_pairs_mut()
                .clear()
                .extend_pairs(kept.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
        Some(url.to_string())
    }

    /// Extract URLs from `text` and return `(original, cleaned)` pairs for the
    /// ones that changed, de-duplicated by original.
    fn clean_text(&self, text: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for raw in extract_urls(text) {
            if !seen.insert(raw.clone()) {
                continue;
            }
            if let Some(cleaned) = self.clean_url(&raw)
                && cleaned != raw
            {
                out.push((raw, cleaned));
            }
        }
        out
    }
}

/// True if a single-`*`-glob `pattern` (e.g. `utm_*`, `*.reddit.com`,
/// `amazon.*`) matches `text`. `*` matches any (possibly empty) run.
fn glob_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let n = parts.len();
    if !text.starts_with(parts[0]) || !text.ends_with(parts[n - 1]) {
        return false;
    }
    if parts[0].len() + parts[n - 1].len() > text.len() {
        return false;
    }
    let mut pos = parts[0].len();
    let end = text.len() - parts[n - 1].len();
    for part in &parts[1..n - 1] {
        if part.is_empty() {
            continue;
        }
        match text[pos..end].find(part) {
            Some(idx) => pos += idx + part.len(),
            None => return false,
        }
    }
    true
}

/// Characters allowed in a bare URL we extract from chat text. Mirrors the
/// reference bot's `[\w\-._~:/?#\[\]@!$&'()*+,;=%]` set.
fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | ':'
                | '/'
                | '?'
                | '#'
                | '['
                | ']'
                | '@'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | '%'
        )
}

/// Scan plain text for `http(s)://` URLs.
fn extract_urls(text: &str) -> Vec<String> {
    // Sentence punctuation that is almost never part of a trailing URL.
    const TRAILING: &[char] = &['.', ',', '!', '?', ';', ':', ')', ']', '}', '\'', '"'];
    let lower = text.to_ascii_lowercase(); // ASCII-only change keeps byte offsets aligned
    let mut urls = Vec::new();
    let mut from = 0;
    while let Some(rel) = lower[from..].find("http") {
        let start = from + rel;
        let scheme_len = if lower[start..].starts_with("https://") {
            8
        } else if lower[start..].starts_with("http://") {
            7
        } else {
            from = start + 4;
            continue;
        };

        let mut end = start + scheme_len;
        while end < text.len() {
            let c = text[end..].chars().next().unwrap();
            if is_url_char(c) {
                end += c.len_utf8();
            } else {
                break;
            }
        }

        let url = text[start..end].trim_end_matches(TRAILING);
        if url.len() > scheme_len {
            urls.push(url.to_string());
        }
        from = end;
    }
    urls
}

/// Render the reply body. URLs are emitted bare so the client linkifies them.
fn render_reply(pairs: &[(String, String)]) -> String {
    if pairs.len() == 1 {
        format!("Cleaned link: {}", pairs[0].1)
    } else {
        let mut s = String::from("Cleaned links:");
        for (_, cleaned) in pairs {
            s.push('\n');
            s.push_str(cleaned);
        }
        s
    }
}

/// The link-cleaner plugin.
pub struct LinkCleanerPlugin {
    rules: LinkRules,
    username: String,
    label: Option<String>,
    groups: Vec<String>,
    max_message_length: usize,
    /// Bot participant id, 0 until `start` registers it.
    bot_id: AtomicU64,
    /// Keeps the participant alive; dropping it removes the bot from the roster.
    handle: Mutex<Option<ParticipantHandle>>,
}

impl LinkCleanerPlugin {
    /// Build from a parsed config section.
    pub fn with_config(cfg: LinkCleanerConfig) -> Self {
        Self {
            rules: LinkRules::from_config(&cfg),
            username: cfg.username,
            label: cfg.label,
            groups: cfg.groups,
            max_message_length: cfg.max_message_length,
            bot_id: AtomicU64::new(0),
            handle: Mutex::new(None),
        }
    }

    /// Inspect a chat message authored by `author_id`; if it contains cleanable
    /// links, spawn a task to reply into the author's room as the bot.
    fn react(&self, author_id: u64, text: &str, ctx: &ServerCtx) {
        let bot_id = self.bot_id.load(Ordering::SeqCst);
        if bot_id == 0 || author_id == bot_id || text.len() > self.max_message_length {
            return;
        }
        let cleaned = self.rules.clean_text(text);
        if cleaned.is_empty() {
            return;
        }
        let reply = render_reply(&cleaned);
        let state = ctx.state().clone();
        let persistence = ctx.persistence().clone();
        tokio::spawn(async move {
            tokio::time::sleep(REPLY_DELAY).await;
            let Some(room) = state.get_user_room(author_id).await else {
                return;
            };
            let bot_ctx = ServerCtx::new(state, persistence);
            if let Err(e) = bot_ctx.post_chat_as(bot_id, room, reply).await {
                warn!("link-cleaner: failed to post cleaned links: {e}");
            }
        });
    }
}

#[async_trait::async_trait]
impl ServerPlugin for LinkCleanerPlugin {
    fn name(&self) -> &str {
        "link-cleaner"
    }

    async fn on_message(
        &self,
        _envelope: &proto::Envelope,
        _sender: &Arc<ClientHandle>,
        _ctx: &ServerCtx,
    ) -> Result<bool> {
        // Chat reactions are handled in `on_chat_accepted`, which fires only
        // after auth/ownership/ACL validation accepts the message — reacting
        // here (pre-validation) would let any client spoof a cleaned-link reply
        // into another user's room or bypass a chat-mute (issue #32). Nothing
        // else for this plugin to do on the raw control stream.
        Ok(false)
    }

    async fn on_chat_accepted(&self, author_id: u64, _room: Uuid, text: &str, ctx: &ServerCtx) {
        // `author_id` is server-authoritative and the message has passed the
        // TEXT_MESSAGE ACL. `react` re-resolves the author's current room when
        // it posts (after a short delay), matching the prior behavior.
        self.react(author_id, text, ctx);
    }

    async fn start(&self, ctx: &ServerCtx) -> Result<()> {
        let handle = ctx
            .register_participant(self.username.clone(), self.label.clone(), self.groups.clone())
            .await?;
        let id = handle.user_id();
        self.bot_id.store(id, Ordering::SeqCst);
        *self.handle.lock().unwrap() = Some(handle);
        info!(
            user_id = id,
            name = %self.username,
            domains = self.rules.domains.len(),
            "link-cleaner plugin started"
        );
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Drop the handle → participant removed from the roster.
        self.handle.lock().unwrap().take();
        info!("link-cleaner plugin stopped");
        Ok(())
    }
}

/// Factory for [`LinkCleanerPlugin`] from TOML config.
pub struct LinkCleanerFactory;

impl PluginFactory for LinkCleanerFactory {
    fn name(&self) -> &str {
        "link-cleaner"
    }

    fn create(&self, config: Option<toml::Value>) -> anyhow::Result<Box<dyn ServerPlugin>> {
        let cfg: LinkCleanerConfig = match config {
            Some(v) => v.try_into()?,
            None => LinkCleanerConfig::default(),
        };
        Ok(Box::new(LinkCleanerPlugin::with_config(cfg)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> LinkRules {
        LinkRules::defaults()
    }

    #[test]
    fn strips_global_tracking_params() {
        let r = rules();
        let cleaned = r
            .clean_url("https://example.com/page?id=42&utm_source=news&fbclid=xyz")
            .unwrap();
        assert_eq!(cleaned, "https://example.com/page?id=42");
    }

    #[test]
    fn youtube_allow_list_keeps_v_and_t() {
        let r = rules();
        let cleaned = r
            .clean_url("https://www.youtube.com/watch?v=abc123&list=PL&utm_medium=x&t=30")
            .unwrap();
        // Only v and t survive; order preserved from the original query.
        assert_eq!(cleaned, "https://www.youtube.com/watch?v=abc123&t=30");
    }

    #[test]
    fn remove_all_params_for_twitter() {
        let r = rules();
        let cleaned = r.clean_url("https://twitter.com/user/status/123?s=20&ref=foo").unwrap();
        assert_eq!(cleaned, "https://twitter.com/user/status/123");
    }

    #[test]
    fn wildcard_subdomain_reddit() {
        let r = rules();
        // *.reddit.com allow-list keeps only `url`.
        let cleaned = r
            .clean_url("https://old.reddit.com/r/rust/?utm_source=share&url=keep")
            .unwrap();
        assert_eq!(cleaned, "https://old.reddit.com/r/rust/?url=keep");
    }

    #[test]
    fn wildcard_tld_amazon() {
        let r = rules();
        let cleaned = r
            .clean_url("https://amazon.co.uk/dp/B000?k=phone&ref=nav&s=electronics")
            .unwrap();
        assert_eq!(cleaned, "https://amazon.co.uk/dp/B000?k=phone&s=electronics");
    }

    #[test]
    fn unchanged_url_returns_none() {
        let r = rules();
        assert!(r.clean_url("https://example.com/page?id=42").is_none());
        assert!(r.clean_url("https://example.com/page").is_none());
    }

    #[test]
    fn extracts_and_trims_trailing_punctuation() {
        let urls = extract_urls("see https://x.com/a?s=1, and (https://youtu.be/x?t=1&utm=2).");
        assert_eq!(urls, vec!["https://x.com/a?s=1", "https://youtu.be/x?t=1&utm=2"]);
    }

    #[test]
    fn clean_text_dedupes_and_filters_unchanged() {
        let r = rules();
        let pairs = r.clean_text("https://example.com/clean https://twitter.com/a?s=1 https://twitter.com/a?s=1");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].1, "https://twitter.com/a");
    }

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("utm_*", "utm_source"));
        assert!(!glob_match("utm_*", "ref"));
        assert!(glob_match("*.reddit.com", "old.reddit.com"));
        assert!(!glob_match("*.reddit.com", "reddit.com"));
        assert!(glob_match("amazon.*", "amazon.co.uk"));
        assert!(glob_match("gclid", "gclid"));
    }
}
