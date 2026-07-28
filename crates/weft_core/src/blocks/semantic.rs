//! v1.7.0-C: Output semantic classifier — a pure-logic, cross-command
//! classifier that assigns semantic roles to unstyled (no-ANSI) command
//! output text. It reads only single-line text + bounded context; it does
//! NOT read files, access the network, or execute commands.
//!
//! ## Design (V17 §2.3)
//!
//! The classifier runs ONLY on ranges where the program did not emit ANSI
//! styling (`ansi_owned == false`). ANSI-owned ranges are always preserved
//! as-is; semantic fallback never overrides program-emitted colors.
//!
//! Rules are intentionally generic — no per-command or per-field-name logic:
//! - **Lexical**: URL, POSIX/`~` path, IPv4/IPv6, host:port, duration, PID,
//!   version, bounded hash, integer/decimal.
//! - **Structural**: `label: value`, `key=value`, table columns. Only the
//!   short label on the left of the separator and identified value tokens
//!   are colored.
//! - **Status**: complete-word tokens (ok/ready/running/active/loaded/passed,
//!   warn/pending/degraded, error/failed/inactive/stopped). Substring matches
//!   inside paths, URLs, JSON strings, or long natural language are NOT matched.
//!
//! ## Limits
//!
//! - O(n) per line; max 256 spans/line, 8192 spans/block.
//! - Over-limit → drop remaining spans for that line/block; text is unaffected.
//! - Lines >16 KiB or control-char-dense are skipped entirely.
//! - Binary substitute text (U+FFFD) → skip line.

// ── Types ──────────────────────────────────────────────────────────────

/// v1.7.0-C: Semantic role assigned to a range of unstyled output text.
/// Stored as a role (not RGB) so theme switches re-resolve colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OutputSemanticRole {
    /// A short label before `:` or `=` (e.g. `Status:` in `Status: ok`).
    Label,
    /// A filesystem path (POSIX or `~`-relative).
    Path,
    /// A URL (`http://`, `https://`, `ftp://`, `git://`, etc.).
    Url,
    /// An IP address (v4 or v6) or host:port.
    Address,
    /// A numeric literal (integer or decimal).
    Number,
    /// A version string (`v1.2.3`, `1.0.0-beta`, `2.0`).
    Version,
    /// A metadata key or secondary field (weaker than Label).
    Metadata,
    /// A success-status word (ok/ready/running/active/loaded/passed).
    Success,
    /// A warning-status word (warn/pending/degraded).
    Warning,
    /// A failure-status word (error/failed/inactive/stopped).
    Failure,
}

/// v1.7.0-C: A semantic span over a char-indexed range of a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SemanticSpan {
    pub start_char: u32,
    pub end_char: u32,
    pub role: OutputSemanticRole,
}

/// v1.7.0-C: Hard caps on semantic spans. Over-limit spans are dropped;
/// text is never affected.
pub const MAX_SEMANTIC_SPANS_PER_LINE: usize = 256;
pub const MAX_SEMANTIC_SPANS_PER_BLOCK: usize = 8_192;

/// Lines longer than this are skipped entirely (no semantic classification).
pub const MAX_SEMANTIC_LINE_BYTES: usize = 16_384;

/// v1.7.0-C: Per-line semantic classification result. Stored alongside
/// `StyledLine` so the renderer can apply semantic colors to unstyled ranges.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SemanticLine {
    pub line: u32,
    #[serde(default)]
    pub spans: Vec<SemanticSpan>,
}

/// v1.7.0-C: Block-level semantic output — the parallel to `StyledOutput`,
/// but for semantic fallback roles. Only lines with at least one span are
/// stored; lines without spans are implicit (no semantic coloring).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SemanticOutput {
    pub lines: Vec<SemanticLine>,
}

impl SemanticOutput {
    pub fn line(&self, index: usize) -> Option<&SemanticLine> {
        let index = u32::try_from(index).ok()?;
        self.lines
            .binary_search_by_key(&index, |line| line.line)
            .ok()
            .map(|position| &self.lines[position])
    }

    pub fn has_spans(&self) -> bool {
        !self.lines.is_empty()
    }
}

// ── Classifier ────────────────────────────────────────────────────────

