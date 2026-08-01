//! Smart Select — recognize semantic targets (URL/path/hash) in terminal text.
//!
//! 纯逻辑 matcher：输入是「单行文本 + 字符偏移」，输出是规范化目标范围与类型。
//! 不读 Grid、不触碰 UI、不执行外部动作——所有副作用（打开浏览器/Finder/编辑器）
//! 由调用方负责，遵循 V110_IMPLEMENTATION_PLAN.md §3.2 安全合同：
//! "默认动作只选中或复制；打开浏览器、Finder 或编辑器必须由用户显式触发"
//!
//! # 优先级（V110_PLAN §3.2）
//! `path:line:column` → URL → email → host:port / IP → Git hash → 普通路径/标识符
//!
//! # 边界语义
//! 所有偏移以 **字节** 为单位（与 Rust &str 原生一致），便于从 `Grid::row_text`
//! 返回的 `String` 直接索引。调用方若持有 display column，需先用
//! `find.rs::build_tokens` 的模式映射到字节偏移（CJK 全角字符 1 char = 2 cols）。
//!
//! # CJK 边界
//! East Asian Wide 字符（display width == 2）与 ASCII 之间视为词边界——
//! 例如"编辑src/main.rs文件"中，"src/main.rs"应被独立识别为 Path。这避免
//! 把 CJK 标签算进 URL/path，符合 V110_PLAN §5 退出标准"CJK 前后的目标没有 off-by-one"。
//!
//! # 安全
//! matcher 只识别与返回文本，**绝不**执行。`SmartTargetKind::is_safe_to_open`
//! 提供调用方做 scheme 白名单判断的钩子，但执行决定权在调用方。

#![allow(clippy::module_name_repetitions)]

use crate::grid::terminal_char_width;

/// 识别出的目标类型。优先级与 V110_PLAN §3.2 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartTargetKind {
    /// `/path/to/file.rs:12:8` 或 `./src/lib.rs:42` —— 最高优先级
    PathLineColumn,
    /// `http(s)://example.com/path?q=1`、`ftp://...`、`mailto:...`
    Url,
    /// `user@example.com`
    Email,
    /// `127.0.0.1:8080`、`[::1]:443`、`example.com:443`
    HostPort,
    /// `127.0.0.1`、`::1`、`example.com`（无端口时的退化形式）
    IpOrHost,
    /// 7-40 位 hex（git commit short/long hash）
    GitHash,
    /// `/abs/path`、`./rel/path`、`~/home/path`、`src/file.rs`
    Path,
    /// 不含路径分隔符的单词标识符（兜底）
    Identifier,
}

impl SmartTargetKind {
    /// 目标是否属于"通常可打开"的类别——**仅作分类参考，不足以决定执行**。
    ///
    /// 调用方在执行任何外部动作（打开浏览器/Finder/编辑器）前，必须重新校验：
    /// 1. URL：scheme 白名单（http/https 允许；file:// 需额外路径越界检查）
    /// 2. Path：本地路径存在性、无 `..` 穿越、无 shell 元字符
    /// 3. 任何目标：用户必须显式触发（V110_PLAN §3.2 "禁止隐式执行"）
    ///
    /// 返回 `false` 的类别（HostPort/IpOrHost/GitHash/Email/Identifier）
    /// 默认只允许"选中/复制"，不允许任何外部动作。
    #[must_use]
    pub const fn is_safe_to_open(self) -> bool {
        matches!(self, Self::PathLineColumn | Self::Url | Self::Path)
    }
}

/// 识别出的目标范围。字节偏移可直接切 `&text[start..end]`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmartTarget {
    pub kind: SmartTargetKind,
    pub start: usize,
    pub end: usize,
}

/// A semantic target together with coordinates suitable for terminal UI
/// selection. Byte offsets remain the source of truth; display columns and
/// character indices are derived once here so Grid and BlockView callers do
/// not implement subtly different CJK/emoji conversions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmartDisplayTarget {
    pub target: SmartTarget,
    pub start_display_col: usize,
    pub end_display_col: usize,
    pub start_char: usize,
    pub end_char: usize,
}

impl SmartTarget {
    #[must_use]
    pub fn text<'a>(&self, text: &'a str) -> &'a str {
        &text[self.start..self.end]
    }
}

