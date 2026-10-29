//! Rich link previews (`com.apple.messages.URLBalloonProvider`).
//!
//! rustpush already carries the format: an incoming URL balloon is decoded
//! into `NormalMessage.link_meta` (`LinkMeta { data: LPLinkMetadata,
//! attachments }`), and a `NormalMessage` with `link_meta` set is sent as a
//! URL balloon (`messages.rs`, the `link_meta` branch of `Message::Message`):
//! a gzipped `BaseBalloonBody` whose `__payload__` is a keyed archive of
//! `RichLink { richLinkMetadata, richLinkIsPlaceholder: true }` and whose
//! `__attachments__` holds the preview image bytes inline. The image is
//! referenced from the metadata by `RichLinkImageAttachmentSubstitute
//! { MIMEType, richLinkImageAttachmentSubstituteIndex }`, an index into those
//! attachments. Nothing here touches MMCS for the image.
//!
//! This module adds: the `link` event, the preview-image cache behind
//! `download_link_image`, building a card for `send_link`, and an optional
//! OpenGraph fetch. OpenBubbles (Dart, not vendored here) fetches the page
//! itself and fills the same fields; that the fields below are the ones iOS
//! renders is inferred from rustpush's types. [VERIFY on a device]

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rustpush::{
    IndexedMessagePart, LPIconMetadata, LPImageMetadata, LPLinkMetadata, LinkMeta, Message, MessageInst,
    MessagePart, MessageParts, MessageType, NSURL, NormalMessage,
    RichLinkImageAttachmentSubstitute,
};
use serde_json::{json, Value as Json};

pub const URL_BALLOON: &str = "com.apple.messages.URLBalloonProvider";

/// Preview images of recent link messages: guid -> (mime, bytes).
static IMAGES: LazyLock<Mutex<(HashMap<String, (String, Vec<u8>)>, VecDeque<String>)>> =
    LazyLock::new(|| Mutex::new((HashMap::new(), VecDeque::new())));
const IMAGE_LIMIT: usize = 64;

fn remember_image(guid: &str, mime: String, bytes: Vec<u8>) {
    let Ok(mut g) = IMAGES.lock() else { return };
    let (map, order) = &mut *g;
    if map.contains_key(guid) {
        return;
    }
    while order.len() >= IMAGE_LIMIT {
        if let Some(old) = order.pop_front() {
            map.remove(&old);
        }
    }
    order.push_back(guid.to_string());
    map.insert(guid.to_string(), (mime, bytes));
}

/// The cached preview image for a link message, for `download_link_image`.
pub fn image_of(guid: &str) -> Option<(String, Vec<u8>)> {
    IMAGES.lock().ok()?.0.get(&guid.to_uppercase()).cloned()
        .or_else(|| IMAGES.lock().ok()?.0.get(guid).cloned())
}

fn url_string(u: &Option<NSURL>) -> Option<String> {
    u.clone().map(Into::<String>::into).filter(|s| !s.is_empty())
}

/// The `link` event for one inbound message, or None if it carries no URL
/// balloon. Caches the preview image as a side effect.
pub fn event_for(inst: &MessageInst) -> Option<Json> {
    let Message::Message(m) = &inst.message else { return None };
    let meta = m.link_meta.as_ref()?;
    let chat = serde_json::to_value(&inst.conversation).unwrap_or(Json::Null);
    let ev = from_meta(meta, &inst.id, inst.sender.as_deref(), chat);
    if let Some(img) = &meta.data.image {
        if let Some(bytes) = meta.attachments.get(img.rich_link_image_attachment_substitute_index as usize) {
            remember_image(&inst.id, img.mime_type.clone(), bytes.clone());
        }
    }
    Some(ev)
}

/// Pure part of `event_for`. `sender` is the Apple-verified envelope sender.
pub fn from_meta(meta: &LinkMeta, guid: &str, sender: Option<&str>, chat: Json) -> Json {
    let d = &meta.data;
    let image = d.image.as_ref().map(|img| {
        let idx = img.rich_link_image_attachment_substitute_index as usize;
        json!({
            "mime": img.mime_type,
            "index": idx,
            "bytes": meta.attachments.get(idx).map(|b| b.len()),
            "available": meta.attachments.get(idx).is_some(),
            "size": d.image_metadata.as_ref().map(|m| m.size.clone()),
            "source_url": d.image_metadata.as_ref().map(|m| Into::<String>::into(m.url.clone())),
        })
    });
    json!({
        "event": "link",
        "guid": guid,
        "sender": sender,
        "chat": chat,
        "url": url_string(&d.url).or_else(|| url_string(&d.original_url)),
        "original_url": url_string(&d.original_url),
        "title": d.title,
        "summary": d.summary,
        "site_name": d.site_name,
        "image": image,
    })
}