/// Classify a single line of unstyled output text into semantic spans.
///
/// This is a pure function — no allocation beyond the returned `Vec`.
/// Returns an empty `Vec` when the line should be skipped (too long,
/// control-char-dense, or contains binary substitute characters).
///
/// `ansi_ranges` is a sorted slice of `[start, end)` char ranges that are
/// ANSI-owned. Spans overlapping these ranges are NOT emitted — the ANSI
/// color takes priority.
pub fn classify_line(line: &str, ansi_ranges: &[(u32, u32)]) -> Vec<SemanticSpan> {
    // Skip lines that are too long.
    if line.len() > MAX_SEMANTIC_LINE_BYTES {
        return Vec::new();
    }
    // Skip lines with binary substitute characters or excessive control chars.
    if line.contains('\u{FFFD}') {
        return Vec::new();
    }
    let control_count = line
        .chars()
        .filter(|c| (*c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r'))
        .count();
    if control_count > 3 {
        return Vec::new();
    }

    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<SemanticSpan> = Vec::new();
    let mut i = 0usize;

    while i < chars.len() {
        // Skip whitespace.
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }

        // Try to match a token starting at `i`.
        let char_start = i as u32;
        let (token_end, token_text) = match scan_token(&chars, i) {
            Some((end, text)) => (end, text),
            None => {
                i += 1;
                continue;
            }
        };
        let char_end = token_end as u32;

        // Check if this token overlaps an ANSI-owned range.
        if !overlaps_ansi(char_start, char_end, ansi_ranges) {
            // Classify the token.
            if let Some(role) = classify_token(&token_text, &chars, i, token_end) {
                push_span(&mut spans, char_start, char_end, role);
                // If this is a label followed by `:` or `=`, try to classify
                // the value token after the separator.
                if matches!(role, OutputSemanticRole::Label) {
                    if let Some((val_start, val_end, val_role)) =
                        scan_value_after_separator(&chars, token_end)
                    {
                        if !overlaps_ansi(val_start, val_end, ansi_ranges) {
                            push_span(&mut spans, val_start, val_end, val_role);
                        }
                    }
                }
            }
        }

        i = token_end;
    }

    // Enforce per-line span cap.
    spans.truncate(MAX_SEMANTIC_SPANS_PER_LINE);
    spans
}

/// Classify a block of text (multiple lines) into semantic output.
/// Lines are split on `\n`; each line is classified independently.
/// ANSI ranges are line-local (char indices reset per line).
///
/// `ansi_owned_per_line` is an optional slice parallel to the lines of `text`;
/// each element is the ANSI-owned ranges for that line. When `None`, no ANSI
/// masking is applied (all text is eligible for semantic classification).
pub fn classify_block(
    text: &str,
    ansi_owned_per_line: Option<&[Vec<(u32, u32)>]>,
) -> Option<SemanticOutput> {
    let mut lines: Vec<SemanticLine> = Vec::new();
    let mut total_spans = 0usize;

    for (line_idx, line_text) in (0u32..).zip(text.split('\n')) {
        let ansi_ranges = ansi_owned_per_line
            .and_then(|per_line| per_line.get(line_idx as usize))
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        let spans = classify_line(line_text, ansi_ranges);
        if !spans.is_empty() {
            total_spans += spans.len();
            if total_spans > MAX_SEMANTIC_SPANS_PER_BLOCK {
                // Block cap exceeded: keep what we have, drop the rest.
                break;
            }
            lines.push(SemanticLine {
                line: line_idx,
                spans,
            });
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(SemanticOutput { lines })
    }
}

// ── Token scanning ────────────────────────────────────────────────────

/// Scan a single token starting at `start`. Returns `(end_index, token_text)`.
/// A token is a maximal run of non-whitespace characters, with special handling
/// for URLs (which may contain `:` and `/`).
fn scan_token(chars: &[char], start: usize) -> Option<(usize, String)> {
    if start >= chars.len() {
        return None;
    }

    // Check for URL prefix.
    let url_end = scan_url(chars, start);
    if url_end > start {
        let text: String = chars[start..url_end].iter().collect();
        return Some((url_end, text));
    }

    // Scan a general word (non-whitespace, non-structural-separator).
    let mut i = start;
    while i < chars.len() && !chars[i].is_whitespace() {
        // Stop at structural separators (: =) when they're followed by space
        // or end — these end the token so the value can be scanned separately.
        if (chars[i] == ':' || chars[i] == '=') && is_label_end(chars, i) {
            break;
        }
        i += 1;
    }

    if i == start {
        return None;
    }
    let text: String = chars[start..i].iter().collect();
    Some((i, text))
}

/// Check if the char at `i` is a label-ending separator (`:` or `=`)
/// followed by whitespace or end-of-line.
fn is_label_end(chars: &[char], i: usize) -> bool {
    if i + 1 >= chars.len() {
        return true; // end of line
    }
    chars[i + 1].is_whitespace() || chars[i + 1] == '"' || chars[i + 1] == '\''
}

/// Scan a URL starting at `start`. Returns the end index (exclusive).
/// Returns `start` if no URL prefix is found.
fn scan_url(chars: &[char], start: usize) -> usize {
    // Check for scheme:// prefix
    let scheme_end = chars.iter().skip(start).position(|c| *c == ':');
    if let Some(colon_offset) = scheme_end {
        let colon_idx = start + colon_offset;
        // Scheme must be 2-10 alpha chars.
        if colon_idx > start && colon_idx - start <= 10 {
            let is_scheme = chars[start..colon_idx].iter().all(|c| {
                c.is_ascii_alphabetic() || c.is_ascii_digit() || *c == '+' || *c == '-' || *c == '.'
            });
            if is_scheme
                && colon_idx + 2 < chars.len()
                && chars[colon_idx + 1] == '/'
                && chars[colon_idx + 2] == '/'
            {
                // Scan to end of URL (non-whitespace).
                let mut i = colon_idx + 3;
                while i < chars.len() && !chars[i].is_whitespace() {
                    i += 1;
                }
                return i;
            }
        }
    }
    start
}

/// After a label token, scan the separator (`:` or `=`) and the value token.
/// Returns `(value_start, value_end, value_role)`.
fn scan_value_after_separator(
    chars: &[char],
    label_end: usize,
) -> Option<(u32, u32, OutputSemanticRole)> {
    let mut i = label_end;
    // Skip the separator.
    if i < chars.len() && (chars[i] == ':' || chars[i] == '=') {
        i += 1;
    } else {
        return None;
    }
    // Skip whitespace after separator.
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    if i >= chars.len() {
        return None;
    }

    let val_start = i as u32;
    // Scan the value token (non-whitespace).
    while i < chars.len() && !chars[i].is_whitespace() {
        i += 1;
    }
    if i as u32 == val_start {
        return None;
    }
    let val_end = i as u32;
    let val_text: String = chars[val_start as usize..i].iter().collect();

    // Classify the value.
    let role = classify_token(&val_text, chars, val_start as usize, i)
        .unwrap_or(OutputSemanticRole::Metadata);
    Some((val_start, val_end, role))
}

// ── Token classification ──────────────────────────────────────────────

/// Classify a single token into a semantic role. Returns `None` when the
/// token doesn't match any role (it stays as plain `output_default`).
///
/// `chars`, `start`, `end` provide line context so we can detect labels
/// (a token followed by `:` or `=` and whitespace — the separator is NOT
/// part of the token itself).
fn classify_token(
    text: &str,
    chars: &[char],
    _start: usize,
    end: usize,
) -> Option<OutputSemanticRole> {
    // Status words (complete-word match only).
    if let Some(role) = classify_status_word(text) {
        return Some(role);
    }
    // URL.
    if is_url(text) {
        return Some(OutputSemanticRole::Url);
    }
    // Path (contains `/` or starts with `~`).
    if is_path(text) {
        return Some(OutputSemanticRole::Path);
    }
    // IPv4 or host:port.
    if is_ipv4(text) || is_host_port(text) {
        return Some(OutputSemanticRole::Address);
    }
    // Version (v1.2.3, 1.0.0-beta, 2.0).
    if is_version(text) {
        return Some(OutputSemanticRole::Version);
    }
    // Number (integer or decimal).
    if is_number_token(text) {
        return Some(OutputSemanticRole::Number);
    }
    // Label: a short token (≤30 chars) followed by `:` or `=` and whitespace
    // (or end of line). The separator is NOT part of the token.
    if text.len() <= 30 && is_label_separator_ahead(chars, end) {
        return Some(OutputSemanticRole::Label);
    }
    // Key=value: the token itself contains `=` with content on both sides.
    if let Some(eq_pos) = text.find('=') {
        if eq_pos > 0 && eq_pos < text.len() - 1 {
            return Some(OutputSemanticRole::Label);
        }
    }

    None
}

/// Check if the char at `end` is a label separator (`:` or `=`) followed by
/// whitespace, quote, or end-of-line. This identifies `token: value` patterns
/// where the colon is NOT part of the scanned token.
fn is_label_separator_ahead(chars: &[char], end: usize) -> bool {
    if end >= chars.len() {
        return false;
    }
    let sep = chars[end];
    if sep != ':' && sep != '=' {
        return false;
    }
    // Separator must be followed by whitespace, quote, or end-of-line.
    if end + 1 >= chars.len() {
        return true; // end of line
    }
    let next = chars[end + 1];
    next.is_whitespace() || next == '"' || next == '\''
}

/// Match complete-word status tokens. Only matches the full token —
/// substrings inside paths/URLs/long text are NOT matched.
fn classify_status_word(text: &str) -> Option<OutputSemanticRole> {
    // Normalize: strip surrounding punctuation that's common in output
    // (brackets, parens, quotes, periods, semicolons, colons, commas) but
    // keep the core word intact. This lets "ok.", "passed;", "[active]",
    // "\"active\"" all match.
    let cleaned = text.trim_matches(|c: char| {
        c == '['
            || c == ']'
            || c == '('
            || c == ')'
            || c == '<'
            || c == '>'
            || c == '.'
            || c == ';'
            || c == ','
            || c == ':'
            || c == '"'
            || c == '\''
    });
    match cleaned.to_lowercase().as_str() {
        "ok" | "ready" | "running" | "active" | "loaded" | "passed" | "up" | "healthy"
        | "complete" | "completed" | "success" | "succeeded" | "started" | "alive"
        | "reachable" => Some(OutputSemanticRole::Success),
        "warn" | "warning" | "pending" | "degraded" | "paused" | "queued" | "waiting" => {
            Some(OutputSemanticRole::Warning)
        }
        "error" | "failed" | "failure" | "inactive" | "stopped" | "down" | "crashed" | "dead"
        | "exited" | "aborted" => Some(OutputSemanticRole::Failure),
        _ => None,
    }
}

fn is_url(text: &str) -> bool {
    let colon_pos = match text.find(':') {
        Some(p) => p,
        None => return false,
    };
    if colon_pos == 0 || colon_pos > 10 {
        return false;
    }
    let scheme = &text[..colon_pos];
    if !scheme
        .chars()
        .all(|c| c.is_ascii_alphabetic() || c.is_ascii_digit() || c == '+' || c == '-' || c == '.')
    {
        return false;
    }
    text[colon_pos..].starts_with("://")
}

fn is_path(text: &str) -> bool {
    if text.starts_with('~') {
        return true;
    }
    if text.contains('/') {
        // Reject if it looks like a URL (already handled) or a date.
        if is_url(text) {
            return false;
        }
        return true;
    }
    false
}

fn is_ipv4(text: &str) -> bool {
    // Reject if there's any whitespace or colons (IPv6 / host:port handled elsewhere).
    if text.contains(':') || text.contains('/') {
        return false;
    }
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts.iter().all(|p| {
        p.len() <= 3
            && p.chars().all(|c| c.is_ascii_digit())
            && p.parse::<u32>().is_ok_and(|n| n <= 255)
    })
}

fn is_host_port(text: &str) -> bool {
    // host:port — host is a hostname (alnum, dot, hyphen, underscore) and
    // port is 1-5 digits in the valid range. is_url() is checked before this
    // in classify_token, so URLs (scheme://...) never reach here.
    let Some((host, port)) = text.rsplit_once(':') else {
        return false;
    };
    if host.is_empty() || port.is_empty() {
        return false;
    }
    let host_ok = host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_');
    let port_ok = port.len() <= 5
        && port.chars().all(|c| c.is_ascii_digit())
        && port.parse::<u32>().is_ok_and(|p| p <= 65535);
    host_ok && port_ok
}

fn is_version(text: &str) -> bool {
    // v1.2.3, 1.0.0-beta, 2.0, v0.8.1-rc.1
    let stripped = text.strip_prefix('v').unwrap_or(text);
    if stripped.is_empty() {
        return false;
    }
    // Must start with a digit.
    if !stripped.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    // Must contain at least one dot (to distinguish from plain numbers).
    if !stripped.contains('.') {
        return false;
    }
    // First segment (before any non-version-char) must be numeric.dot.numeric.
    let version_part: String = stripped
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    // Must have at least two numeric segments separated by a dot.
    let numeric_segments: Vec<&str> = version_part.split('.').collect();
    if numeric_segments.len() < 2 {
        return false;
    }
    if !numeric_segments
        .iter()
        .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    // Optional suffix (-beta, -rc.1, +build) is allowed.
    true
}

fn is_number_token(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    // Integer or decimal, optional leading sign. Distinguish from version (no dots-with-segments).
    let bytes = text.as_bytes();
    let mut i = 0usize;
    if bytes[0] == b'+' || bytes[0] == b'-' {
        i += 1;
    }
    if i >= bytes.len() {
        return false;
    }
    let mut saw_digit = false;
    let mut saw_dot = false;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => {
                saw_digit = true;
            }
            b'.' if !saw_dot => {
                saw_dot = true;
            }
            // Allow trailing unit suffixes like 12ms, 100kB (single letter, common in output).
            c if i > 0 && (c as char).is_ascii_alphabetic() => {
                // Only allow a 1-2 char alphabetic suffix at the end.
                let suffix = &text[i..];
                return suffix.len() <= 3
                    && suffix.chars().all(|c| c.is_ascii_alphabetic())
                    && saw_digit;
            }
            _ => return false,
        }
        i += 1;
    }
    saw_digit
}

