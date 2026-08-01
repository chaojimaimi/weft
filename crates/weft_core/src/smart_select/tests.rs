use super::*;

fn m(text: &str, byte_idx: usize) -> Option<SmartTargetKind> {
    match_at(text, byte_idx).map(|t| t.kind)
}

// ---------- 基本优先级（V110_PLAN §3.2）----------

#[test]
fn url_at_cursor() {
    let s = "see https://example.com/path?q=1 now";
    let idx = s.find("example").unwrap();
    assert_eq!(m(s, idx), Some(SmartTargetKind::Url));
}

#[test]
fn http_url_priority_over_path() {
    // URL 含 `/` 但应是 URL，非 Path
    let s = "open https://a.com/b";
    let idx = s.find("a.com").unwrap();
    assert_eq!(m(s, idx), Some(SmartTargetKind::Url));
}

#[test]
fn email_at_cursor() {
    let s = "contact user@example.com for details";
    let idx = s.find("example").unwrap();
    assert_eq!(m(s, idx), Some(SmartTargetKind::Email));
}

#[test]
fn host_port_at_cursor() {
    let s = "ssh root@1.2.3.4:2222 then";
    let idx = s.find("2222").unwrap();
    assert_eq!(m(s, idx), Some(SmartTargetKind::HostPort));
}

#[test]
fn ipv4_without_port() {
    let s = "ping 192.168.1.1 to test";
    let idx = s.find("168").unwrap();
    assert_eq!(m(s, idx), Some(SmartTargetKind::IpOrHost));
}

#[test]
fn git_hash_7char() {
    let s = "fixed in abc1234 last week";
    let idx = s.find("abc1").unwrap();
    assert_eq!(m(s, idx), Some(SmartTargetKind::GitHash));
}

#[test]
fn git_hash_40char() {
    let s = "commit deadbeefcafebabe1234567890abcdef12345678 done";
    let idx = s.find("dead").unwrap();
    assert_eq!(m(s, idx), Some(SmartTargetKind::GitHash));
}

#[test]
fn path_at_cursor() {
    let s = "edit src/lib.rs to fix";
    let idx = s.find("lib").unwrap();
    assert_eq!(m(s, idx), Some(SmartTargetKind::Path));
}

#[test]
fn path_line_column_highest_priority() {
    let s = "src/lib.rs:42:8 error here";
    let idx = s.find("lib").unwrap();
    assert_eq!(m(s, idx), Some(SmartTargetKind::PathLineColumn));
    // 完整范围覆盖到 column
    let t = match_at(s, idx).unwrap();
    assert_eq!(&s[t.start..t.end], "src/lib.rs:42:8");
}

#[test]
fn path_line_only() {
    let s = "see ./main.rs:15 for";
    let idx = s.find("main").unwrap();
    assert_eq!(m(s, idx), Some(SmartTargetKind::PathLineColumn));
    let t = match_at(s, idx).unwrap();
    assert_eq!(&s[t.start..t.end], "./main.rs:15");
}

// ---------- 边界修剪（V110_PLAN §5 退出标准：标点边界）----------

#[test]
fn url_in_parens_not_include_closing() {
    let s = "see (https://example.com) for";
    let idx = s.find("example").unwrap();
    let t = match_at(s, idx).unwrap();
    assert_eq!(t.kind, SmartTargetKind::Url);
    assert_eq!(&s[t.start..t.end], "https://example.com");
}

#[test]
fn url_in_brackets() {
    let s = "ref [https://example.com] now";
    let idx = s.find("example").unwrap();
    let t = match_at(s, idx).unwrap();
    assert_eq!(&s[t.start..t.end], "https://example.com");
}

#[test]
fn url_in_angles() {
    let s = "<https://example.com>";
    let t = match_at(s, 5).unwrap();
    assert_eq!(&s[t.start..t.end], "https://example.com");
}

// ---------- CJK / 全角边界（V110_PLAN §5 退出标准核心）----------

#[test]
fn url_adjacent_to_cjk() {
    // CJK 紧贴 URL，不应把 CJK 算进 URL
    let s = "访问https://example.com查看";
    let idx = s.find("example").unwrap();
    let t = match_at(s, idx).unwrap();
    assert_eq!(t.kind, SmartTargetKind::Url);
    assert_eq!(&s[t.start..t.end], "https://example.com");
    // 起点不包含"访问"，终点不包含"查看"
    assert!(!s[t.start..t.end].contains('访'));
    assert!(!s[t.start..t.end].contains('查'));
}