fn nsurl(s: &str) -> NSURL {
    // A keyed-archive NSURL with no base; rustpush reads "$null" back as "".
    NSURL { base: "$null".to_string(), relative: s.to_string() }
}

/// A preview image ready to embed.
pub struct CardImage {
    pub mime: String,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Where the image came from (og:image), if known.
    pub source_url: Option<String>,
}

impl CardImage {
    /// Accepts JPEG or PNG; anything else is rejected rather than sent as
    /// bytes iOS may not decode.
    pub fn from_bytes(bytes: Vec<u8>, source_url: Option<String>) -> Result<Self> {
        let fmt = image::guess_format(&bytes).context("unrecognised image format")?;
        let mime = match fmt {
            image::ImageFormat::Jpeg => "image/jpeg",
            image::ImageFormat::Png => "image/png",
            other => bail!("preview image must be JPEG or PNG, got {other:?}"),
        };
        let img = image::load_from_memory(&bytes).context("decoding preview image")?;
        Ok(Self { mime: mime.into(), width: img.width(), height: img.height(), bytes, source_url })
    }

    /// For a fetched site icon: JPEG or PNG as is, an ICO decoded (largest
    /// entry) and re-encoded as PNG. SVG and anything else is rejected.
    pub fn icon_from_bytes(bytes: Vec<u8>, source_url: Option<String>) -> Result<Self> {
        if image::guess_format(&bytes).ok() == Some(image::ImageFormat::Ico) {
            let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Ico)
                .context("decoding ICO icon")?;
            let mut out = std::io::Cursor::new(Vec::new());
            img.write_to(&mut out, image::ImageFormat::Png).context("re-encoding ICO as PNG")?;
            return Self::from_bytes(out.into_inner(), source_url);
        }
        Self::from_bytes(bytes, source_url)
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct Card {
    pub title: Option<String>,
    pub summary: Option<String>,
    pub site_name: Option<String>,
    pub image_url: Option<String>,
    /// Site icon candidates in preference order: `apple-touch-icon`, then
    /// `<link rel="icon">`, then `/favicon.ico`. Filled by `parse_og`.
    pub icon_urls: Vec<String>,
}

/// Build the link message. The text body is the URL itself, as iOS sends it,
/// so a client that ignores the balloon still shows a tappable link.
///
/// `image` fills the big preview slot and `icon` the small icon slot; each is
/// its own attachment, image at index 0 and icon after it (0 or 1). With
/// `compact` there is no big image: if no icon was given, the image moves to
/// the icon slot (iOS then draws the small card), otherwise it is dropped.
pub fn build(url: &str, card: &Card, image: Option<CardImage>, icon: Option<CardImage>, compact: bool) -> NormalMessage {
    let (image, icon) = match (compact, image, icon) {
        (true, img, None) => (None, img),
        (true, _, ic) => (None, ic),
        // iOS draws only the small card when an icon is present, so a full card with an
        // image carries no icon (verified on an iPhone 2026-10-28: the image was dropped).
        (false, Some(img), _) => (Some(img), None),
        (false, None, ic) => (None, ic),
    };
    let mut attachments: Vec<Vec<u8>> = Vec::new();
    let mut sub = |img: CardImage| {
        let s = RichLinkImageAttachmentSubstitute {
            mime_type: img.mime.clone(),
            rich_link_image_attachment_substitute_index: attachments.len() as _,
        };
        attachments.push(img.bytes);
        s
    };
    let (image_sub, image_meta) = match image {
        Some(img) => {
            let meta = LPImageMetadata {
                size: format!("{{{}, {}}}", img.width, img.height),
                url: nsurl(img.source_url.as_deref().unwrap_or(url)),
                version: 1,
            };
            (Some(sub(img)), Some(meta))
        }
        None => (None, None),
    };
    let (icon_sub, icon_meta) = match icon {
        Some(ic) => {
            let meta = LPIconMetadata { url: nsurl(ic.source_url.as_deref().unwrap_or(url)), version: 1 };
            (Some(sub(ic)), Some(meta))
        }
        None => (None, None),
    };
    let data = LPLinkMetadata {
        image_metadata: image_meta,
        version: 1,
        icon_metadata: icon_meta,
        original_url: Some(nsurl(url)),
        url: Some(nsurl(url)),
        title: card.title.clone(),
        summary: card.summary.clone(),
        site_name: card.site_name.clone(),
        image: image_sub,
        icon: icon_sub,
        images: None,
        icons: None,
        is_incomplete: None,
        uses_activity_pub: None,
        is_encoded_for_local_use: None,
        collaboration_type: None,
        specialization2: None,
    };
    let mut m = NormalMessage::new(String::new(), MessageType::IMessage);
    m.parts = MessageParts(vec![IndexedMessagePart {
        part: MessagePart::Text(url.to_string(), Default::default()),
        idx: None,
        ext: None,
    }]);
    m.link_meta = Some(LinkMeta { data, attachments });
    m
}

/// A plain-text message carrying just the URL: the fallback when a card
/// cannot be built.
pub fn plain(url: &str) -> NormalMessage {
    let mut m = NormalMessage::new(String::new(), MessageType::IMessage);
    m.parts = MessageParts(vec![IndexedMessagePart {
        part: MessagePart::Text(url.to_string(), Default::default()),
        idx: None,
        ext: None,
    }]);
    m
}

pub fn check_url(url: &str) -> Result<()> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) || url.len() < 10 {
        bail!("`url` must be an absolute http(s) URL");
    }
    if url.chars().any(|c| c.is_whitespace()) {
        bail!("`url` must not contain whitespace");
    }
    Ok(())
}

