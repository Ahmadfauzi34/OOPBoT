//! Ring buffer output terbatas untuk [`super::BackgroundPlugin`].
//!
//! Menjaga penggunaan memori per task tetap O(1) (`<= MAX_OUTPUT_BUFFER_BYTES`) meskipun
//! proses background berjalan sangat lama dan mencetak log besar, sekaligus mendukung:
//! - Cursor inkremental (`since_byte` -> `next_byte`) agar polling hemat token LLM
//! - Pembacaan N baris terakhir (`tail_lines`) dan filter substring (`filter`)
//! - Pencarian pola teks (`contains_pattern`) untuk aksi `wait`
//! - Pemotongan aman di batas karakter UTF-8 (`first_utf8_char_boundary`)

use std::collections::VecDeque;

/// Batas buffer output per task di memori (64 KiB tail ring buffer) untuk mencegah OOM.
pub const MAX_OUTPUT_BUFFER_BYTES: usize = 64 * 1024;
/// Batas default byte output yang dikembalikan ke LLM pada aksi `output`.
pub const DEFAULT_OUTPUT_RETURN_BYTES: usize = 10_000;
/// Batas preview output per task pada aksi `list` agar payload JSON tetap ringkas.
pub const MAX_LIST_PREVIEW_BYTES: usize = 512;

/// Parameter pembacaan log dari [`RingOutputBuffer`].
#[derive(Debug, Clone, Copy)]
pub struct OutputQuery<'a> {
    pub since_byte: Option<u64>,
    pub tail_bytes: usize,
    pub tail_lines: Option<usize>,
    pub filter: Option<&'a str>,
}

/// Hasil potongan log beserta metadata posisi cursor.
#[derive(Debug, Clone)]
pub struct OutputSlice {
    pub text: String,
    pub next_byte: u64,
    pub window_start_byte: u64,
    pub cursor_reset: bool,
    pub truncated: bool,
}

/// Ring buffer byte terbatas untuk menyimpan ekor (*tail*) output proses tanpa risiko OOM.
#[derive(Debug, Default)]
pub struct RingOutputBuffer {
    pub(super) bytes: VecDeque<u8>,
    pub(super) total_bytes: u64,
    pub(super) dropped_bytes: u64,
}

impl RingOutputBuffer {
    pub fn push_chunk(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.total_bytes = self.total_bytes.saturating_add(chunk.len() as u64);

        if chunk.len() >= MAX_OUTPUT_BUFFER_BYTES {
            let overflow = self.bytes.len() + (chunk.len() - MAX_OUTPUT_BUFFER_BYTES);
            self.dropped_bytes = self.dropped_bytes.saturating_add(overflow as u64);
            self.bytes.clear();
            let tail = &chunk[chunk.len() - MAX_OUTPUT_BUFFER_BYTES..];
            self.bytes.extend(tail);
            return;
        }

        let needed = self.bytes.len() + chunk.len();
        if needed > MAX_OUTPUT_BUFFER_BYTES {
            let to_drop = needed - MAX_OUTPUT_BUFFER_BYTES;
            for _ in 0..to_drop {
                self.bytes.pop_front();
            }
            self.dropped_bytes = self.dropped_bytes.saturating_add(to_drop as u64);
        }
        self.bytes.extend(chunk);
    }

    pub fn is_truncated(&self) -> bool {
        self.dropped_bytes > 0
    }

    pub fn window_start_byte(&self) -> u64 {
        self.total_bytes.saturating_sub(self.bytes.len() as u64)
    }

    pub fn contains_pattern(&self, pattern: &str) -> bool {
        if pattern.is_empty() || self.bytes.is_empty() {
            return false;
        }
        let raw: Vec<u8> = self.bytes.iter().copied().collect();
        let utf8_start = first_utf8_char_boundary(&raw);
        String::from_utf8_lossy(&raw[utf8_start..]).contains(pattern)
    }

