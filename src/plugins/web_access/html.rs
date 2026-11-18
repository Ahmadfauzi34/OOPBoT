use serde_json::{json, Value};

pub(crate) fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

pub(crate) fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn decode_html_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        if let Some(semi) = after[..after.len().min(12)].find(';') {
            let entity = &after[1..semi];
            if let Some(decoded) = decode_single_entity(entity) {
                out.push(decoded);
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &rest[amp + 1..];
    }
    out.push_str(rest);
    out
}

fn decode_single_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" | "#x27" => Some('\''),
        "nbsp" => Some(' '),
        "ndash" => Some('–'),
        "mdash" => Some('—'),
        "hellip" => Some('…'),
        "laquo" => Some('«'),
        "raquo" => Some('»'),
        _ if entity.starts_with("#x") || entity.starts_with("#X") => {
            u32::from_str_radix(&entity[2..], 16).ok().and_then(char::from_u32)
        }
        _ if entity.starts_with('#') => entity[1..].parse::<u32>().ok().and_then(char::from_u32),
        _ => None,
    }
}

/// Menghapus tag HTML serta blok non-konten (`<script>`, `<style>`, `<noscript>`, `<svg>`, komentar HTML),
/// lalu men-decode entitas HTML dan merapikan spasi/baris kosong berlebih.
pub(crate) fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'<' {
            if lower[i..].starts_with("<!--") {
                if let Some(end) = lower[i + 4..].find("-->") {
                    i += 4 + end + 3;
                    continue;
                }
            }
            let mut skipped_block = false;
            for tag in ["script", "style", "noscript", "svg"] {
                if starts_with_open_tag(&lower[i..], tag) {
                    let close = format!("</{}>", tag);
                    if let Some(end) = lower[i..].find(&close) {
                        i += end + close.len();
                    } else {
                        i = bytes.len();
                    }
                    out.push(' ');
                    skipped_block = true;
                    break;
                }
            }
            if skipped_block {
                continue;
            }
            if let Some(gt) = html[i..].find('>') {
                i += gt + 1;
                out.push(' ');
                continue;
            } else {
                break;
            }
        } else if let Some(ch) = html[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }

    let decoded = decode_html_entities(&out);
    let mut cleaned = String::new();
    let mut last_empty = false;
    for line in decoded.lines() {
        let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            if !last_empty && !cleaned.is_empty() {
                cleaned.push('\n');
                last_empty = true;
            }
        } else {
            cleaned.push_str(&collapsed);
            cleaned.push('\n');
            last_empty = false;
        }
    }
    cleaned.trim().to_string()
}

fn starts_with_open_tag(slice_lower: &str, tag: &str) -> bool {
    if !slice_lower.starts_with('<') {
        return false;
    }
    let after = &slice_lower[1..];
    if let Some(rest) = after.strip_prefix(tag) {
        matches!(
            rest.as_bytes().first(),
            Some(b'>' | b' ' | b'\t' | b'\r' | b'\n' | b'/')
        )
    } else {
        false
    }
}

/// Mendeteksi apakah DuckDuckGo HTML mengembalikan halaman anti-bot / challenge (misal HTTP 202 anomaly-modal).
pub(crate) fn is_ddg_challenge(status: u16, body: &str) -> bool {
    if matches!(status, 202 | 403 | 429) {
        return true;
    }
    let lower = body.to_ascii_lowercase();
    lower.contains("anomaly-modal")
        || lower.contains("challenge-form")
        || lower.contains("bots use duckduckgo")
        || lower.contains("select all squares containing")
}

pub(crate) fn parse_ddg_results(html: &str, limit: usize) -> Vec<Value> {
    let mut results = Vec::new();
    let mut remaining = html;
    while let Some(start) = remaining.find("result__body\"") {
        let chunk = &remaining[start..];
        let end = chunk.find("class=\"clear\"").unwrap_or_else(|| chunk.find("</div>").unwrap_or(chunk.len()));
        let block = &chunk[..end];

        let title = strip_html_tags(&extract_tag_inner(block, "result__a"));
        let title = if title.is_empty() {
            strip_html_tags(&extract_tag_inner(block, "result__title"))
        } else {
            title
        };
        let snippet = strip_html_tags(&extract_tag_inner(block, "result__snippet"));
        let url = extract_href(block).unwrap_or_default();

        if (!title.is_empty() || !snippet.is_empty()) && !url.is_empty() {
            results.push(json!({
                "title": title,
                "snippet": snippet,
                "url": url,
                "source": "duckduckgo"
            }));
            if results.len() >= limit {
                break;
            }
        }
        let advance = end.max(1).min(chunk.len());
        remaining = &chunk[advance..];
    }
    results
}

fn extract_tag_inner(chunk: &str, class_marker: &str) -> String {
    if let Some(pos) = chunk.find(class_marker) {
        let sub = &chunk[pos..];
        if let Some(gt) = sub.find('>') {
            let inner_start = &sub[gt + 1..];
            if let Some(lt) = inner_start.find("</") {
                return inner_start[..lt].to_string();
            }
        }
    }
    String::new()
}

pub(crate) fn extract_href(chunk: &str) -> Option<String> {
    let href_pos = chunk.find("href=\"")?;
    let after = &chunk[href_pos + 6..];
    let end_quote = after.find('"')?;
    let raw_url = decode_html_entities(&after[..end_quote]);

    if let Some(uddg_pos) = raw_url.find("uddg=") {
        let encoded = &raw_url[uddg_pos + 5..];
        let end = encoded.find('&').unwrap_or(encoded.len());
        let decoded = urldecode(&encoded[..end]);
        if !decoded.is_empty() {
            return Some(decoded);
        }
    }
    if raw_url.starts_with("//") {
        Some(format!("https:{}", raw_url))
    } else {
        Some(raw_url)
    }
}