// ------------------------------------------------------------ OpenGraph

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HTML: usize = 512 * 1024;
const MAX_IMAGE: usize = 2 * 1024 * 1024;

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'")
        .replace("&#x27;", "'").replace("&lt;", "<").replace("&gt;", ">")
}

/// Value of attribute `name` inside one tag's text (`<meta ...>`).
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find(name) {
        let at = from + pos;
        from = at + name.len();
        // whole word, followed by optional spaces and '='
        if at > 0 && !lower.as_bytes()[at - 1].is_ascii_whitespace() {
            continue;
        }
        let rest = tag[at + name.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let (q, body) = match rest.chars().next() {
            Some(c @ ('"' | '\'')) => (Some(c), &rest[1..]),
            _ => (None, rest),
        };
        let end = match q {
            Some(c) => body.find(c),
            None => body.find(|c: char| c.is_whitespace() || c == '>' || c == '/'),
        }
        .unwrap_or(body.len());
        return Some(decode_entities(body[..end].trim()));
    }
    None
}

/// OpenGraph (and fallback `<title>` / `description`) from page HTML.
pub fn parse_og(html: &str, base: &str) -> Card {
    let mut card = Card::default();
    let mut desc = None;
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while let Some(p) = lower[i..].find("<meta") {
        let start = i + p;
        let end = lower[start..].find('>').map(|e| start + e).unwrap_or(lower.len());
        let tag = &html[start..end];
        i = end.max(start + 5);
        let key = attr(tag, "property").or_else(|| attr(tag, "name")).map(|k| k.to_ascii_lowercase());
        let Some(val) = attr(tag, "content").filter(|v| !v.is_empty()) else { continue };
        match key.as_deref() {
            Some("og:title") => card.title.get_or_insert(val),
            Some("og:description") => card.summary.get_or_insert(val),
            Some("og:site_name") => card.site_name.get_or_insert(val),
            Some("og:image") | Some("og:image:url") | Some("og:image:secure_url") => {
                card.image_url.get_or_insert(absolutize(&val, base))
            }
            Some("description") => desc.get_or_insert(val),
            _ => continue,
        };
    }
    if card.summary.is_none() {
        card.summary = desc;
    }
    let (mut touch, mut plain_icons) = (Vec::new(), Vec::new());
    let mut i = 0;
    while let Some(p) = lower[i..].find("<link") {
        let start = i + p;
        let end = lower[start..].find('>').map(|e| start + e).unwrap_or(lower.len());
        let tag = &html[start..end];
        i = end.max(start + 5);
        let (Some(rel), Some(href)) = (attr(tag, "rel"), attr(tag, "href").filter(|h| !h.is_empty())) else { continue };
        let rel = rel.to_ascii_lowercase();
        let rels: Vec<&str> = rel.split_whitespace().collect();
        let href = absolutize(&href, base);
        if rels.iter().any(|r| r.starts_with("apple-touch-icon")) {
            touch.push(href);
        } else if rels.contains(&"icon") {
            plain_icons.push(href);
        }
    }
    card.icon_urls = touch;
    card.icon_urls.extend(plain_icons);
    card.icon_urls.retain(|u| !u.to_ascii_lowercase().split(['?', '#']).next().unwrap_or("").ends_with(".svg"));
    let fav = absolutize("/favicon.ico", base);
    if !card.icon_urls.contains(&fav) {
        card.icon_urls.push(fav);
    }
    if card.title.is_none() {
        if let (Some(a), Some(b)) = (lower.find("<title"), lower.find("</title>")) {
            if let Some(gt) = lower[a..].find('>') {
                let s = a + gt + 1;
                if s < b {
                    let t = decode_entities(html[s..b].trim());
                    if !t.is_empty() {
                        card.title = Some(t);
                    }
                }
            }
        }
    }
    card
}