/// 在 `text` 的 `byte_idx` 处识别语义目标。
///
/// `byte_idx` 必须是 `text` 的合法 char 边界。函数会向两侧扩展直到目标边界，
/// 返回最高优先级的匹配。若 `byte_idx` 落在空白/无识别内容上，返回 `None`。
///
/// # 复杂度
/// O(n) 最坏情况（n = 行长），实际场景因早返回远低于此。无分配（除非返回
/// `SmartTarget` 的小 struct）。
#[must_use]
pub fn match_at(text: &str, byte_idx: usize) -> Option<SmartTarget> {
    // 契约：byte_idx 应 <= text.len() 且在 char 边界。
    // 越界时 release-safe 地返回 None（而非 panic），防御调用方的 display→byte 映射误差。
    if byte_idx >= text.len() {
        return None;
    }
    // 对齐到 char 边界：调用方可能传 char 中间的字节偏移
    let idx = align_to_char_boundary(text, byte_idx);
    if idx >= text.len() {
        return None;
    }
    // 空白/控制字符处不识别
    let ch = text[idx..].chars().next()?;
    if ch.is_whitespace() || ch.is_control() {
        return None;
    }

    // 先按「非空白 token」扩展出当前 word 边界
    let (word_start, word_end) = expand_word(text, idx);

    // 最高优先级：path:line:column（检查 word 是否带 `:num` 后缀，需向右探查）
    if let Some(t) = try_path_line_column(text, word_start, word_end) {
        return Some(t);
    }
    // URL（含 scheme）
    if let Some(t) = try_url(text, word_start, word_end) {
        return Some(t);
    }
    // email（含 @）
    if let Some(t) = try_email(text, word_start, word_end) {
        return Some(t);
    }
    // host:port
    if let Some(t) = try_host_port(text, word_start, word_end) {
        return Some(t);
    }
    // Git hash（纯 hex 7-40 位）
    if let Some(t) = try_git_hash(text, word_start, word_end) {
        return Some(t);
    }
    // 路径（含 / 或 ~ 或已知相对路径特征）
    if let Some(t) = try_path(text, word_start, word_end) {
        return Some(t);
    }
    // 兜底：identifier
    Some(SmartTarget {
        kind: SmartTargetKind::Identifier,
        start: word_start,
        end: word_end,
    })
}

/// Match a target at a terminal display column and return both byte and UI
/// coordinate ranges. `end_display_col` and `end_char` are exclusive.
#[must_use]
pub fn match_display_at(text: &str, display_col: usize) -> Option<SmartDisplayTarget> {
    let byte_idx = byte_index_at_display_col(text, display_col)?;
    let target = match_at(text, byte_idx)?;
    Some(SmartDisplayTarget {
        start_display_col: display_col_at_byte(text, target.start),
        end_display_col: display_col_at_byte(text, target.end),
        start_char: text[..target.start].chars().count(),
        end_char: text[..target.end].chars().count(),
        target,
    })
}

/// Convert a character index (used by BlockView hit testing) to a byte index.
#[must_use]
pub fn byte_index_at_char(text: &str, char_index: usize) -> Option<usize> {
    if char_index == text.chars().count() {
        return Some(text.len());
    }
    text.char_indices().nth(char_index).map(|(byte, _)| byte)
}

fn byte_index_at_display_col(text: &str, target_col: usize) -> Option<usize> {
    let mut col = 0usize;
    for (byte, ch) in text.char_indices() {
        let width = terminal_char_width(ch);
        if width == 0 {
            continue;
        }
        if target_col < col.saturating_add(width) {
            return Some(byte);
        }
        col = col.saturating_add(width);
    }
    None
}

fn display_col_at_byte(text: &str, byte_idx: usize) -> usize {
    text[..byte_idx.min(text.len())]
        .chars()
        .map(terminal_char_width)
        .sum()
}