#[test]
fn path_surrounded_by_cjk() {
    let s = "编辑src/main.rs文件";
    let idx = s.find("main").unwrap();
    let t = match_at(s, idx).unwrap();
    assert_eq!(t.kind, SmartTargetKind::Path);
    assert_eq!(&s[t.start..t.end], "src/main.rs");
}

#[test]
fn cjk_word_is_identifier() {
    // 纯 CJK（无 URL/路径特征）回退到 Identifier，不 panic
    let s = "你好世界";
    let idx = s.find("好").unwrap();
    assert_eq!(m(s, idx), Some(SmartTargetKind::Identifier));
}

// ---------- 恶意输入安全（V110_PLAN §3.2 "禁止隐式执行"）----------

#[test]
fn shell_metacharacters_only_selected_not_executed() {
    // matcher 只返回范围，不执行。`;rm -rf` 应被识别为标识符，
    // is_safe_to_open 应为 false，调用方据此决定不打开。
    let s = "foo;rm -rf /";
    let idx = s.find("foo").unwrap();
    let t = match_at(s, idx).unwrap();
    assert!(!t.kind.is_safe_to_open() || t.kind == SmartTargetKind::Identifier);
}

#[test]
fn file_scheme_recognized_but_caller_must_validate() {
    // file:// 识别为 URL 类型，但 is_safe_to_open 对 Url 返回 true
    // 这是钩子；调用方仍必须做路径存在性 + 越界校验（V110_PLAN §5.5）
    let s = "open file:///etc/passwd here";
    let idx = s.find("etc").unwrap();
    let t = match_at(s, idx).unwrap();
    assert_eq!(t.kind, SmartTargetKind::Url);
}

#[test]
fn whitespace_cursor_returns_none() {
    assert_eq!(m("a b", 1), None);
    assert_eq!(m("   ", 0), None);
}

#[test]
fn empty_string_returns_none() {
    assert_eq!(m("", 0), None);
}

#[test]
fn out_of_range_returns_none_or_aligned() {
    // 不应 panic
    let _ = m("abc", 100);
}

#[test]
fn display_mapping_handles_cjk_and_emoji_boundaries() {
    let text = "中 https://example.com 🔬";
    let target = match_display_at(text, 5).unwrap();
    assert_eq!(target.target.kind, SmartTargetKind::Url);
    assert_eq!(target.target.text(text), "https://example.com");
    assert_eq!(target.start_display_col, 3);
    assert_eq!(target.end_display_col, 22);
    assert_eq!(
        &text[target.target.start..target.target.end],
        "https://example.com"
    );
}

#[test]
fn char_to_byte_mapping_is_unicode_safe() {
    let text = "中a🔬";
    assert_eq!(byte_index_at_char(text, 0), Some(0));
    assert_eq!(byte_index_at_char(text, 1), Some("中".len()));
    assert_eq!(byte_index_at_char(text, 3), Some(text.len()));
    assert_eq!(byte_index_at_char(text, 4), None);
}

#[test]
fn combining_mark_does_not_capture_the_next_display_cell() {
    let text = "e\u{301}中";
    let target = match_display_at(text, 1).unwrap();
    assert_eq!(target.target.text(text), "中");
}

#[test]
fn cjk_and_escaped_space_paths_stay_whole() {
    for (text, needle, expected) in [
        ("open /tmp/中文/main.rs now", "main", "/tmp/中文/main.rs"),
        ("open /tmp/my\\ file.rs now", "file", "/tmp/my\\ file.rs"),
    ] {
        let target = match_at(text, text.find(needle).unwrap()).unwrap();
        assert_eq!(target.kind, SmartTargetKind::Path);
        assert_eq!(target.text(text), expected);
    }
}

// ---------- 优先级冲突仲裁 ----------

#[test]
fn url_beats_path_when_scheme_present() {
    // https://a.com 含 `/`，但优先识别为 URL 而非 Path
    let s = "https://a.com/b";
    let idx = s.find("a.com").unwrap();
    assert_eq!(m(s, idx), Some(SmartTargetKind::Url));
}