fn absolutize(u: &str, base: &str) -> String {
    if u.starts_with("http://") || u.starts_with("https://") {
        return u.to_string();
    }
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let host_end = base[scheme_end..].find('/').map(|i| scheme_end + i).unwrap_or(base.len());
    if let Some(rest) = u.strip_prefix("//") {
        return format!("{}{}", &base[..scheme_end], rest);
    }
    if u.starts_with('/') {
        return format!("{}{}", &base[..host_end], u);
    }
    let dir_end = base.rfind('/').filter(|&i| i >= host_end).map(|i| i + 1);
    match dir_end {
        Some(d) => format!("{}{}", &base[..d], u),
        None => format!("{}/{}", &base[..host_end], u),
    }
}

async fn get_capped(client: &reqwest::Client, url: &str, cap: usize) -> Result<Vec<u8>> {
    let mut resp = client.get(url).send().await.with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url}: HTTP {}", resp.status());
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.context("reading body")? {
        out.extend_from_slice(&chunk);
        if out.len() > cap {
            out.truncate(cap);
            break;
        }
    }
    Ok(out)
}

/// What `fetch` found: the parsed card, the og:image bytes, and the first
/// site icon that decoded (JPEG, PNG, or ICO re-encoded as PNG).
pub struct Fetched {
    pub card: Card,
    pub image: Option<Vec<u8>>,
    pub icon: Option<CardImage>,
}