    pub fn query(&self, q: OutputQuery<'_>) -> OutputSlice {
        let window_start = self.window_start_byte();
        let next_byte = self.total_bytes;

        if self.bytes.is_empty() {
            return OutputSlice {
                text: String::new(),
                next_byte,
                window_start_byte: window_start,
                cursor_reset: q.since_byte.map(|s| s < window_start).unwrap_or(false),
                truncated: self.is_truncated(),
            };
        }

        let tail_limit = q.tail_bytes.min(MAX_OUTPUT_BUFFER_BYTES);
        let (rel_start, cursor_reset, since_skipped_bytes) = match q.since_byte {
            Some(since) if since >= self.total_bytes => {
                return OutputSlice {
                    text: String::new(),
                    next_byte,
                    window_start_byte: window_start,
                    cursor_reset: false,
                    truncated: self.is_truncated(),
                };
            }
            Some(since) if since < window_start => {
                let len = self.bytes.len();
                (len.saturating_sub(tail_limit), true, 0)
            }
            Some(since) => {
                let offset = (since - window_start) as usize;
                let avail = self.bytes.len().saturating_sub(offset);
                if avail > tail_limit {
                    let start = self.bytes.len() - tail_limit;
                    (start, false, start.saturating_sub(offset))
                } else {
                    (offset, false, 0)
                }
            }
            None => {
                let len = self.bytes.len();
                (len.saturating_sub(tail_limit), false, 0)
            }
        };

        let raw: Vec<u8> = self.bytes.iter().skip(rel_start).copied().collect();
        let utf8_start = first_utf8_char_boundary(&raw);
        let valid_slice = &raw[utf8_start..];
        let mut rendered = String::from_utf8_lossy(valid_slice).into_owned();

        if let Some(needle) = q.filter.map(str::trim).filter(|s| !s.is_empty()) {
            let needle_lower = needle.to_lowercase();
            let filtered: Vec<&str> = rendered
                .lines()
                .filter(|line| line.to_lowercase().contains(&needle_lower))
                .collect();
            rendered = filtered.join("\n");
        }

        if let Some(max_lines) = q.tail_lines.filter(|&n| n > 0) {
            let lines: Vec<&str> = rendered.lines().collect();
            if lines.len() > max_lines {
                let start_line = lines.len() - max_lines;
                rendered = lines[start_line..].join("\n");
            }
        }

        let displayed_bytes = valid_slice.len() as u64;
        let slice_truncated = rel_start > 0 || self.is_truncated();

        let text = if q.since_byte.is_none() && displayed_bytes < self.total_bytes {
            let omitted = self.total_bytes.saturating_sub(displayed_bytes);
            format!(
                "...[{} byte awal dipangkas; menampilkan {} byte terakhir dari total {} byte]\n{}",
                omitted, displayed_bytes, self.total_bytes, rendered
            )
        } else if cursor_reset {
            let omitted = self.total_bytes.saturating_sub(displayed_bytes);
            format!(
                "...[cursor since_byte telah terlewati ring buffer ({} byte awal terlewat); menampilkan {} byte terbaru]\n{}",
                omitted, displayed_bytes, rendered
            )
        } else if since_skipped_bytes > 0 {
            let skipped = since_skipped_bytes.saturating_add(utf8_start);
            format!(
                "...[{} byte baru dilewati oleh batas tail_bytes; menampilkan {} byte terbaru]\n{}",
                skipped, displayed_bytes, rendered
            )
        } else {
            rendered
        };

        OutputSlice {
            text,
            next_byte,
            window_start_byte: window_start,
            cursor_reset,
            truncated: slice_truncated,
        }
    }

    pub fn render_tail(&self, max_bytes: usize) -> String {
        self.query(OutputQuery {
            since_byte: None,
            tail_bytes: max_bytes,
            tail_lines: None,
            filter: None,
        })
        .text
    }
}

pub(super) fn first_utf8_char_boundary(bytes: &[u8]) -> usize {
    // Byte lanjutan UTF-8 selalu berpola 10xxxxxx (0x80..=0xBF); maksimal 3 byte di awal slice.
    let mut idx = 0;
    while idx < bytes.len() && idx < 4 && (bytes[idx] & 0b1100_0000) == 0b1000_0000 {
        idx += 1;
    }
    if idx < bytes.len() {
        idx
    } else {
        0
    }
}
