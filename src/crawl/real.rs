//! Bridge: Crawler over the real DonShadow fetcher.
//!
//! Maps governor lanes to actual egress: "direct" rides the
//! plain socket, "proxy-*" rides DONSEEK_PROXIES entries. The
//! lane id string IS the proxy id (host:port) for proxies.

use std::sync::Arc;
use std::time::Instant;

use futures_util::FutureExt;

use crate::detect::walls::Verdict;
use crate::fetch::client::{CacheState, Fetcher};
use crate::transport::proxy::Proxy;

use super::governor::{Governor, Lane, LaneKind};
use super::{Crawler, FetchedPage, PageFetcher};

/// Build the real crawl stack. `fetcher` is shared state (same
/// jar/pool/cache as everything else in the process); proxies
/// come from env or config, same format as DonSeek.
pub fn build(fetcher: Arc<Fetcher>, proxies: Vec<Proxy>) -> (Crawler, Arc<Governor>) {
    let mut lanes = vec![Lane {
        id: "direct".into(),
        kind: LaneKind::Direct,
    }];
    for p in &proxies {
        lanes.push(Lane {
            id: p.id(),
            kind: LaneKind::Proxy,
        });
    }
    let governor = Arc::new(Governor::new(lanes));

    let fetch: PageFetcher = {
        let fetcher = Arc::clone(&fetcher);
        let proxies = Arc::new(proxies);
        Arc::new(move |url: String, lane: String, referer: Option<String>| {
            let fetcher = Arc::clone(&fetcher);
            let proxies = Arc::clone(&proxies);
            async move {
                let started = Instant::now();
                let proxy = if lane == "direct" {
                    None
                } else {
                    proxies.iter().find(|p| p.id() == lane)
                };
                // Proxy lanes: shared jar OUT : one cookie carrying
                // lane B's identity would link the two egress IPs.
                let use_jar = proxy.is_none();
                match fetcher
                    .fetch_via_jar_ref(&url, proxy, use_jar, referer.as_deref())
                    .await
                {
                    Ok(out) => {
                        // Fresh-window cache hit made ZERO requests:
                        // exclude from governor pacing. Revalidated
                        // hits made a (304) request, keep them.
                        let cached = matches!(out.cache, CacheState::Fresh);
                        FetchedPage {
                            url: out.url,
                            status: out.status,
                            headers: out.headers,
                            body: out.body,
                            verdict: out.verdict,
                            latency: started.elapsed(),
                            cached,
                            error_hint: None,
                        }
                    }
                    Err(e) => FetchedPage {
                        url,
                        status: 0,
                        headers: Vec::new(),
                        body: Vec::new(),
                        verdict: Verdict::Blocked,
                        latency: started.elapsed(),
                        cached: false,
                        error_hint: Some(format!("network: {e}")),
                    },
                }
            }
            .boxed()
        })
    };

    (Crawler::new(fetch, Arc::clone(&governor)), governor)
}
