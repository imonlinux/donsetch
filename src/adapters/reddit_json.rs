//! Reddit `.json` adapter: threads and subreddit listings as
//! structured markdown from the site's own keyless JSON API.
//!
//! One plain-HTTP GET replaces the ghost-prone HTML scrape:
//! comment trees with scores/ages, listings with vote counts :
//! no JS shell, no login overlay. Anything unexpected returns
//! `None` → the caller falls back to the old.reddit HTML
//! extractor or generic DonSift.

use serde_json::Value;

use crate::extract::{ContentKind, ExtractOptions, Extracted};

const MAX_COMMENTS: usize = 150;
const MAX_DEPTH: usize = 8;

/// Entry point. `url` is the final fetched URL (old.reddit.com/
/// ...json after the fetch-level rewrite).
pub fn extract(body: &[u8], url: &str, opts: &ExtractOptions) -> Option<Extracted> {
    let host = url::Url::parse(url).ok()?.host_str()?.to_string();
    if !host.ends_with("reddit.com") {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;

    let md = match &v {
        // Thread: [post-listing, comments-listing].
        Value::Array(arr) if arr.len() == 2 => {
            let post = arr
                .first()?
                .pointer("/data/children/0/data")
                .cloned()
                .unwrap_or_default();
            post.get("title")?;
            let comments = arr
                .get(1)?
                .pointer("/data/children")
                .cloned()
                .unwrap_or_default();
            render_thread(&post, &comments)
        }
        // Listing: {kind: Listing, data: {children: [t3...]}}.
        Value::Object(_) if v.pointer("/data/children/0/data/title").is_some() => {
            let children = v.pointer("/data/children")?.clone();
            render_listing(&children)
        }
        _ => return None,
    };

    let total = md.len();
    let max = opts.max_chars.unwrap_or(16_000).max(200);
    let (slice, next) = crate::extract::paginate_public(&md, opts.offset, max);
    let (kind, blocks) = if v.is_array() {
        (ContentKind::Forum, count_comments(&v))
    } else {
        (ContentKind::Listing, children_count(&v))
    };
    Some(Extracted {
        markdown: slice,
        title: title_of(&v),
        byline: None,
        published: None,
        site: Some("reddit".to_string()),
        total_chars: total,
        next_offset: next,
        blocks_total: blocks,
        blocks_shown: blocks,
        tokens_est: total / 4,
        thin: false,
        content_kind: kind,
        lang: "en".to_string(),
        quality: 0.95,
        pdf_pages: None,
        images: Vec::new(),
        fingerprint: None,
        via: Some("adapter:reddit-json"),
    })
}

fn title_of(v: &Value) -> Option<String> {
    if v.is_array() {
        v.pointer("/0/data/children/0/data/title")
            .and_then(Value::as_str)
            .map(String::from)
    } else {
        v.pointer("/data/children/0/data/subreddit")
            .and_then(Value::as_str)
            .map(|s| format!("r/{s}"))
    }
}

fn children_count(v: &Value) -> usize {
    v.pointer("/data/children")
        .and_then(Value::as_array)
        .map_or(0, std::vec::Vec::len)
}

fn count_comments(v: &Value) -> usize {
    v.get(1)
        .and_then(|l| l.pointer("/data/children"))
        .and_then(Value::as_array)
        .map_or(0, |arr| {
            arr.iter()
                .filter(|c| c.get("kind").and_then(Value::as_str) == Some("t1"))
                .count()
        })
}

// ── Thread ────────────────────────────────────────────────────

fn render_thread(post: &Value, comments: &Value) -> String {
    let mut md = String::new();
    let title = post.get("title").and_then(Value::as_str).unwrap_or("");
    let sub = post.get("subreddit").and_then(Value::as_str).unwrap_or("?");
    let author = post
        .get("author")
        .and_then(Value::as_str)
        .unwrap_or("[deleted]");
    let score = post.get("score").and_then(Value::as_i64).unwrap_or(0);
    let age = age_of(post.get("created_utc").and_then(Value::as_f64));
    let comments_n = post
        .get("num_comments")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    let flags: Vec<&str> = [
        post.get("over_18")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some("NSFW"),
        post.get("spoiler")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some("spoiler"),
        post.get("locked")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some("locked"),
        post.get("stickied")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some("sticky"),
        post.get("pinned")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some("pinned"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!(" · [{}]", flags.join(" "))
    };

    md.push_str(&format!("# {title}\n"));
    md.push_str(&format!(
        "u/{author} · {score} pts · {age} · {comments_n} comments · r/{sub}{flag_str}\n\n"
    ));

    if let Some(flair) = post.get("link_flair_text").and_then(Value::as_str)
        && !flair.is_empty()
    {
        md.push_str(&format!("*flair: {flair}*\n\n"));
    }

    // Link posts: the destination URL. Self posts: the body.
    let selftext = post.get("selftext").and_then(Value::as_str).unwrap_or("");
    if !selftext.is_empty() && selftext != "[removed]" && selftext != "[deleted]" {
        md.push_str(selftext.trim());
        md.push_str("\n\n---\n\n");
    } else if let Some(dest) = post.get("url").and_then(Value::as_str)
        && !dest.contains("reddit.com")
    {
        md.push_str(&format!("→ {dest}\n\n---\n\n"));
    }

    let mut rendered = 0usize;
    if let Some(children) = comments.as_array() {
        for child in children {
            render_comment(child, 0, &mut md, &mut rendered);
        }
    }
    if rendered >= MAX_COMMENTS {
        md.push_str(&format!(
            "*(showing {MAX_COMMENTS} of {comments_n} comments)*\n"
        ));
    }
    md
}

fn render_comment(node: &Value, depth: usize, md: &mut String, rendered: &mut usize) {
    if *rendered >= MAX_COMMENTS {
        return;
    }
    match node.get("kind").and_then(Value::as_str) {
        Some("t1") => {}
        Some("more") => {
            let n = node
                .pointer("/data/count")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if n > 0 {
                let indent = "  ".repeat(depth.min(MAX_DEPTH));
                md.push_str(&format!(
                    "{indent}*(+{n} more replies : deeper thread)*\n\n"
                ));
            }
            return;
        }
        _ => return,
    }
    let d = node.get("data").cloned().unwrap_or_default();
    let author = d
        .get("author")
        .and_then(Value::as_str)
        .unwrap_or("[deleted]");
    let score = d.get("score").and_then(Value::as_i64).unwrap_or(0);
    let age = age_of(d.get("created_utc").and_then(Value::as_f64));
    let body = d.get("body").and_then(Value::as_str).unwrap_or("");
    let is_op = d
        .get("is_submitter")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let stickied = d.get("stickied").and_then(Value::as_bool).unwrap_or(false);
    let controversial = d
        .get("controversiality")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        > 0;

    let indent = "  ".repeat(depth.min(MAX_DEPTH));
    let mut tags = String::new();
    if is_op {
        tags.push_str(" · OP");
    }
    if stickied {
        tags.push_str(" · sticky");
    }
    if controversial {
        tags.push_str(" · contested");
    }
    md.push_str(&format!(
        "{indent}**u/{author}** · {score} pts · {age}{tags}\n"
    ));

    if body.is_empty() || body == "[removed]" || body == "[deleted]" {
        md.push_str(&format!("{indent}[removed]\n\n"));
    } else {
        for line in body.lines() {
            md.push_str(&format!("{indent}{line}\n"));
        }
        md.push('\n');
    }
    *rendered += 1;

    // replies: "" (string) when there are none.
    if let Some(children) = d
        .pointer("/replies/data/children")
        .and_then(Value::as_array)
    {
        for child in children {
            render_comment(child, depth + 1, md, rendered);
        }
    }
}

// ── Listing ───────────────────────────────────────────────────

fn render_listing(children: &Value) -> String {
    let mut md = String::new();
    let mut n = 0usize;
    if let Some(posts) = children.as_array() {
        for post in posts {
            let d = match post.get("data") {
                Some(d) => d,
                None => continue,
            };
            if d.get("stickied").and_then(Value::as_bool).unwrap_or(false) && n > 0 {
                continue; // one sticky at top is enough context
            }
            let title = d.get("title").and_then(Value::as_str).unwrap_or("");
            if title.is_empty() {
                continue;
            }
            n += 1;
            let domain = d.get("domain").and_then(Value::as_str).unwrap_or("?");
            let score = d.get("score").and_then(Value::as_i64).unwrap_or(0);
            let author = d
                .get("author")
                .and_then(Value::as_str)
                .unwrap_or("[deleted]");
            let comments = d.get("num_comments").and_then(Value::as_i64).unwrap_or(0);
            let age = age_of(d.get("created_utc").and_then(Value::as_f64));
            let nsfw = d.get("over_18").and_then(Value::as_bool).unwrap_or(false);
            let sticky = d.get("stickied").and_then(Value::as_bool).unwrap_or(false);
            let prefix = if sticky {
                "[sticky] "
            } else if nsfw {
                "[NSFW] "
            } else {
                ""
            };
            let flair = d
                .get("link_flair_text")
                .and_then(Value::as_str)
                .filter(|f| !f.is_empty())
                .map(|f| format!(" · {f}"))
                .unwrap_or_default();
            md.push_str(&format!(
                "{n}. {prefix}**{title}** ({domain}) · {score} pts · u/{author} · {age} · {comments} comments{flair}\n\n"
            ));
            // Sticky announcements often carry the rules : first
            // 200 chars of the self-text.
            if sticky
                && let Some(st) = d.get("selftext").and_then(Value::as_str)
                && !st.is_empty()
            {
                let preview: String = st.chars().take(200).collect();
                md.push_str(&format!("   > {preview}\n\n"));
            }
        }
    }
    md
}

// ── helpers ───────────────────────────────────────────────────

/// created_utc → compact relative age ("now", "5m", "3h", "2d",
/// "1w", "3mo", "2y").
fn age_of(created: Option<f64>) -> String {
    let Some(t) = created else {
        return "?".to_string();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let secs = (now - t).max(0.0);
    match secs as u64 {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", secs as u64 / 60),
        3600..=86_399 => format!("{}h", secs as u64 / 3600),
        86_400..=604_799 => format!("{}d", secs as u64 / 86_400),
        604_800..=2_591_999 => format!("{}w", secs as u64 / 604_800),
        2_592_000..=31_535_999 => format!("{}mo", secs as u64 / 2_592_000),
        _ => format!("{}y", secs as u64 / 31_536_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ExtractOptions {
        ExtractOptions::default()
    }

    const THREAD: &str = r#"[
      {"kind":"Listing","data":{"children":[{"kind":"t3","data":{
        "title":"Why Rust is great","subreddit":"rust","author":"alice",
        "score":421,"created_utc":1755800000.0,"num_comments":42,
        "selftext":"Body text here","stickied":false,"over_18":false,
        "url":"https://example.com/self"}}]}},
      {"kind":"Listing","data":{"children":[
        {"kind":"t1","data":{"author":"bob","score":88,"created_utc":1755800100.0,
          "body":"First!","is_submitter":false,
          "replies":{"data":{"children":[
            {"kind":"t1","data":{"author":"alice","score":30,"created_utc":1755800200.0,
              "body":"Thanks","is_submitter":true,"replies":""}},
            {"kind":"more","data":{"count":7,"children":["x"]}}]}}}},
        {"kind":"t1","data":{"author":"carol","score":-2,"created_utc":1755800300.0,
          "body":"[removed]","replies":""}}
      ]}}
    ]"#;

    #[test]
    fn thread_renders() {
        let ex = extract(
            THREAD.as_bytes(),
            "https://old.reddit.com/r/rust/comments/abc/x.json",
            &opts(),
        )
        .expect("thread");
        assert_eq!(ex.via, Some("adapter:reddit-json"));
        assert!(ex.markdown.contains("# Why Rust is great"));
        assert!(ex.markdown.contains("u/alice"));
        assert!(ex.markdown.contains("Body text here"));
        assert!(ex.markdown.contains("First!"));
        // Nested reply indented, OP-tagged.
        assert!(ex.markdown.contains("u/alice** · 30 pts"));
        assert!(ex.markdown.contains("· OP"));
        // Removed comment.
        assert!(ex.markdown.contains("[removed]"));
        // "more" node → note, not a panic.
        assert!(ex.markdown.contains("+7 more replies"));
        assert_eq!(ex.content_kind, ContentKind::Forum);
    }

    #[test]
    fn listing_renders() {
        let listing = r#"{"kind":"Listing","data":{"children":[
          {"kind":"t3","data":{"title":"Sticky: rules","stickied":true,"domain":"self.rust","subreddit":"rust",
            "score":1,"author":"mods","num_comments":2,"created_utc":1755800000.0,
            "selftext":"Be nice. Read the FAQ first.","over_18":false}},
          {"kind":"t3","data":{"title":"Real post","domain":"example.com","score":99,
            "author":"dave","num_comments":5,"created_utc":1755800500.0,
            "over_18":false,"stickied":false,"selftext":""}}
        ]}}"#;
        let ex = extract(
            listing.as_bytes(),
            "https://old.reddit.com/r/rust.json",
            &opts(),
        )
        .expect("listing");
        assert!(ex.markdown.contains("Sticky: rules"));
        assert!(ex.markdown.contains("Be nice."));
        assert!(ex.markdown.contains("Real post"));
        assert!(ex.markdown.contains("99 pts"));
        assert_eq!(ex.content_kind, ContentKind::Listing);
        assert_eq!(ex.title.as_deref(), Some("r/rust"));
    }

    #[test]
    fn wrong_site_json_rejected() {
        assert!(
            extract(
                THREAD.as_bytes(),
                "https://registry.npmjs.org/react",
                &opts()
            )
            .is_none()
        );
    }

    #[test]
    fn garbage_rejected() {
        assert!(extract(b"not json", "https://old.reddit.com/r/rust.json", &opts()).is_none());
        assert!(extract(b"[]", "https://old.reddit.com/r/rust.json", &opts()).is_none());
    }
}
