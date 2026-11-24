//! Pure-Rust PDF attachments -> compact Markdown (zero Python dependency).
//! Extracts text from uncompressed and FlateDecode PDF streams, caches by SHA-1
//! hash next to the PDF, caps memory usage against zip bombs, and never panics.
use flate2::read::{DeflateDecoder, ZlibDecoder};
use sha1_smol::Sha1;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_PDF_BYTES: usize = 50 * 1024 * 1024;
const MAX_STREAM_INFLATED: u64 = 8 * 1024 * 1024;
const MAX_MD_CHARS: usize = 400_000;
pub(crate) const MAX_INLINE_FILE_BYTES: u64 = 2 * 1024 * 1024;

fn cache_dir_for(pdf: &Path) -> Result<PathBuf, String> {
    let parent = pdf
        .parent()
        .ok_or_else(|| "Invalid PDF path.".to_string())?;
    Ok(if parent.as_os_str().is_empty() {
        PathBuf::from(".md-cache")
    } else {
        parent.join(".md-cache")
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() || haystack.len() - from < needle.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

fn inflate_stream(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut z = ZlibDecoder::new(raw).take(MAX_STREAM_INFLATED);
    if z.read_to_end(&mut out).is_ok() && !out.is_empty() {
        return Some(out);
    }
    out.clear();
    let mut d = DeflateDecoder::new(raw).take(MAX_STREAM_INFLATED);
    if d.read_to_end(&mut out).is_ok() && !out.is_empty() {
        return Some(out);
    }
    None
}

fn parse_to_unicode_cmap(stream: &[u8], map: &mut HashMap<u16, String>) {
    let Ok(txt) = std::str::from_utf8(stream) else {
        return;
    };
    if !txt.contains("beginbfchar") && !txt.contains("beginbfrange") {
        return;
    }
    let mut in_char = false;
    let mut in_range = false;
    for raw_line in txt.lines() {
        let line = raw_line.trim();
        if line.ends_with("beginbfchar") {
            in_char = true;
            continue;
        }
        if line == "endbfchar" {
            in_char = false;
            continue;
        }
        if line.ends_with("beginbfrange") {
            in_range = true;
            continue;
        }
        if line == "endbfrange" {
            in_range = false;
            continue;
        }
        let hexes: Vec<&str> = line
            .split('<')
            .skip(1)
            .filter_map(|part| part.split('>').next())
            .collect();
        if in_char && hexes.len() >= 2 {
            if let Ok(code) = u16::from_str_radix(hexes[0], 16) {
                if let Some(decoded) = decode_utf16_hex(hexes[1]) {
                    map.insert(code, decoded);
                }
            }
        } else if in_range && hexes.len() >= 3 {
            if let (Ok(lo), Ok(hi)) = (
                u16::from_str_radix(hexes[0], 16),
                u16::from_str_radix(hexes[1], 16),
            ) {
                if hi >= lo && (hi - lo) <= 256 {
                    if let Ok(mut dst) = u32::from_str_radix(hexes[2], 16) {
                        for code in lo..=hi {
                            if let Some(ch) = char::from_u32(dst) {
                                map.insert(code, ch.to_string());
                            }
                            dst = dst.saturating_add(1);
                        }
                    }
                }
            }
        }
    }
}

fn decode_utf16_hex(hex: &str) -> Option<String> {
    let bytes = hex_to_bytes(hex);
    if bytes.is_empty() {
        return None;
    }
    if bytes.len().is_multiple_of(2) {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        Some(String::from_utf16_lossy(&units))
    } else {
        Some(bytes.into_iter().map(winansi_char).collect())
    }
}

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    let clean: Vec<u8> = hex.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
    let mut out = Vec::with_capacity(clean.len().div_ceil(2));
    let mut i = 0;
    while i < clean.len() {
        let hi = hex_val(clean[i]);
        let lo = if i + 1 < clean.len() {
            hex_val(clean[i + 1])
        } else {
            0
        };
        out.push((hi << 4) | lo);
        i += 2;
    }
    out
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

fn winansi_char(b: u8) -> char {
    match b {
        0x80 => '€',
        0x85 => '…',
        0x91 => '‘',
        0x92 => '’',
        0x93 => '“',
        0x94 => '”',
        0x95 => '•',
        0x96 => '–',
        0x97 => '—',
        _ => b as char,
    }
}

fn decode_pdf_bytes(bytes: &[u8], cmap: &HashMap<u16, String>) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if !cmap.is_empty() && bytes.len() >= 2 && bytes.len().is_multiple_of(2) {
        let mut mapped = String::new();
        let mut hits = 0usize;
        for c in bytes.as_chunks::<2>().0 {
            let code = u16::from_be_bytes([c[0], c[1]]);
            if let Some(s) = cmap.get(&code) {
                mapped.push_str(s);
                hits += 1;
            }
        }
        if hits > 0 && hits * 2 >= bytes.len() / 2 {
            return mapped;
        }
    }
    if !cmap.is_empty() {
        let mut mapped = String::new();
        let mut hits = 0usize;
        for &b in bytes {
            if let Some(s) = cmap.get(&(b as u16)) {
                mapped.push_str(s);
                hits += 1;
            } else {
                mapped.push(winansi_char(b));
            }
        }
        if hits > 0 {
            return mapped;
        }
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    bytes.iter().copied().map(winansi_char).collect()
}

fn parse_literal_string(data: &[u8], start: usize) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut i = start + 1; // skip '('
    let mut depth = 1usize;
    while i < data.len() && depth > 0 {
        match data[i] {
            b'\\' if i + 1 < data.len() => {
                i += 1;
                match data[i] {
                    b'n' => {
                        out.push(b'\n');
                        i += 1;
                    }
                    b'r' => {
                        out.push(b'\r');
                        i += 1;
                    }
                    b't' => {
                        out.push(b'\t');
                        i += 1;
                    }
                    b'b' => {
                        out.push(0x08);
                        i += 1;
                    }
                    b'f' => {
                        out.push(0x0C);
                        i += 1;
                    }
                    b'(' | b')' | b'\\' => {
                        out.push(data[i]);
                        i += 1;
                    }
                    b'\r' => {
                        i += 1;
                        if i < data.len() && data[i] == b'\n' {
                            i += 1;
                        }
                    }
                    b'\n' => {
                        i += 1;
                    }
                    b'0'..=b'7' => {
                        let mut val = (data[i] - b'0') as u32;
                        i += 1;
                        for _ in 0..2 {
                            if i < data.len() && (b'0'..=b'7').contains(&data[i]) {
                                val = val * 8 + (data[i] - b'0') as u32;
                                i += 1;
                            } else {
                                break;
                            }
                        }
                        out.push((val & 0xFF) as u8);
                    }
                    other => {
                        out.push(other);
                        i += 1;
                    }
                }
            }
            b'(' => {
                depth += 1;
                out.push(b'(');
                i += 1;
            }
            b')' => {
                depth -= 1;
                if depth > 0 {
                    out.push(b')');
                }
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    (out, i)
}

fn extract_text_from_content(data: &[u8], cmap: &HashMap<u16, String>, out: &mut String) {
    let mut i = 0usize;
    let mut in_text = false;
    let mut pending_str: Option<String> = None;
    let mut pending_arr: Option<String> = None;
    let mut nums: Vec<f64> = Vec::new();
    let len = data.len();

    while i < len && out.len() < MAX_MD_CHARS {
        let b = data[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'%' {
            while i < len && data[i] != b'\n' && data[i] != b'\r' {
                i += 1;
            }
            continue;
        }
        if b == b'(' {
            let (raw, next) = parse_literal_string(data, i);
            if in_text {
                pending_str = Some(decode_pdf_bytes(&raw, cmap));
            }
            i = next;
            continue;
        }
        if b == b'<' {
            if i + 1 < len && data[i + 1] == b'<' {
                i += 2;
                continue;
            }
            let end = data[i + 1..]
                .iter()
                .position(|&c| c == b'>')
                .map(|p| i + 1 + p)
                .unwrap_or(len);
            if in_text {
                let hex = String::from_utf8_lossy(&data[i + 1..end]);
                let raw = hex_to_bytes(&hex);
                pending_str = Some(decode_pdf_bytes(&raw, cmap));
            }
            i = (end + 1).min(len);
            continue;
        }
        if b == b'[' {
            i += 1;
            let mut acc = String::new();
            while i < len && data[i] != b']' {
                match data[i] {
                    b'(' => {
                        let (raw, next) = parse_literal_string(data, i);
                        if in_text {
                            acc.push_str(&decode_pdf_bytes(&raw, cmap));
                        }
                        i = next;
                    }
                    b'<' if i + 1 < len && data[i + 1] != b'<' => {
                        let end = data[i + 1..]
                            .iter()
                            .position(|&c| c == b'>')
                            .map(|p| i + 1 + p)
                            .unwrap_or(len);
                        if in_text {
                            let hex = String::from_utf8_lossy(&data[i + 1..end]);
                            let raw = hex_to_bytes(&hex);
                            acc.push_str(&decode_pdf_bytes(&raw, cmap));
                        }
                        i = (end + 1).min(len);
                    }
                    b'-' | b'+' | b'0'..=b'9' | b'.' => {
                        let start = i;
                        i += 1;
                        while i < len && (data[i].is_ascii_digit() || data[i] == b'.') {
                            i += 1;
                        }
                        if in_text {
                            if let Ok(kern) = std::str::from_utf8(&data[start..i])
                                .unwrap_or("")
                                .parse::<f64>()
                            {
                                if kern <= -120.0 && !acc.ends_with(' ') && !acc.is_empty() {
                                    acc.push(' ');
                                }
                            }
                        }
                    }
                    _ => i += 1,
                }
            }
            if i < len && data[i] == b']' {
                i += 1;
            }
            if in_text {
                pending_arr = Some(acc);
            }
            continue;
        }
        if b == b'/' {
            i += 1;
            while i < len
                && !data[i].is_ascii_whitespace()
                && !matches!(data[i], b'/' | b'[' | b']' | b'(' | b')' | b'<' | b'>')
            {
                i += 1;
            }
            continue;
        }
        if matches!(b, b'-' | b'+' | b'0'..=b'9' | b'.') {
            let start = i;
            i += 1;
            while i < len && (data[i].is_ascii_digit() || data[i] == b'.') {
                i += 1;
            }
            if let Ok(n) = std::str::from_utf8(&data[start..i])
                .unwrap_or("")
                .parse::<f64>()
            {
                nums.push(n);
                if nums.len() > 8 {
                    nums.remove(0);
                }
            }
            continue;
        }
        let start = i;
        while i < len
            && !data[i].is_ascii_whitespace()
            && !matches!(
                data[i],
                b'/' | b'[' | b']' | b'(' | b')' | b'<' | b'>' | b'%'
            )
        {
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        let op = &data[start..i];
        match op {
            b"BT" => {
                in_text = true;
                pending_str = None;
                pending_arr = None;
                nums.clear();
            }
            b"ET" => {
                in_text = false;
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                pending_str = None;
                pending_arr = None;
                nums.clear();
            }
            b"Tj" | b"'" | b"\"" if in_text => {
                if op != b"Tj" && !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                if let Some(s) = pending_str.take() {
                    out.push_str(&s);
                }
                nums.clear();
            }
            b"TJ" if in_text => {
                if let Some(s) = pending_arr.take() {
                    out.push_str(&s);
                }
                nums.clear();
            }
            b"Td" | b"TD" if in_text => {
                let dy = nums.last().copied().unwrap_or(0.0);
                if dy.abs() > 1.0 {
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                } else if !out.is_empty() && !out.ends_with([' ', '\n']) {
                    out.push(' ');
                }
                nums.clear();
            }
            b"T*" if in_text => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                nums.clear();
            }
            b"Tm" if in_text => {
                if !out.is_empty() && !out.ends_with([' ', '\n']) {
                    out.push(' ');
                }
                nums.clear();
            }
            _ => {
                nums.clear();
            }
        }
    }
}

/// Pure-Rust PDF text extractor supporting uncompressed and FlateDecode streams plus ToUnicode CMaps.
pub fn extract_pdf_text(bytes: &[u8]) -> Result<String, String> {
    if bytes.is_empty() {
        return Err("Empty PDF.".into());
    }
    if bytes.len() > MAX_PDF_BYTES {
        return Err("PDF exceeds the 50 MiB size limit.".into());
    }
    if !bytes.starts_with(b"%PDF-")
        && find_bytes(&bytes[..bytes.len().min(1024)], b"%PDF-", 0).is_none()
    {
        return Err("File is not a valid PDF.".into());
    }

    const MAX_TOTAL_INFLATED: usize = 32 * 1024 * 1024;
    const MAX_PDF_STREAMS: usize = 512;
    let mut streams: Vec<Vec<u8>> = Vec::new();
    let mut total_inflated: usize = 0;
    let mut pos = 0usize;
    while let Some(s_pos) = find_bytes(bytes, b"stream", pos) {
        if streams.len() >= MAX_PDF_STREAMS || total_inflated >= MAX_TOTAL_INFLATED {
            break;
        }
        // Ensure "stream" is a standalone keyword, not "endstream".
        let prev_ok = s_pos == 0 || !bytes[s_pos - 1].is_ascii_alphabetic();
        let after_kw = s_pos + 6;
        let data_start = if after_kw + 1 < bytes.len()
            && bytes[after_kw] == b'\r'
            && bytes[after_kw + 1] == b'\n'
        {
            after_kw + 2
        } else if after_kw < bytes.len() && (bytes[after_kw] == b'\n' || bytes[after_kw] == b'\r') {
            after_kw + 1
        } else {
            pos = after_kw;
            continue;
        };
        if !prev_ok {
            pos = after_kw;
            continue;
        }
        let Some(e_pos) = find_bytes(bytes, b"endstream", data_start) else {
            break;
        };
        let mut data_end = e_pos;
        while data_end > data_start && matches!(bytes[data_end - 1], b'\n' | b'\r') {
            data_end -= 1;
        }
        let header_start = s_pos.saturating_sub(512);
        let header = String::from_utf8_lossy(&bytes[header_start..s_pos]);
        // Skip obvious image XObjects.
        let is_image_xobj = header.contains("/Subtype /Image") || header.contains("/Subtype/Image");
        if !is_image_xobj && data_end > data_start {
            let slice = &bytes[data_start..data_end];
            let flate = header.contains("/FlateDecode");
            let maybe = if flate {
                inflate_stream(slice)
            } else if slice.windows(2).any(|w| w == b"BT")
                || slice.windows(11).any(|w| w == b"beginbfchar")
            {
                Some(slice.to_vec())
            } else {
                inflate_stream(slice)
            };
            if let Some(buf) = maybe {
                if total_inflated.saturating_add(buf.len()) <= MAX_TOTAL_INFLATED {
                    total_inflated += buf.len();
                    streams.push(buf);
                } else {
                    break;
                }
            }
        }
        pos = e_pos + 9;
    }

    let mut cmap: HashMap<u16, String> = HashMap::new();
    for s in &streams {
        parse_to_unicode_cmap(s, &mut cmap);
    }

    let mut extracted = String::new();
    for s in &streams {
        if s.windows(2).any(|w| w == b"BT") {
            let before = extracted.len();
            extract_text_from_content(s, &cmap, &mut extracted);
            if extracted.len() > before && !extracted.ends_with("\n\n") {
                extracted.push('\n');
            }
        }
        if extracted.len() >= MAX_MD_CHARS {
            break;
        }
    }

    // Clean up whitespace while preserving paragraph structure.
    let mut cleaned_lines: Vec<String> = Vec::new();
    let mut prev_blank = false;
    for raw_line in extracted.lines() {
        let line: String = raw_line
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if line.is_empty() {
            if !prev_blank && !cleaned_lines.is_empty() {
                cleaned_lines.push(String::new());
                prev_blank = true;
            }
        } else {
            cleaned_lines.push(line);
            prev_blank = false;
        }
    }
    let result = cleaned_lines.join("\n").trim().to_string();
    if result.is_empty() {
        return Err("No extractable text found in PDF (scanned/image-only PDF).".into());
    }
    Ok(result)
}

fn is_unsafe_local_path(s: &str) -> bool {
    let t = s.trim();
    t.is_empty()
        || t.starts_with(r"\\")
        || t.starts_with("//")
        || t.bytes().any(|b| b < 0x20 || b == 0x7f)
}

/// Convert a PDF to Markdown in pure Rust, cached by SHA-1 content hash next to the PDF.
pub fn to_markdown(pdf: &Path) -> Result<PathBuf, String> {
    if is_unsafe_local_path(&pdf.to_string_lossy()) {
        return Err("Network/UNC or control-character PDF paths are not allowed.".into());
    }
    let md_meta = std::fs::metadata(pdf).map_err(|e| format!("Could not read PDF: {e}"))?;
    if md_meta.len() as usize > MAX_PDF_BYTES {
        return Err("PDF exceeds the 50 MiB size limit.".into());
    }
    let bytes = std::fs::read(pdf).map_err(|e| format!("Could not read PDF: {e}"))?;
    if bytes.is_empty() {
        return Err("Empty PDF.".into());
    }
    let mut h = Sha1::new();
    h.update(&bytes);
    let sha = h.digest().to_string();
    let cache_dir = cache_dir_for(pdf)?;
    std::fs::create_dir_all(&cache_dir).map_err(|e| format!("Could not create cache dir: {e}"))?;
    let out = cache_dir.join(format!("{sha}.md"));
    if out.is_file() {
        return Ok(out);
    }

    let text = extract_pdf_text(&bytes)?;
    let title = pdf
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "document.pdf".into());
    let markdown = format!("# {title}\n\n{text}\n");

    static PDF_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let seq = PDF_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = cache_dir.join(format!("{sha}.{}.{seq}.md.tmp", std::process::id()));
    std::fs::write(&tmp, markdown.as_bytes())
        .map_err(|e| format!("Could not write temporary Markdown: {e}"))?;
    if let Err(e) = std::fs::rename(&tmp, &out) {
        let _ = std::fs::remove_file(&tmp);
        if out.is_file() {
            return Ok(out);
        }
        return Err(format!("Could not save cached Markdown: {e}"));
    }
    Ok(out)
}

/// Replace `.pdf` attachments with their extracted Markdown version; return `(final paths, notes)`.
pub fn prepare_attachments(files: &[String]) -> (Vec<String>, Vec<String>) {
    let mut out = Vec::with_capacity(files.len());
    let mut notes = Vec::new();
    for f in files {
        let p = Path::new(f);
        let is_pdf = p
            .extension()
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "PDF".into());
        if !is_pdf {
            out.push(f.clone());
            continue;
        }
        match to_markdown(p) {
            Ok(md) => {
                notes.push(format!(
                    "{name}: extracted to Markdown (pure Rust, fewer tokens)."
                ));
                out.push(md.to_string_lossy().to_string());
            }
            Err(e) => {
                notes.push(format!(
                    "{name}: could not extract text ({e}); keeping original PDF."
                ));
                out.push(f.clone());
            }
        }
    }
    (out, notes)
}

pub(crate) fn is_image(name: &str) -> bool {
    let n = name.to_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".svg"]
        .iter()
        .any(|e| n.ends_with(e))
}

/// Text attachments are inlined for API brains (they cannot open local files); images are listed by path.
pub(crate) fn attach_to_prompt(prompt: &str, files: &[String], api_brain: bool) -> String {
    const ATTACHMENT_LIMIT: usize = 40_000;
    if files.is_empty() {
        return prompt.to_string();
    }
    let mut p = format!("{prompt}\n\nAttached files (review before starting):\n");
    for f in files {
        p.push_str(&format!("- {f}\n"));
    }
    if api_brain {
        let mut remaining = ATTACHMENT_LIMIT;
        let mut truncated = false;
        for f in files {
            if is_unsafe_local_path(f) {
                p.push_str(&format!("\n[{f}: skipped — network/UNC or invalid path]\n"));
                continue;
            }
            if is_image(f) {
                p.push_str(&format!(
                    "\n[{f}: image; this API brain cannot view local images]\n"
                ));
                continue;
            }
            if remaining == 0 {
                truncated = true;
                continue;
            }
            if std::fs::metadata(f)
                .map(|m| m.len() > MAX_INLINE_FILE_BYTES)
                .unwrap_or(true)
            {
                p.push_str(&format!(
                    "\n[{f}: skipped inline preview — file exceeds 2 MiB or is unreadable]\n"
                ));
                continue;
            }
            if let Ok(t) = std::fs::read_to_string(f) {
                let text = match t.char_indices().nth(remaining) {
                    Some((end, _)) => {
                        remaining = 0;
                        truncated = true;
                        &t[..end]
                    }
                    None => {
                        remaining -= t.chars().count();
                        &t
                    }
                };
                p.push_str(&format!("\n--- {f} ---\n{text}\n"));
            }
        }
        if truncated {
            p.push_str(
                "\n[Note: attachment contents were truncated to the 40,000-character limit.]\n",
            );
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;
    use std::time::Duration;

    /// Build a tiny one-page PDF with given ASCII text, optionally Flate-compressed.
    fn minimal_pdf(text: &str, compress: bool) -> Vec<u8> {
        let content = format!("BT /F1 24 Tf 20 100 Td ({text}) Tj ET");
        let stream_bytes: Vec<u8> = if compress {
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            enc.write_all(content.as_bytes()).unwrap();
            enc.finish().unwrap()
        } else {
            content.into_bytes()
        };
        let filter = if compress {
            " /Filter /FlateDecode"
        } else {
            ""
        };
        let mut out: Vec<u8> = b"%PDF-1.4\n".to_vec();
        out.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        out.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
        out.extend_from_slice(
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>\nendobj\n",
        );
        out.extend_from_slice(
            format!(
                "4 0 obj\n<< /Length {}{} >>\nstream\n",
                stream_bytes.len(),
                filter
            )
            .as_bytes(),
        );
        out.extend_from_slice(&stream_bytes);
        out.extend_from_slice(b"\nendstream\nendobj\n%%EOF\n");
        out
    }

    #[test]
    fn converts_and_caches() {
        let dir = std::env::temp_dir().join(format!("mb_docs_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("hello.pdf");
        std::fs::write(&pdf, minimal_pdf("Hello Pure Rust PDF", false)).unwrap();

        let md1 = to_markdown(&pdf).expect("pure-Rust extraction must succeed");
        assert!(md1.is_file());
        let t1 = std::fs::metadata(&md1).and_then(|m| m.modified()).ok();

        std::thread::sleep(Duration::from_millis(1100));
        let md2 = to_markdown(&pdf).expect("second call must hit cache");
        assert_eq!(md1, md2);
        let t2 = std::fs::metadata(&md2).and_then(|m| m.modified()).ok();
        assert_eq!(t1, t2, "cache must not reconvert");

        let txt = std::fs::read_to_string(&md1).unwrap_or_default();
        assert!(
            txt.contains("Hello Pure Rust PDF"),
            "unexpected text: {txt}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_flate_compressed_pdf() {
        let raw = minimal_pdf("Compressed MetaBrain Report", true);
        let text = extract_pdf_text(&raw).expect("flate stream must extract");
        assert!(text.contains("Compressed MetaBrain Report"), "got: {text}");
    }
}