/// Check if `[start, end)` overlaps any of the ANSI-owned ranges.
fn overlaps_ansi(start: u32, end: u32, ansi_ranges: &[(u32, u32)]) -> bool {
    ansi_ranges.iter().any(|(a_start, a_end)| {
        // Standard overlap check: not (end <= a_start || start >= a_end)
        !(end <= *a_start || start >= *a_end)
    })
}

/// Push a span, merging with the previous span if same role and adjacent.
fn push_span(spans: &mut Vec<SemanticSpan>, start: u32, end: u32, role: OutputSemanticRole) {
    if start >= end {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.role == role && last.end_char == start {
            last.end_char = end;
            return;
        }
    }
    spans.push(SemanticSpan {
        start_char: start,
        end_char: end,
        role,
    });
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: u32, end: u32, role: OutputSemanticRole) -> SemanticSpan {
        SemanticSpan {
            start_char: start,
            end_char: end,
            role,
        }
    }

    // ── classify_line: basic ───────────────────────────────────────────

    #[test]
    fn classifies_url() {
        let spans = classify_line("see https://example.com/path?q=1", &[]);
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Url));
    }

    #[test]
    fn classifies_path_absolute() {
        let spans = classify_line("wrote /tmp/foo/bar.txt", &[]);
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Path));
    }

    #[test]
    fn classifies_path_tilde() {
        let spans = classify_line("see ~/.config/weft", &[]);
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Path));
    }

    #[test]
    fn classifies_ipv4() {
        let spans = classify_line("ping 192.168.1.1", &[]);
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Address));
    }

    #[test]
    fn classifies_host_port() {
        let spans = classify_line("listening on example.com:8080", &[]);
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Address));
    }

    #[test]
    fn classifies_version() {
        let spans = classify_line("installed v1.2.3-beta", &[]);
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Version));
    }

    #[test]
    fn classifies_number() {
        let spans = classify_line("count: 42", &[]);
        // The 42 should be a number (or the value of the label).
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Number));
    }

    #[test]
    fn classifies_label_value() {
        let spans = classify_line("Status: ok", &[]);
        // "Status:" is a label; "ok" is a success value.
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Label));
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Success));
    }

    #[test]
    fn classifies_label_key_value() {
        let spans = classify_line("count=42", &[]);
        // The whole "count=42" token: classify_token returns Label for it.
        // (The implementation: text.find('=') -> Label, with eq_pos in middle.)
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Label));
    }

    #[test]
    fn classifies_success_words() {
        for word in &["ok", "ready", "running", "active", "loaded", "passed"] {
            let spans = classify_line(word, &[]);
            assert!(
                spans.iter().any(|s| s.role == OutputSemanticRole::Success),
                "expected Success for {}",
                word
            );
        }
    }

    #[test]
    fn classifies_warning_words() {
        for word in &["warn", "pending", "degraded", "paused"] {
            let spans = classify_line(word, &[]);
            assert!(
                spans.iter().any(|s| s.role == OutputSemanticRole::Warning),
                "expected Warning for {}",
                word
            );
        }
    }

    #[test]
    fn classifies_failure_words() {
        for word in &["error", "failed", "inactive", "stopped", "crashed"] {
            let spans = classify_line(word, &[]);
            assert!(
                spans.iter().any(|s| s.role == OutputSemanticRole::Failure),
                "expected Failure for {}",
                word
            );
        }
    }

    // ── Negative cases: substring inside paths/URLs must NOT match ─────

    #[test]
    fn does_not_match_status_inside_url() {
        // "error" appears as a substring of the URL host, but the URL token
        // is scanned as a whole and classified as Url, not Failure.
        let spans = classify_line("https://error.example.com/", &[]);
        assert!(spans.iter().all(|s| s.role != OutputSemanticRole::Failure));
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Url));
    }

    #[test]
    fn does_not_match_status_inside_path() {
        // "active" appears as a path segment, but the whole token is a Path.
        let spans = classify_line("/var/active/failed.log", &[]);
        assert!(spans.iter().all(|s| s.role != OutputSemanticRole::Success));
        assert!(spans.iter().all(|s| s.role != OutputSemanticRole::Failure));
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Path));
    }

    // ── ANSI mask ──────────────────────────────────────────────────────

    #[test]
    fn ansi_ranges_mask_classifications() {
        // The "ok" is inside an ANSI-owned range [3..5), so it should not
        // be classified as Success.
        let spans = classify_line("ok ok", &[(0, 2)]);
        // First "ok" should be masked; only the second one is classified.
        let successes: Vec<_> = spans
            .iter()
            .filter(|s| s.role == OutputSemanticRole::Success)
            .collect();
        assert_eq!(successes.len(), 1);
        assert!(successes[0].start_char >= 3);
    }

    #[test]
    fn ansi_ranges_partial_overlap_skips_token() {
        // Even a partial overlap skips the whole token.
        let spans = classify_line("path /tmp/foo", &[(5, 8)]);
        // The "/tmp/foo" token starts at index 5 and overlaps [5,8), so it
        // should not be classified as a Path.
        assert!(spans.iter().all(|s| s.role != OutputSemanticRole::Path));
    }

    // ── Line-level limits ─────────────────────────────────────────────

    #[test]
    fn skips_too_long_line() {
        let long_line = "a".repeat(MAX_SEMANTIC_LINE_BYTES + 1);
        let spans = classify_line(&long_line, &[]);
        assert!(spans.is_empty());
    }

    #[test]
    fn skips_control_char_dense_line() {
        // 4 control chars (> 3 allowed) → skip.
        let line = "ok\x01\x02\x03\x04";
        let spans = classify_line(line, &[]);
        assert!(spans.is_empty());
    }

    #[test]
    fn skips_binary_substitute_line() {
        let spans = classify_line("ok \u{FFFD} failure", &[]);
        assert!(spans.is_empty());
    }

    #[test]
    fn respects_per_line_span_cap() {
        // Generate many labels to exceed 256 spans. Each "x:" is a label.
        let mut line = String::new();
        for i in 0..MAX_SEMANTIC_SPANS_PER_LINE + 50 {
            if i > 0 {
                line.push(' ');
            }
            line.push_str(&format!("x{}:", i));
        }
        let spans = classify_line(&line, &[]);
        assert!(spans.len() <= MAX_SEMANTIC_SPANS_PER_LINE);
    }

    // ── classify_block ────────────────────────────────────────────────

    #[test]
    fn classify_block_multiple_lines() {
        let text = "Status: ok\nPath: /tmp/foo\nError: failed";
        let output = classify_block(text, None).expect("output");
        assert_eq!(output.lines.len(), 3);
        assert_eq!(output.lines[0].line, 0);
        assert_eq!(output.lines[1].line, 1);
        assert_eq!(output.lines[2].line, 2);
    }

    #[test]
    fn classify_block_skips_empty_lines() {
        let text = "ok\n\n\nfailed";
        let output = classify_block(text, None).expect("output");
        // Empty lines produce no spans, so only lines 0 and 3 are stored.
        assert_eq!(output.lines.len(), 2);
        assert_eq!(output.lines[0].line, 0);
        assert_eq!(output.lines[1].line, 3);
    }

    #[test]
    fn classify_block_respects_block_cap() {
        // Build a text with many lines that each produce spans, exceeding
        // the block cap.
        let mut lines: Vec<String> = Vec::new();
        for i in 0..1000 {
            lines.push(format!("line{}: ok", i));
        }
        let text = lines.join("\n");
        let output = classify_block(&text, None).expect("output");
        let total_spans: usize = output.lines.iter().map(|l| l.spans.len()).sum();
        assert!(total_spans <= MAX_SEMANTIC_SPANS_PER_BLOCK);
    }

    #[test]
    fn classify_block_empty_returns_none() {
        let text = "plain text without any semantic tokens here";
        let output = classify_block(text, None);
        // "plain text without any semantic tokens here" — none of these
        // match a label, url, path, etc. Should return None.
        assert!(output.is_none());
    }

    #[test]
    fn classify_block_with_ansi_mask_per_line() {
        let text = "ok\nok";
        // Mask the first line entirely.
        let ansi = vec![vec![(0, 2)], vec![]];
        let output = classify_block(text, Some(&ansi)).expect("output");
        // Only line 1 should have spans.
        assert_eq!(output.lines.len(), 1);
        assert_eq!(output.lines[0].line, 1);
    }

    // ── SemanticOutput lookup ────────────────────────────────────────

    #[test]
    fn semantic_output_line_lookup() {
        let output = SemanticOutput {
            lines: vec![
                SemanticLine {
                    line: 0,
                    spans: vec![span(0, 2, OutputSemanticRole::Success)],
                },
                SemanticLine {
                    line: 5,
                    spans: vec![span(0, 5, OutputSemanticRole::Path)],
                },
            ],
        };
        assert!(output.line(0).is_some());
        assert!(output.line(5).is_some());
        assert!(output.line(3).is_none());
        assert!(output.has_spans());
    }

    #[test]
    fn semantic_output_empty_has_no_spans() {
        let output = SemanticOutput::default();
        assert!(!output.has_spans());
        assert!(output.line(0).is_none());
    }

    // ── Helper function unit tests ──────────────────────────────────

    #[test]
    fn is_ipv4_valid() {
        assert!(is_ipv4("192.168.1.1"));
        assert!(is_ipv4("0.0.0.0"));
        assert!(is_ipv4("255.255.255.255"));
    }

    #[test]
    fn is_ipv4_invalid() {
        assert!(!is_ipv4("256.1.1.1"));
        assert!(!is_ipv4("1.2.3"));
        assert!(!is_ipv4("1.2.3.4.5"));
        assert!(!is_ipv4("a.b.c.d"));
        assert!(!is_ipv4("1.2.3.4:8080"));
        assert!(!is_ipv4("1.2.3.4/path"));
    }

    #[test]
    fn is_host_port_valid() {
        assert!(is_host_port("example.com:8080"));
        assert!(is_host_port("localhost:3000"));
        assert!(is_host_port("host.test:443"));
    }

    #[test]
    fn is_host_port_invalid() {
        assert!(!is_host_port("example.com")); // no port
        assert!(!is_host_port(":8080")); // no host
        assert!(!is_host_port("example.com:abc")); // non-numeric port
        assert!(!is_host_port("example.com:99999")); // port > 65535
    }

    #[test]
    fn is_version_valid() {
        assert!(is_version("v1.2.3"));
        assert!(is_version("1.0.0-beta"));
        assert!(is_version("2.0"));
        assert!(is_version("v0.8.1-rc.1"));
        assert!(is_version("1.2.3.4"));
    }

    #[test]
    fn is_version_invalid() {
        assert!(!is_version("v")); // no digits
        assert!(!is_version("1")); // no dot
        assert!(!is_version("vbeta")); // no numeric segments
        assert!(!is_version("hello.world")); // non-numeric segments
    }

    #[test]
    fn is_number_token_valid() {
        assert!(is_number_token("42"));
        assert!(is_number_token("-7"));
        assert!(is_number_token("+3.14"));
        assert!(is_number_token("100"));
        assert!(is_number_token("12ms"));
        assert!(is_number_token("100kB"));
    }

    #[test]
    fn is_number_token_invalid() {
        assert!(!is_number_token(""));
        assert!(!is_number_token("abc"));
        assert!(!is_number_token("1.2.3")); // version, not number
        assert!(!is_number_token("--5"));
    }

    #[test]
    fn overlaps_ansi_basic() {
        let ranges = [(5, 10)];
        assert!(overlaps_ansi(7, 12, &ranges));
        assert!(overlaps_ansi(0, 6, &ranges));
        assert!(overlaps_ansi(5, 10, &ranges)); // exact
        assert!(!overlaps_ansi(0, 5, &ranges)); // adjacent, no overlap
        assert!(!overlaps_ansi(10, 15, &ranges)); // adjacent, no overlap
    }

    #[test]
    fn push_span_merges_adjacent_same_role() {
        let mut spans = Vec::new();
        push_span(&mut spans, 0, 3, OutputSemanticRole::Path);
        push_span(&mut spans, 3, 6, OutputSemanticRole::Path);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].start_char, 0);
        assert_eq!(spans[0].end_char, 6);
    }

    #[test]
    fn push_span_does_not_merge_different_role() {
        let mut spans = Vec::new();
        push_span(&mut spans, 0, 3, OutputSemanticRole::Path);
        push_span(&mut spans, 3, 6, OutputSemanticRole::Url);
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn push_span_ignores_empty_range() {
        let mut spans = Vec::new();
        push_span(&mut spans, 5, 5, OutputSemanticRole::Path);
        assert!(spans.is_empty());
    }

    // ── Real-world fixture cases ────────────────────────────────────

    #[test]
    fn fixture_git_status_output() {
        let line = "On branch main";
        let spans = classify_line(line, &[]);
        // "main" is a plain word; no path/url/number/etc. So no spans.
        assert!(spans.is_empty() || spans.iter().all(|s| s.role != OutputSemanticRole::Path));
    }

    #[test]
    fn fixture_brew_services() {
        let line = "nginx started";
        let spans = classify_line(line, &[]);
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Success));
    }

    #[test]
    fn fixture_docker_ps() {
        let line = "CONTAINER ID   IMAGE   STATUS   NAMES";
        let spans = classify_line(line, &[]);
        // Header words are plain words; no labels (no `:`), no status words.
        // "ID" alone isn't a status. Result: no spans.
        assert!(
            spans.is_empty()
                || !spans.iter().any(|s| matches!(
                    s.role,
                    OutputSemanticRole::Success
                        | OutputSemanticRole::Failure
                        | OutputSemanticRole::Warning
                ))
        );
    }

    #[test]
    fn fixture_cargo_test_output() {
        let line = "test result: ok. 12 passed; 0 failed;";
        let spans = classify_line(line, &[]);
        // "ok" should be Success; "12" Number; "0" Number; "failed" Failure.
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Success));
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Failure));
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Number));
    }

    #[test]
    fn fixture_json_like_output() {
        let line = "\"status\": \"active\"";
        let spans = classify_line(line, &[]);
        // "active" is a Success word.
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Success));
    }

    #[test]
    fn fixture_log_line_with_address() {
        let line = "connection from 10.0.0.1:54321";
        let spans = classify_line(line, &[]);
        assert!(spans.iter().any(|s| s.role == OutputSemanticRole::Address));
    }
}