/// Fetch the page and, if named, its og:image; with `want_icon`, also the
/// site icon, trying the first four of `Card::icon_urls` until one decodes.
/// SVG icons are skipped (the `image` crate cannot decode them). Every
/// request is bounded by a 5 s timeout and a size cap.
pub async fn fetch(url: &str, want_icon: bool) -> Result<Fetched> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(4))
        .user_agent("Mozilla/5.0 (compatible; ib-imessage link preview)")
        .build()
        .map_err(|e| anyhow!("building HTTP client: {e}"))?;
    let html = get_capped(&client, url, MAX_HTML).await?;
    let card = parse_og(&String::from_utf8_lossy(&html), url);
    let image = match &card.image_url {
        Some(u) => match get_capped(&client, u, MAX_IMAGE + 1).await {
            Ok(b) if b.len() <= MAX_IMAGE => Some(b),
            Ok(_) => { log::info!("og:image {u} over {MAX_IMAGE} bytes; skipped"); None }
            Err(e) => { log::info!("og:image {u}: {e:#}"); None }
        },
        None => None,
    };
    let mut icon = None;
    if want_icon {
        for u in card.icon_urls.iter().take(4) {
            match get_capped(&client, u, MAX_IMAGE + 1).await {
                Ok(b) if b.len() <= MAX_IMAGE => match CardImage::icon_from_bytes(b, Some(u.clone())) {
                    Ok(i) => { icon = Some(i); break; }
                    Err(e) => log::info!("icon {u}: {e:#}"),
                },
                Ok(_) => log::info!("icon {u} over {MAX_IMAGE} bytes; skipped"),
                Err(e) => log::info!("icon {u}: {e:#}"),
            }
        }
    }
    Ok(Fetched { card, image, icon })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// rustpush's private `RichLink` (rawmessages.rs), mirrored field for field.
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "camelCase", tag = "$class")]
    struct RichLink {
        rich_link_metadata: LPLinkMetadata,
        rich_link_is_placeholder: bool,
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::new(w, h);
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn build_then_parse_round_trip() {
        let card = Card {
            title: Some("2824 Gaul St".into()),
            summary: Some("Zoning summary".into()),
            site_name: Some("Atlas".into()),
            image_url: None,
            ..Default::default()
        };
        let img = CardImage::from_bytes(png(40, 20), Some("https://x.example/i.png".into())).unwrap();
        let m = build("https://atlas.phila.gov/2824", &card, Some(img), None, false);
        assert_eq!(m.parts.raw_text(), "https://atlas.phila.gov/2824");
        let lm = m.link_meta.as_ref().unwrap();
        assert_eq!(lm.attachments.len(), 1);
        assert_eq!(lm.data.image_metadata.as_ref().unwrap().size, "{40, 20}");
        let ev = from_meta(lm, "G1", Some("tel:+12155550100"), Json::Null);
        assert_eq!(ev["event"], "link");
        assert_eq!(ev["url"], "https://atlas.phila.gov/2824");
        assert_eq!(ev["title"], "2824 Gaul St");
        assert_eq!(ev["summary"], "Zoning summary");
        assert_eq!(ev["site_name"], "Atlas");
        assert_eq!(ev["sender"], "tel:+12155550100");
        assert_eq!(ev["image"]["mime"], "image/png");
        assert_eq!(ev["image"]["available"], true);
        assert_eq!(ev["image"]["source_url"], "https://x.example/i.png");
    }

    #[test]
    fn metadata_survives_keyed_archive() {
        // The same encoding rustpush applies on send (KeyedArchive of
        // RichLink), and the same expansion it applies on receive.
        let m = build("https://example.com/a", &Card { title: Some("T".into()), site_name: Some("S".into()), ..Default::default() }, None, None, false);
        let data = m.link_meta.unwrap().data;
        let v = plist::to_value(&RichLink { rich_link_metadata: data, rich_link_is_placeholder: true }).unwrap();
        let archived = rustpush::KeyedArchive::archive_item(v).unwrap();
        let mut bin = Vec::new();
        plist::to_writer_binary(&mut bin, &archived).unwrap();
        let back = rustpush::KeyedArchive::expand_root(&bin).unwrap();
        let rl: RichLink = plist::from_value(&back).unwrap();
        let d = rl.rich_link_metadata;
        assert_eq!(d.title.as_deref(), Some("T"));
        assert_eq!(d.site_name.as_deref(), Some("S"));
        assert_eq!(url_string(&d.url).as_deref(), Some("https://example.com/a"));
    }

    #[test]
    fn no_image_means_null() {
        let m = build("https://example.com", &Card::default(), None, None, false);
        let ev = from_meta(m.link_meta.as_ref().unwrap(), "G", None, Json::Null);
        assert!(ev["image"].is_null());
        assert!(ev["sender"].is_null());
    }

    #[test]
    fn rejects_non_image_and_gif() {
        assert!(CardImage::from_bytes(b"not an image".to_vec(), None).is_err());
        assert!(CardImage::from_bytes(b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec(), None).is_err());
    }

    #[test]
    fn og_parsing() {
        let html = r#"<html><head><title>Fallback &amp; title</title>
            <meta property="og:title" content="OG &quot;Title&quot;">
            <meta content='Desc here' property='og:description' />
            <meta name="description" content="plain desc">
            <meta property="og:site_name" content="Site">
            <meta property="og:image" content="/img/card.jpg"></head></html>"#;
        let c = parse_og(html, "https://example.com/path/page");
        assert_eq!(c.title.as_deref(), Some("OG \"Title\""));
        assert_eq!(c.summary.as_deref(), Some("Desc here"));
        assert_eq!(c.site_name.as_deref(), Some("Site"));
        assert_eq!(c.image_url.as_deref(), Some("https://example.com/img/card.jpg"));
        let c = parse_og("<title>Only</title><meta name=description content=d>", "https://e.com");
        assert_eq!(c.title.as_deref(), Some("Only"));
        assert_eq!(c.summary.as_deref(), Some("d"));
    }

    #[test]
    fn absolutize_forms() {
        assert_eq!(absolutize("//cdn.x/i.png", "https://a.com/b"), "https://cdn.x/i.png");
        assert_eq!(absolutize("i.png", "https://a.com/b/c"), "https://a.com/b/i.png");
        assert_eq!(absolutize("i.png", "https://a.com"), "https://a.com/i.png");
    }

    fn ico(w: u32) -> Vec<u8> {
        let img = image::RgbaImage::new(w, w);
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Ico).unwrap();
        out.into_inner()
    }

    #[test]
    fn full_card_with_image_drops_the_icon() {
        // iOS shows only the small card when an icon is present; the image must win.
        let img = CardImage::from_bytes(png(40, 20), None).unwrap();
        let ic = CardImage::from_bytes(png(16, 16), Some("https://e.com/i.png".into())).unwrap();
        let m = build("https://e.com", &Card::default(), Some(img), Some(ic), false);
        let lm = m.link_meta.unwrap();
        assert_eq!(lm.attachments.len(), 1);
        assert_eq!(lm.data.image.as_ref().unwrap().rich_link_image_attachment_substitute_index, 0);
        assert!(lm.data.image_metadata.is_some());
        assert!(lm.data.icon.is_none() && lm.data.icon_metadata.is_none());
    }

    #[test]
    fn icon_alone_is_index_zero() {
        let ic = CardImage::from_bytes(png(16, 16), None).unwrap();
        let lm = build("https://e.com", &Card::default(), None, Some(ic), false).link_meta.unwrap();
        assert_eq!(lm.attachments.len(), 1);
        assert!(lm.data.image.is_none());
        assert_eq!(lm.data.icon.unwrap().rich_link_image_attachment_substitute_index, 0);
    }

    #[test]
    fn compact_moves_lone_image_to_icon() {
        let img = CardImage::from_bytes(png(40, 20), None).unwrap();
        let lm = build("https://e.com", &Card::default(), Some(img), None, true).link_meta.unwrap();
        assert_eq!(lm.attachments.len(), 1);
        assert!(lm.data.image.is_none() && lm.data.image_metadata.is_none());
        assert_eq!(lm.data.icon.unwrap().rich_link_image_attachment_substitute_index, 0);
    }

    #[test]
    fn compact_with_icon_drops_image() {
        let img = CardImage::from_bytes(png(40, 20), None).unwrap();
        let ic = CardImage::from_bytes(png(16, 16), None).unwrap();
        let lm = build("https://e.com", &Card::default(), Some(img), Some(ic), true).link_meta.unwrap();
        assert_eq!(lm.attachments.len(), 1);
        assert!(lm.data.image.is_none());
        assert_eq!(lm.attachments[0], png(16, 16));
    }

    #[test]
    fn icon_links_parsed_in_order() {
        let html = r#"<link rel="icon" href="/f.png"><link rel="apple-touch-icon" href="/t.png">
            <link rel="icon" type="image/svg+xml" href="/f.svg"><link rel="stylesheet" href="/s.css">"#;
        let c = parse_og(html, "https://e.com/a/b");
        assert_eq!(c.icon_urls, vec!["https://e.com/t.png", "https://e.com/f.png", "https://e.com/favicon.ico"]);
        assert_eq!(parse_og("", "https://e.com").icon_urls, vec!["https://e.com/favicon.ico"]);
    }

    #[test]
    fn ico_becomes_png_svg_refused() {
        let i = CardImage::icon_from_bytes(ico(32), None).unwrap();
        assert_eq!(i.mime, "image/png");
        assert_eq!((i.width, i.height), (32, 32));
        assert!(CardImage::from_bytes(ico(32), None).is_err());
        assert!(CardImage::icon_from_bytes(b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(), None).is_err());
    }

    #[test]
    fn url_checks() {
        assert!(check_url("https://example.com").is_ok());
        assert!(check_url("ftp://example.com").is_err());
        assert!(check_url("https://exa mple.com").is_err());
    }
}
