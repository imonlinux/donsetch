//! Bridge: Crawler over the real DonShadow fetcher.
//!
//! Maps governor lanes to actual egress: "direct" rides the
//! plain socket, proxy ids ride the shared EgressPool (v4 A2).
//! Dead lanes are skipped at fetch time; outcomes report back
//! into the same health world search and fetch use.

use std::sync::Arc;
use std::time::Instant;

use futures_util::FutureExt;

use crate::detect::walls::Verdict;
use crate::fetch::client::{CacheState, Fetcher};
use crate::search::egress::EgressPool;

use super::governor::{Governor, Lane, LaneKind};
use super::{Crawler, FetchedPage, PageFetcher};

/// Build the real crawl stack. `fetcher` is shared state (same
/// jar/pool/cache as everything else in the process); `pool` is
/// the process-wide egress fabric (health + dead benches shared
/// with search and fetch).
pub fn build(fetcher: Arc<Fetcher>, pool: Arc<EgressPool>) -> (Crawler, Arc<Governor>) {
    let proxies = Arc::new(pool.proxies());
    let mut lanes = vec![Lane {
        id: "direct".into(),
        kind: LaneKind::Direct,
    }];
    for p in proxies.iter() {
        lanes.push(Lane {
            id: p.id(),
            kind: LaneKind::Proxy,
        });
    }
    let governor = Arc::new(Governor::new(lanes));

    let fetch: PageFetcher = {
        let fetcher = Arc::clone(&fetcher);
        let pool = Arc::clone(&pool);
        let proxies = Arc::clone(&proxies);
        Arc::new(move |url: String, lane: String, referer: Option<String>| {
            let fetcher = Arc::clone(&fetcher);
            let pool = Arc::clone(&pool);
            let proxies = Arc::clone(&proxies);
            // v4 phase 3: the same adapter registry web_fetch uses
            // shapes crawl fetches, so a reddit/npm class URL rides
            // the cheap .json/registry path instead of the HTML app.
            // The candidate URL stays canonical: dedup, history and
            // output rows key on it, the wire just asks for the
            // rewritten endpoint. DONSETCH_NO_ADAPTERS silences this
            // exactly as it does in web_fetch (handled inside
            // adapters::rewrite).
            let fetch_url = url::Url::parse(&url)
                .ok()
                .and_then(|u| crate::adapters::rewrite(&u).map(|(alt, _via)| alt))
                .unwrap_or_else(|| url.clone());
            let host = url::Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
                .unwrap_or_default();
            async move {
                let started = Instant::now();
                // Never assign a globally benched line mid-crawl.
                if lane != "direct" && pool.is_dead(&lane) {
                    return FetchedPage {
                        url,
                        status: 0,
                        headers: Vec::new(),
                        body: Vec::new(),
                        verdict: Verdict::Blocked,
                        latency: started.elapsed(),
                        cached: false,
                        error_hint: Some(format!("egress: lane {lane} is benched")),
                    };
                }
                let proxy = if lane == "direct" {
                    None
                } else {
                    proxies.iter().find(|p| p.id() == lane).cloned()
                };
                // Proxy lanes: shared jar OUT : one cookie carrying
                // lane B's identity would link the two egress IPs.
                let use_jar = proxy.is_none();
                // v4 F2: cross-process politeness floor. The in-process
                // governor already spaced this request; a second
                // donsetch process on the same host still needs a
                // shared last-stamp. Best-effort, kill-switchable.
                crate::crawl::host_pace::wait_and_stamp(&host).await;
                // A recrawl exists to see what changed : serve the
                // live response, never the revalidation cache (the
                // cached body made every delta recrawl compare the
                // previous crawl's own content and report zero
                // changes forever).
                match fetcher
                    .fetch_via_jar_opts(
                        &fetch_url,
                        proxy.as_ref(),
                        use_jar,
                        referer.as_deref(),
                        true,
                    )
                    .await
                {
                    Ok(out) => {
                        // Fresh-window cache hit made ZERO requests:
                        // exclude from governor pacing. Revalidated
                        // hits made a (304) request, keep them.
                        let cached = matches!(out.cache, CacheState::Fresh);
                        if !cached {
                            match out.status {
                                200 | 304 => {
                                    if !host.is_empty() {
                                        pool.report_ok(&host, &lane);
                                    }
                                    pool.observe_rtt(&lane, started.elapsed());
                                }
                                429 | 503 if !host.is_empty() && lane != "direct" => {
                                    pool.note_fetch_rate_limited(&host, &lane);
                                }
                                _ => {}
                            }
                        }
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
                    Err(e) => {
                        let msg = format!("{e}");
                        if lane != "direct" {
                            if msg.contains("CONNECT -> 407") {
                                pool.note_fetch_auth_fail(&host, &lane);
                            } else if msg.contains("timeout") || msg.contains("timed out") {
                                pool.note_fetch_timeout(&host, &lane);
                            } else {
                                pool.note_fetch_dead(&host, &lane);
                            }
                        }
                        FetchedPage {
                            url,
                            status: 0,
                            headers: Vec::new(),
                            body: Vec::new(),
                            verdict: Verdict::Blocked,
                            latency: started.elapsed(),
                            cached: false,
                            error_hint: Some(format!("network: {e}")),
                        }
                    }
                }
            }
            .boxed()
        })
    };

    (Crawler::new(fetch, Arc::clone(&governor)), governor)
}
