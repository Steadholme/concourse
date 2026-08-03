//! Deterministic, loopback-only Atrium browser-gate fixture.
//!
//! This binary is not referenced by the production image or router. It accepts only argv so the
//! gate can exercise a bounded synthetic inbox without adding runtime configuration to Atrium.

use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use atrium::audit::AuditSink;
use atrium::config::Config;
use atrium::inbox::{Engine, InboxCache};
use atrium::source::{InboxRow, Section, SectionKind, Source};
use atrium::store::InMemoryActionStore;
use atrium::AppState;

#[derive(Clone)]
struct GateSource {
    kind: SectionKind,
    section: Section,
    down: bool,
    delay: Duration,
}

#[async_trait]
impl Source for GateSource {
    fn kind(&self) -> SectionKind {
        self.kind
    }

    async fn fetch(&self, _user_sub: &str, limit: i64) -> Result<Section, String> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if self.down {
            return Err("synthetic browser-gate outage".to_string());
        }
        let mut section = self.section.clone();
        section.rows.truncate(limit.max(0) as usize);
        Ok(section)
    }
}

fn row(key: &str, title: &str, snippet: &str, link: &str, at: i64) -> InboxRow {
    InboxRow {
        key: key.to_string(),
        title: title.to_string(),
        snippet: snippet.to_string(),
        source: String::new(),
        at: Some(at),
        link: link.to_string(),
        count: Some(1),
    }
}

fn source(kind: SectionKind, rows: Vec<InboxRow>, down: bool, delay: Duration) -> Arc<dyn Source> {
    Arc::new(GateSource {
        kind,
        section: Section {
            total: rows.iter().map(|item| item.count.unwrap_or(1)).sum(),
            rows,
        },
        down,
        delay,
    })
}

fn gate_engine(scenario: &str, delay: Duration) -> Result<Engine, String> {
    if scenario == "down-all" {
        return Ok(Engine::new(
            Some(source(SectionKind::Chat, Vec::new(), true, delay)),
            Some(source(SectionKind::Notifications, Vec::new(), true, delay)),
            Some(source(SectionKind::Feed, Vec::new(), true, delay)),
        ));
    }
    if scenario == "rows-zero" {
        return Ok(Engine::new(None, None, None));
    }
    if scenario != "mixed" && scenario != "hostile" {
        return Err(format!(
            "unknown scenario {scenario:?}; expected mixed, hostile, rows-zero, or down-all"
        ));
    }

    let long_hostile_title = format!(
        "<script>Gate hostile</script> {}{}",
        char::from_u32(0x202e).expect("valid bidi test scalar"),
        "long-boundary-".repeat(32)
    );
    let chat = vec![row(
        "chat-alpha",
        "Alpha dispatch",
        "A stable synthetic chat preview",
        "/alpha",
        1_700_000_003,
    )];
    let notifications = vec![row(
        "notification-beta",
        "Beta dispatch",
        "A stable synthetic notification preview",
        "/beta",
        1_700_000_002,
    )];
    let feed = vec![InboxRow {
        source: "Hostile & synthetic feed".to_string(),
        ..row(
            "feed-hostile",
            &long_hostile_title,
            "\"><img src=x onerror=alert(1)> — synthetic only",
            "javascript:alert(1)",
            1_700_000_001,
        )
    }];

    Ok(Engine::new(
        Some(source(SectionKind::Chat, chat, false, delay)),
        Some(source(
            SectionKind::Notifications,
            notifications,
            false,
            delay,
        )),
        Some(source(SectionKind::Feed, feed, false, delay)),
    ))
}

fn usage() -> &'static str {
    "usage: atrium_gate <scenario> <cache_ttl_ms> <bind_addr> [source_delay_ms]"
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let scenario = args.next().ok_or_else(|| usage().to_string())?;
    let ttl_arg = args.next().ok_or_else(|| usage().to_string())?;
    let ttl_ms = ttl_arg
        .parse::<u64>()
        .map_err(|error| format!("invalid cache_ttl_ms: {error}"))?;
    let bind_arg = args.next().ok_or_else(|| usage().to_string())?;
    let bind = bind_arg
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid bind_addr: {error}"))?;
    let delay_ms = args
        .next()
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|error| format!("invalid source_delay_ms: {error}"))
        })
        .transpose()?
        .unwrap_or(0);
    if args.next().is_some() {
        return Err(usage().into());
    }

    let state = AppState {
        config: Arc::new(Config::dev()),
        engine: Arc::new(gate_engine(&scenario, Duration::from_millis(delay_ms))?),
        cache: InboxCache::new(Duration::from_millis(ttl_ms)),
        audit: AuditSink::disabled(),
        store: Arc::new(InMemoryActionStore::new()),
    };
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, atrium::app(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mixed_fixture_is_bounded_and_contains_only_synthetic_rows() {
        let inbox = gate_engine("mixed", Duration::ZERO)
            .expect("mixed fixture")
            .aggregate("browser-fixture", 50)
            .await;
        assert_eq!(inbox.total_unread(), 3);
        let atrium::inbox::SectionState::Ready(feed) = inbox.feed else {
            panic!("feed fixture should be ready");
        };
        assert_eq!(feed.rows.len(), 1);
        assert!(feed.rows[0].title.contains("<script>Gate hostile</script>"));
        assert_eq!(feed.rows[0].link, "javascript:alert(1)");
    }

    #[tokio::test]
    async fn down_all_fixture_exposes_three_unavailable_sections() {
        let inbox = gate_engine("down-all", Duration::ZERO)
            .expect("down fixture")
            .aggregate("browser-fixture", 50)
            .await;
        assert_eq!(inbox.unavailable_kinds().len(), 3);
        assert_eq!(inbox.total_unread(), 0);
    }
}