/// 把任意字节偏移对齐到 char 边界（向后退到 char 起点）。
fn align_to_char_boundary(text: &str, mut idx: usize) -> usize {
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// 向两侧扩展到 token 边界。
///
/// 边界规则（保守，避免把无关文本算进目标）：
/// 1. 非转义空白是边界；`\ ` 可出现在终端打印的路径中
/// 2. East Asian Wide 字符（display width == 2）与 narrow 字符之间是边界
///    —— 路径分隔符两侧除外，因此 `/tmp/中文/main.rs` 保持完整
/// 3. narrow 内部不切标点（URL/path/email/hash 含 `:/.@-`）
fn expand_word(text: &str, idx: usize) -> (usize, usize) {
    let mut start = idx;
    while start > 0 {
        let prev = prev_char_start(text, start);
        let Some(prev_ch) = text[prev..].chars().next() else {
            break;
        };
        let Some(current_ch) = text[start..].chars().next() else {
            break;
        };
        if prev_ch.is_whitespace() && !is_escaped_at(text, prev) {
            break;
        }
        if width_class_differs(prev_ch, current_ch) && !path_boundary_bridge(prev_ch, current_ch) {
            break;
        }
        start = prev;
    }

    let mut end = idx + text[idx..].chars().next().map_or(0, char::len_utf8);
    while end < text.len() {
        let Some(next_ch) = text[end..].chars().next() else {
            break;
        };
        let prev = prev_char_start(text, end);
        let Some(prev_ch) = text[prev..].chars().next() else {
            break;
        };
        if next_ch.is_whitespace() && !is_escaped_at(text, end) {
            break;
        }
        if width_class_differs(prev_ch, next_ch) && !path_boundary_bridge(prev_ch, next_ch) {
            break;
        }
        end += next_ch.len_utf8();
    }
    (start, end)
}

fn width_class_differs(left: char, right: char) -> bool {
    (terminal_char_width(left) == 2) != (terminal_char_width(right) == 2)
}

fn path_boundary_bridge(left: char, right: char) -> bool {
    matches!(left, '/' | '\\') || matches!(right, '/' | '\\')
}

fn is_escaped_at(text: &str, byte: usize) -> bool {
    let mut cursor = byte;
    let mut slashes = 0usize;
    while cursor > 0 {
        cursor = prev_char_start(text, cursor);
        if !text[cursor..].starts_with('\\') {
            break;
        }
        slashes += 1;
    }
    slashes % 2 == 1
}

/// 返回 `pos` 之前一个 char 的起点（pos 必须 > 0）。
/// S7 修复：pos == 0 时返回 0（防御未来调用方），而非下溢。
fn prev_char_start(text: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut p = pos;
    while p > 1 && !text.is_char_boundary(p - 1) {
        p -= 1;
    }
    p - 1
}

/// 修剪 word 两端的常见包围标点（括号、引号、逗号、句末点），返回净范围。
/// 用于避免把 `(https://example.com)` 的 `)` 或句末 `a@b.com.` 的 `.` 算进目标。
fn trim_brackets(text: &str, start: usize, end: usize) -> (usize, usize) {
    let mut s = start;
    let mut e = end;
    // 左侧修剪：开头是开括号/引号则右移
    while s < e {
        let Some(ch) = text[s..].chars().next() else {
            break;
        };
        if matches!(ch, '(' | '[' | '{' | '<' | '"' | '\'' | '`' | '“' | '‘') {
            s += ch.len_utf8();
        } else {
            break;
        }
    }
    // 右侧修剪：末尾是闭括号/引号/句末分隔符则左移
    while e > s {
        let p = prev_char_start(text, e);
        let Some(ch) = text[p..].chars().next() else {
            break;
        };
        if matches!(
            ch,
            ')' | ']' | '}' | '>' | '"' | '\'' | '`' | '”' | '’' | ',' | ';'
        ) {
            e = p;
        } else {
            break;
        }
    }
    // B4 修复：句末单个 `.` 修剪——但保护 URL/path 末尾的合法 `.`（如 `./`、`../`）
    // 规则：仅当 `.` 是最末字符，且前一字符不是 `/` `\` 时修剪
    while e > s {
        let p = prev_char_start(text, e);
        let Some(dot) = text[p..].chars().next() else {
            break;
        };
        if dot != '.' {
            break;
        }
        if p > s {
            let pp = prev_char_start(text, p);
            if let Some(prev_ch) = text[pp..].chars().next() {
                if prev_ch == '/' || prev_ch == '\\' {
                    break; // 合法尾点（如 `config/.`），保留
                }
            }
        }
        e = p;
    }
    (s, e)
}

/// 检测 `path:line:column` / `path:line` 模式。
///
/// word 已扩展为完整 ASCII token（如 `src/lib.rs:42:8`）。在 word 内部查找：
/// 路径/文件名前缀 + `:num` + 可选 `:num`。
///
/// 接受裸文件名（`main.rs:42`、`lib.rs:10:5`）——编译器/grep/make 的典型输出。
fn try_path_line_column(text: &str, word_start: usize, word_end: usize) -> Option<SmartTarget> {
    // B3 修复：先 trim 包围括号，与其他 matcher 对称
    let (ws, we) = trim_brackets(text, word_start, word_end);
    let word = &text[ws..we];
    // 必须含至少一个 `:` 且 `:` 后跟数字
    let colon_idx = word.rfind(':')?;
    let after_colon = &word[colon_idx + 1..];
    if after_colon.is_empty() {
        return None;
    }
    // 末段必须是纯数字（line 或 column）
    if !after_colon.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    // colon 之前是 path[:num] 形式
    let before = &word[..colon_idx];
    // 可能有第二个 `:num`（column）
    let path_part = if let Some(colon2) = before.rfind(':') {
        let mid = &before[colon2 + 1..];
        if !mid.is_empty() && mid.chars().all(|c| c.is_ascii_digit()) {
            &before[..colon2]
        } else {
            before
        }
    } else {
        before
    };

    // path 部分必须像路径或文件名（B1/B2 修复：放宽到 filename）
    if !looks_like_path(path_part) && !looks_like_filename(path_part) {
        return None;
    }
    Some(SmartTarget {
        kind: SmartTargetKind::PathLineColumn,
        start: ws,
        end: we,
    })
}

/// 检测 URL：`scheme://...` 或 `mailto:...`。
fn try_url(text: &str, start: usize, end: usize) -> Option<SmartTarget> {
    let (s, e) = trim_brackets(text, start, end);
    let candidate = &text[s..e];
    // S1 修复：零分配 scheme 匹配——所有 scheme 前缀都是 ASCII 小写。
    // 安全：用 get(..sc.len()) 切片，CJK candidate 切到 char 中间时返回 None。
    let schemes = ["https://", "http://", "ftp://", "mailto:", "file://"];
    let matched = schemes.iter().copied().find(|sc| {
        candidate
            .get(..sc.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(sc))
    });
    if matched.is_some() {
        return Some(SmartTarget {
            kind: SmartTargetKind::Url,
            start: s,
            end: e,
        });
    }
    None
}

/// 检测 email：含 `@` 且两侧是标识符字符。
fn try_email(text: &str, start: usize, end: usize) -> Option<SmartTarget> {
    let (s, e) = trim_brackets(text, start, end);
    let candidate = &text[s..e];
    let at = candidate.find('@')?;
    if at == 0 || at == candidate.len() - 1 {
        return None;
    }
    let local = &candidate[..at];
    let domain = &candidate[at + 1..];
    // local: 至少 1 个非 @ 的 word char
    if local.is_empty() || !local.chars().next()?.is_alphanumeric() {
        return None;
    }
    // domain: 含 `.` 且末段至少 2 字符（粗略 TLD 校验）
    if !domain.contains('.') {
        return None;
    }
    let last_segment = domain.rsplit('.').next()?;
    if last_segment.len() < 2 {
        return None;
    }
    if !last_segment.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(SmartTarget {
        kind: SmartTargetKind::Email,
        start: s,
        end: e,
    })
}

/// 检测 `host:port` 或纯 IP/host（无端口时降级为 IpOrHost）。
/// 支持 `[user@]host:port` 形式（SSH 常见）。
fn try_host_port(text: &str, start: usize, end: usize) -> Option<SmartTarget> {
    let (s, e) = trim_brackets(text, start, end);
    let candidate = &text[s..e];
    // 必须含 `:`，否则不是 host:port
    let Some(colon) = candidate.rfind(':') else {
        // 但可能是纯 IP（含 `.`，4 段数字）
        if is_ipv4(candidate) || is_ipv6(candidate) {
            return Some(SmartTarget {
                kind: SmartTargetKind::IpOrHost,
                start: s,
                end: e,
            });
        }
        return None;
    };
    let port_str = &candidate[colon + 1..];
    // port 必须是纯数字 1-5 位
    if port_str.is_empty() || port_str.len() > 5 || !port_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let port: u32 = port_str.parse().ok()?;
    if port > 65535 {
        return None;
    }
    // host 部分可能含 `user@`，剥掉再校验
    let host_with_user = &candidate[..colon];
    let host = host_with_user
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(host_with_user);
    // host 合法性：IP 或域名（含 `.` 或 localhost）
    if host == "localhost" || is_ipv4(host) || is_ipv6(host) || looks_like_domain(host) {
        return Some(SmartTarget {
            kind: SmartTargetKind::HostPort,
            start: s,
            end: e,
        });
    }
    None
}

/// 检测 Git hash：纯 hex，7-40 位，词边界。
fn try_git_hash(text: &str, start: usize, end: usize) -> Option<SmartTarget> {
    let candidate = &text[start..end];
    let len = candidate.len();
    // 必须 7-40 位纯 hex
    if !(7..=40).contains(&len) {
        return None;
    }
    if !candidate.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    // 必须至少有 1 个字母（避免把纯数字当 hash）
    if !candidate.chars().any(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(SmartTarget {
        kind: SmartTargetKind::GitHash,
        start,
        end,
    })
}

/// 检测路径：含 `/`、`~` 或已知相对路径前缀，或像文件名。
fn try_path(text: &str, start: usize, end: usize) -> Option<SmartTarget> {
    let candidate = &text[start..end];
    if looks_like_path(candidate) || looks_like_filename(candidate) {
        Some(SmartTarget {
            kind: SmartTargetKind::Path,
            start,
            end,
        })
    } else {
        None
    }
}

/// 启发式：字符串是否像路径。保守——只接受明确的路径特征。
fn looks_like_path(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // 绝对路径 / home 路径 / 含分隔符
    if s.starts_with('/') || s.starts_with("~/") || s.starts_with("./") || s.contains('/') {
        return true;
    }
    // Windows 盘符（C:\）——macOS 项目少见但兼容
    if s.len() >= 3 {
        let bytes = s.as_bytes();
        if bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
            return true;
        }
    }
    false
}

/// 启发式：字符串是否像文件名（含扩展名）。
/// 接受 `lib.rs`、`main.rs`、`Cargo.toml`；拒绝纯数字、纯字母词、域名。
/// 规则：含 `.`，扩展名段至少 1 个字母，主体段至少 1 字符。
fn looks_like_filename(s: &str) -> bool {
    if s.is_empty() || !s.contains('.') {
        return false;
    }
    // 排除域名（含多个 `.` 且纯字母数字+连字符的，更像 host）——简化：
    // filename 的扩展名段通常较短（1-5 字符）且全字母。
    let last_dot = s.rfind('.').unwrap();
    let ext = &s[last_dot + 1..];
    let stem = &s[..last_dot];
    if stem.is_empty() || ext.is_empty() {
        return false;
    }
    // 扩展名：1-5 字符，全字母（rs, ts, toml, py, go, c, h, js...）
    if ext.len() > 5 || !ext.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    // 主体：至少 1 个字母或路径分隔符（排除 `1.0`、`0.5` 等版本号）
    if !stem
        .chars()
        .any(|c| c.is_ascii_alphabetic() || c == '/' || c == '\\')
    {
        return false;
    }
    // 主体不能含 `@`（email）或空格
    if stem.contains('@') || stem.contains(' ') {
        return false;
    }
    true
}

/// 是否像域名：含 `.`，每段是字母数字，至少 2 段。
fn looks_like_domain(s: &str) -> bool {
    if !s.contains('.') || s.starts_with('.') || s.ends_with('.') {
        return false;
    }
    s.split('.')
        .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// 是否是 IPv4：4 段 0-255 数字。
fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.len() <= 3
            && p.chars().all(|c| c.is_ascii_digit())
            && p.parse::<u32>().map(|n| n <= 255).unwrap_or(false)
    })
}

/// 是否是 IPv6（简化：含多个 `:` 或被 `[]` 包裹）。完整 RFC 校验留给调用方。
fn is_ipv6(s: &str) -> bool {
    let inner = s
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(s);
    // 至少 2 个冒号（最简形式 `::1`）或含 hex+冒号
    inner.matches(':').count() >= 2 && inner.chars().all(|c| c.is_ascii_hexdigit() || c == ':')
}

#[cfg(test)]
mod tests;
