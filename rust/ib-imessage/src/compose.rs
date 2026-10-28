//! Building message bodies: formatted text runs, mentions, and the mime/UTI
//! guesses attachments need.
//!
//! Kept apart from `main.rs` because none of it touches the network; it turns
//! the wire request into rustpush `MessagePart`s and can be reasoned about (and
//! tested) on its own.

use anyhow::{bail, Result};
use serde::Deserialize;

use rustpush::{IndexedMessagePart, MessagePart, TextEffect, TextFlags, TextFormat};

/// One formatting range on `Send.text`.
///
/// `start` and `len` are in **UTF-16 code units**, the unit iOS (NSString)
/// and JavaScript string indices use — not bytes and not Unicode scalar
/// values. An emoji outside the BMP counts as 2. A range edge that falls
/// between the two halves of a surrogate pair is rejected.
#[derive(Deserialize, Clone, Debug)]
pub struct FormatRange {
    pub start: usize,
    pub len: usize,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underline: bool,
    #[serde(default)]
    pub strikethrough: bool,
    /// big | small | shake | nod | explode | ripple | bloom | jitter
    #[serde(default)]
    pub effect: Option<String>,
}

/// One mention on `Send.text`. Same UTF-16 units as `FormatRange`. The
/// mentioned span of `text` is what the recipient sees (usually the person's
/// name); `handle` is who it points at.
#[derive(Deserialize, Clone, Debug)]
pub struct MentionRange {
    pub start: usize,
    pub len: usize,
    /// Full handle: `tel:+12155550100` or `mailto:someone@example.com`.
    pub handle: String,
}

fn parse_effect(name: &str) -> Result<TextEffect> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "big" => TextEffect::Big,
        "small" => TextEffect::Small,
        "shake" => TextEffect::Shake,
        "nod" => TextEffect::Nod,
        "explode" => TextEffect::Explode,
        "ripple" => TextEffect::Ripple,
        "bloom" => TextEffect::Bloom,
        "jitter" => TextEffect::Jitter,
        other => bail!(
            "unknown text effect {other:?}; expected big/small/shake/nod/\
             explode/ripple/bloom/jitter"
        ),
    })
}

/// UTF-16 offset -> byte offset, for every offset that sits on a char
/// boundary. Index `i` holds the byte offset of UTF-16 unit `i`, or `None`
/// when unit `i` is the low half of a surrogate pair.
fn utf16_to_byte_map(text: &str) -> Vec<Option<usize>> {
    let mut map = Vec::with_capacity(text.len() + 1);
    for (byte, ch) in text.char_indices() {
        map.push(Some(byte));
        if ch.len_utf16() == 2 {
            map.push(None);
        }
    }
    map.push(Some(text.len()));
    map
}

/// Split `text` into the parts iOS 18 renders: plain and formatted
/// `MessagePart::Text` runs and `MessagePart::Mention`s, split at every range
/// boundary.
///
/// rustpush's `TextFormat` is *either* a set of flags *or* one effect, so a
/// span covered by both a flag and an effect cannot be expressed and is
/// rejected rather than silently dropping one. Mentions carry no formatting;
/// a format range that overlaps a mention is likewise rejected. Mentions may
/// not overlap each other.
pub fn text_parts(
    text: &str,
    format: &[FormatRange],
    mentions: &[MentionRange],
) -> Result<Vec<IndexedMessagePart>> {
    let map = utf16_to_byte_map(text);
    let total = map.len() - 1;

    let check = |what: &str, start: usize, len: usize| -> Result<()> {
        if len == 0 {
            bail!("{what} range at {start} has len 0");
        }
        let end = start
            .checked_add(len)
            .filter(|e| *e <= total)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{what} range {start}+{len} runs past the end of text \
                     ({total} UTF-16 units)"
                )
            })?;
        if map[start].is_none() || map[end].is_none() {
            bail!("{what} range {start}+{len} splits a surrogate pair");
        }
        Ok(())
    };

    let mut effects = Vec::with_capacity(format.len());
    for f in format {
        check("format", f.start, f.len)?;
        effects.push(f.effect.as_deref().map(parse_effect).transpose()?);
        if !(f.bold || f.italic || f.underline || f.strikethrough || f.effect.is_some()) {
            bail!("format range {}+{} sets nothing", f.start, f.len);
        }
    }
    for m in mentions {
        check("mention", m.start, m.len)?;
        if !(m.handle.starts_with("tel:") || m.handle.starts_with("mailto:")) {
            bail!(
                "mention handle {:?} must start with tel: or mailto:",
                m.handle
            );
        }
    }
    let mut sorted: Vec<&MentionRange> = mentions.iter().collect();
    sorted.sort_by_key(|m| m.start);
    for w in sorted.windows(2) {
        if w[0].start + w[0].len > w[1].start {
            bail!("mentions at {} and {} overlap", w[0].start, w[1].start);
        }
    }
    for f in format {
        for m in mentions {
            if f.start < m.start + m.len && m.start < f.start + f.len {
                bail!(
                    "format range {}+{} overlaps mention {}+{}; mentions \
                     cannot be formatted",
                    f.start, f.len, m.start, m.len
                );
            }
        }
    }

    // Every edge of every range, plus the ends of the text.
    let mut cuts: Vec<usize> = vec![0, total];
    for f in format {
        cuts.extend([f.start, f.start + f.len]);
    }
    for m in mentions {
        cuts.extend([m.start, m.start + m.len]);
    }
    cuts.sort_unstable();
    cuts.dedup();

    let mut parts = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let slice = &text[map[a].unwrap()..map[b].unwrap()];
        if slice.is_empty() {
            continue;
        }
        let part = if let Some(m) = mentions.iter().find(|m| m.start <= a && b <= m.start + m.len) {
            MessagePart::Mention(m.handle.clone(), slice.to_string())
        } else {
            let mut flags = TextFlags::default();
            let mut effect: Option<TextEffect> = None;
            for (f, e) in format.iter().zip(&effects) {
                if f.start <= a && b <= f.start + f.len {
                    flags.bold |= f.bold;
                    flags.italic |= f.italic;
                    flags.underline |= f.underline;
                    flags.strikethrough |= f.strikethrough;
                    if let Some(e) = e {
                        if effect.is_some() {
                            bail!("two text effects overlap at UTF-16 {a}..{b}");
                        }
                        effect = Some(*e);
                    }
                }
            }
            let any_flag = flags.bold || flags.italic || flags.underline || flags.strikethrough;
            let fmt = match (effect, any_flag) {
                (Some(_), true) => bail!(
                    "UTF-16 {a}..{b} has both a text effect and bold/italic/\
                     underline/strikethrough; iMessage (and rustpush's \
                     TextFormat) carries one or the other per run"
                ),
                (Some(e), false) => TextFormat::Effect(e),
                (None, _) => TextFormat::Flags(flags),
            };
            MessagePart::Text(slice.to_string(), fmt)
        };
        parts.push(IndexedMessagePart { part, idx: None, ext: None });
    }
    Ok(parts)
}