#[test]
fn path_line_column_beats_url_when_no_scheme() {
    // /var/log:1 没有 scheme，但 path:line 模式
    let s = "/var/log:1";
    let idx = s.find("var").unwrap();
    assert_eq!(m(s, idx), Some(SmartTargetKind::PathLineColumn));
}

// ---------- 范围正确性 ----------

#[test]
fn identifier_fallback_single_word() {
    let s = "run cargo build";
    let idx = s.find("cargo").unwrap();
    let t = match_at(s, idx).unwrap();
    assert_eq!(t.kind, SmartTargetKind::Identifier);
    assert_eq!(&s[t.start..t.end], "cargo");
}

#[test]
fn tilde_path() {
    let s = "cat ~/.config/weft";
    let idx = s.find("config").unwrap();
    let t = match_at(s, idx).unwrap();
    assert_eq!(t.kind, SmartTargetKind::Path);
    assert_eq!(&s[t.start..t.end], "~/.config/weft");
}

// ---------- rust-reviewer B1-B4 回归（裸文件名 / 句末点 / 括号）----------

#[test]
fn bare_filename_with_line() {
    // B1/B2: 编译器典型输出，无路径前缀
    let s = "error in main.rs:42 here";
    let idx = s.find("main").unwrap();
    let t = match_at(s, idx).unwrap();
    assert_eq!(t.kind, SmartTargetKind::PathLineColumn);
    assert_eq!(&s[t.start..t.end], "main.rs:42");
}

#[test]
fn bare_filename_with_line_and_column() {
    // B2: lib.rs:10:5（行:列）
    let s = "see lib.rs:10:5 for";
    let idx = s.find("lib").unwrap();
    let t = match_at(s, idx).unwrap();
    assert_eq!(t.kind, SmartTargetKind::PathLineColumn);
    assert_eq!(&s[t.start..t.end], "lib.rs:10:5");
}

#[test]
fn filename_bare_without_line_is_path() {
    // 无 :num 后缀的裸文件名 → Path（非 Identifier）
    let s = "edit Cargo.toml now";
    let idx = s.find("Cargo").unwrap();
    let t = match_at(s, idx).unwrap();
    assert_eq!(t.kind, SmartTargetKind::Path);
    assert_eq!(&s[t.start..t.end], "Cargo.toml");
}

#[test]
fn path_line_column_in_parens_trimmed() {
    // B3: (src/main.rs:42) 不应含括号
    let s = "see (src/main.rs:42) for";
    let idx = s.find("main").unwrap();
    let t = match_at(s, idx).unwrap();
    assert_eq!(t.kind, SmartTargetKind::PathLineColumn);
    assert_eq!(&s[t.start..t.end], "src/main.rs:42");
}

#[test]
fn url_trailing_period_trimmed() {
    // B4: 句末 https://x.com. 的 `.` 应修剪
    let s = "visit https://x.com.";
    let idx = s.find("x.com").unwrap();
    let t = match_at(s, idx).unwrap();
    assert_eq!(t.kind, SmartTargetKind::Url);
    assert_eq!(&s[t.start..t.end], "https://x.com");
}

#[test]
fn email_trailing_period_detected() {
    // B4: a@b.com. 应检测为 Email（而非降级 Identifier）
    let s = "mail me at a@b.com. please";
    let idx = s.find("b.com").unwrap();
    let t = match_at(s, idx).unwrap();
    assert_eq!(t.kind, SmartTargetKind::Email);
    assert_eq!(&s[t.start..t.end], "a@b.com");
}

#[test]
fn url_path_trailing_slash_dot_preserved() {
    // B4 保护：`config/.` 末尾的合法 `.` 不应被修剪
    // （虽然这是路径，但验证 trim_brackets 不会误删）
    let s = "cat ./config/.";
    let idx = s.find("config").unwrap();
    let t = match_at(s, idx).unwrap();
    // 整个 ./config/. 是 Path，末尾 . 保留
    assert_eq!(t.kind, SmartTargetKind::Path);
    assert_eq!(&s[t.start..t.end], "./config/.");
}

#[test]
fn bare_filename_not_confused_with_hostport() {
    // B1 关键：main.rs:42 不应是 HostPort（port=42, host=main.rs）
    // 优先级：PathLineColumn > HostPort
    let s = "fix main.rs:42 now";
    let idx = s.find("main").unwrap();
    let kind = m(s, idx);
    assert_eq!(kind, Some(SmartTargetKind::PathLineColumn));
}