/// (mime, UTI) for a file extension. Unknown extensions — and CAD files,
/// which iOS has no viewer for — go as generic data.
pub fn mime_uti(ext: &str) -> (&'static str, &'static str) {
    match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => ("image/jpeg", "public.jpeg"),
        "png" => ("image/png", "public.png"),
        "gif" => ("image/gif", "com.compuserve.gif"),
        "heic" => ("image/heic", "public.heic"),
        "heif" => ("image/heif", "public.heif"),
        "webp" => ("image/webp", "org.webmproject.webp"),
        "tif" | "tiff" => ("image/tiff", "public.tiff"),
        "bmp" => ("image/bmp", "com.microsoft.bmp"),
        "pdf" => ("application/pdf", "com.adobe.pdf"),
        "mov" => ("video/quicktime", "com.apple.quicktime-movie"),
        "mp4" => ("video/mp4", "public.mpeg-4"),
        "m4v" => ("video/x-m4v", "com.apple.m4v-video"),
        "caf" => ("audio/x-caf", "com.apple.coreaudio-format"),
        "m4a" => ("audio/mp4", "com.apple.m4a-audio"),
        "mp3" => ("audio/mpeg", "public.mp3"),
        "wav" => ("audio/wav", "com.microsoft.waveform-audio"),
        "aac" => ("audio/aac", "public.aac-audio"),
        "aif" | "aiff" => ("audio/aiff", "public.aiff-audio"),
        "txt" => ("text/plain", "public.plain-text"),
        "csv" => ("text/csv", "public.comma-separated-values-text"),
        "zip" => ("application/zip", "public.zip-archive"),
        "vcf" => ("text/vcard", "public.vcard"),
        "docx" => (
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "org.openxmlformats.wordprocessingml.document",
        ),
        "xlsx" => (
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "org.openxmlformats.spreadsheetml.sheet",
        ),
        // DWG/DXF deliberately generic: no iOS UTI renders them.
        "dwg" | "dxf" => ("application/octet-stream", "public.data"),
        _ => ("application/octet-stream", "public.data"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(parts: &[IndexedMessagePart]) -> Vec<String> {
        parts
            .iter()
            .map(|p| match &p.part {
                MessagePart::Text(t, TextFormat::Flags(f)) => {
                    format!("{t}|{}{}", if f.bold { "b" } else { "" }, if f.italic { "i" } else { "" })
                }
                MessagePart::Text(t, TextFormat::Effect(e)) => format!("{t}|fx{}", *e as u32),
                MessagePart::Mention(u, t) => format!("{t}@{u}"),
                _ => "?".into(),
            })
            .collect()
    }

    #[test]
    fn splits_at_boundaries() {
        let f = vec![FormatRange { start: 6, len: 5, bold: true, italic: false, underline: false, strikethrough: false, effect: None }];
        let p = text_parts("hello world!", &f, &[]).unwrap();
        assert_eq!(texts(&p), vec!["hello |", "world|b", "!|"]);
    }

    #[test]
    fn utf16_counts_surrogates() {
        // "😀" is 2 UTF-16 units.
        let f = vec![FormatRange { start: 2, len: 2, bold: false, italic: true, underline: false, strikethrough: false, effect: None }];
        let p = text_parts("😀hi", &f, &[]).unwrap();
        assert_eq!(texts(&p), vec!["😀|", "hi|i"]);
        let bad = vec![FormatRange { start: 1, len: 1, bold: true, italic: false, underline: false, strikethrough: false, effect: None }];
        assert!(text_parts("😀hi", &bad, &[]).is_err());
    }

    #[test]
    fn mentions_and_effects() {
        let m = vec![MentionRange { start: 0, len: 5, handle: "tel:+12155550100".into() }];
        let f = vec![FormatRange { start: 6, len: 4, bold: false, italic: false, underline: false, strikethrough: false, effect: Some("shake".into()) }];
        let p = text_parts("Gerry look", &f, &m).unwrap();
        assert_eq!(texts(&p), vec!["Gerry@tel:+12155550100", " |", "look|fx9"]);
    }

    #[test]
    fn rejects_flag_plus_effect() {
        let f = vec![FormatRange { start: 0, len: 2, bold: true, italic: false, underline: false, strikethrough: false, effect: Some("big".into()) }];
        assert!(text_parts("hi", &f, &[]).is_err());
    }
}
