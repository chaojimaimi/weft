use super::*;

fn open_test_index() -> SearchIndex {
    let conn = Connection::open_in_memory().unwrap();
    SearchIndex::open(conn).unwrap()
}

fn make_doc(kind: SearchDocumentKind, id: &str, title: &str, body: &str) -> SearchDocument {
    SearchDocument {
        kind,
        stable_id: id.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        cwd: Some("/home/user".to_string()),
        updated_at: 1700000000,
    }
}

#[test]
fn open_and_probe_fts5() {
    let idx = open_test_index();
    // The bundled SQLite has FTS5 enabled, so this should be true.
    // (If running on a system without FTS5, this test is still valid
    // — it just verifies the probe doesn't panic.)
    let _ = idx.fts5_available();
}

#[test]
fn upsert_and_search_basic() {
    let idx = open_test_index();
    let doc = make_doc(
        SearchDocumentKind::Block,
        "1",
        "cargo build",
        "Compiling weft v1.7.0",
    );
    idx.upsert(&doc).unwrap();
    let hits = idx.search(&SearchQuery::new("cargo")).unwrap();
    assert!(!hits.is_empty(), "should find 'cargo'");
    assert_eq!(hits[0].doc.title, "cargo build");
}

#[test]
fn upsert_replaces_existing() {
    let idx = open_test_index();
    let doc1 = make_doc(SearchDocumentKind::Block, "1", "old command", "old output");
    idx.upsert(&doc1).unwrap();
    let doc2 = make_doc(SearchDocumentKind::Block, "1", "new command", "new output");
    idx.upsert(&doc2).unwrap();
    assert_eq!(idx.count().unwrap(), 1);
    let hits = idx.search(&SearchQuery::new("new")).unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].doc.title, "new command");
}

#[test]
fn delete_removes_document() {
    let idx = open_test_index();
    let doc = make_doc(SearchDocumentKind::Block, "1", "delete me", "some output");
    idx.upsert(&doc).unwrap();
    assert_eq!(idx.count().unwrap(), 1);
    idx.delete(SearchDocumentKind::Block, "1").unwrap();
    assert_eq!(idx.count().unwrap(), 0);
}

#[test]
fn delete_kind_removes_all_of_kind() {
    let idx = open_test_index();
    idx.upsert(&make_doc(SearchDocumentKind::Block, "1", "cmd1", "out1"))
        .unwrap();
    idx.upsert(&make_doc(SearchDocumentKind::Block, "2", "cmd2", "out2"))
        .unwrap();
    idx.upsert(&make_doc(
        SearchDocumentKind::Workflow,
        "w1",
        "workflow1",
        "template1",
    ))
    .unwrap();
    assert_eq!(idx.count().unwrap(), 3);
    idx.delete_kind(SearchDocumentKind::Block).unwrap();
    assert_eq!(idx.count().unwrap(), 1);
    assert_eq!(idx.count_kind(SearchDocumentKind::Workflow).unwrap(), 1);
}

#[test]
fn rebuild_clears_and_reinserts() {
    let idx = open_test_index();
    idx.upsert(&make_doc(
        SearchDocumentKind::Block,
        "old",
        "old cmd",
        "old out",
    ))
    .unwrap();
    assert_eq!(idx.count().unwrap(), 1);

    let new_docs = vec![
        make_doc(SearchDocumentKind::Block, "1", "new1", "out1"),
        make_doc(SearchDocumentKind::Block, "2", "new2", "out2"),
    ];
    idx.rebuild(&new_docs).unwrap();
    assert_eq!(idx.count().unwrap(), 2);
    // Old doc should be gone.
    let hits = idx.search(&SearchQuery::new("old")).unwrap();
    assert!(hits.is_empty());
    // New docs should be found.
    let hits = idx.search(&SearchQuery::new("new")).unwrap();
    assert_eq!(hits.len(), 2);
}

#[test]
fn search_empty_query_returns_empty() {
    let idx = open_test_index();
    idx.upsert(&make_doc(SearchDocumentKind::Block, "1", "test", "content"))
        .unwrap();
    let hits = idx.search(&SearchQuery::new("")).unwrap();
    assert!(hits.is_empty());
}

#[test]
fn search_no_match_returns_empty() {
    let idx = open_test_index();
    idx.upsert(&make_doc(SearchDocumentKind::Block, "1", "hello", "world"))
        .unwrap();
    let hits = idx.search(&SearchQuery::new("nonexistent")).unwrap();
    assert!(hits.is_empty());
}

#[test]
fn search_cjk_content() {
    let idx = open_test_index();
    let doc = SearchDocument {
        kind: SearchDocumentKind::Block,
        stable_id: "1".to_string(),
        title: "ls -la".to_string(),
        body: "文件名 中文测试.txt".to_string(),
        cwd: None,
        updated_at: 0,
    };
    idx.upsert(&doc).unwrap();
    let hits = idx.search(&SearchQuery::new("中文")).unwrap();
    assert!(!hits.is_empty(), "should find CJK content");
}

#[test]
fn search_path_content() {
    let idx = open_test_index();
    let doc = SearchDocument {
        kind: SearchDocumentKind::Block,
        stable_id: "1".to_string(),
        title: "cat /usr/local/bin/script".to_string(),
        body: "script output".to_string(),
        cwd: Some("/usr/local".to_string()),
        updated_at: 0,
    };
    idx.upsert(&doc).unwrap();
    let hits = idx.search(&SearchQuery::new("/usr/local")).unwrap();
    assert!(!hits.is_empty());
}

#[test]
fn search_case_insensitive() {
    let idx = open_test_index();
    idx.upsert(&make_doc(
        SearchDocumentKind::Block,
        "1",
        "Cargo Build",
        "Output",
    ))
    .unwrap();
    let hits_lower = idx.search(&SearchQuery::new("cargo")).unwrap();
    assert!(!hits_lower.is_empty(), "lowercase should match");
    let hits_upper = idx.search(&SearchQuery::new("CARGO")).unwrap();
    assert!(!hits_upper.is_empty(), "uppercase should match");
}

#[test]
fn search_special_characters() {
    let idx = open_test_index();
    let doc = make_doc(
        SearchDocumentKind::Block,
        "1",
        "echo 'hello & world'",
        "hello & world",
    );
    idx.upsert(&doc).unwrap();
    // The query "hello" should match despite the special chars.
    let hits = idx.search(&SearchQuery::new("hello")).unwrap();
    assert!(!hits.is_empty());
}

#[test]
fn search_kind_filter() {
    let idx = open_test_index();
    idx.upsert(&make_doc(
        SearchDocumentKind::Block,
        "1",
        "test block",
        "content",
    ))
    .unwrap();
    idx.upsert(&make_doc(
        SearchDocumentKind::Workflow,
        "w1",
        "test workflow",
        "template",
    ))
    .unwrap();

    let hits_all = idx.search(&SearchQuery::new("test")).unwrap();
    assert_eq!(hits_all.len(), 2);

    let q = SearchQuery {
        query: "test",
        kinds: &[SearchDocumentKind::Block],
        limit: 50,
        cwd: None,
    };
    let hits_block = idx.search(&q).unwrap();
    assert_eq!(hits_block.len(), 1);
    assert_eq!(hits_block[0].doc.kind, SearchDocumentKind::Block);
}

#[test]
fn search_limit() {
    let idx = open_test_index();
    for i in 0..100 {
        idx.upsert(&make_doc(
            SearchDocumentKind::Block,
            &i.to_string(),
            &format!("test{i}"),
            "common content",
        ))
        .unwrap();
    }
    let q = SearchQuery {
        query: "common",
        kinds: &[],
        limit: 10,
        cwd: None,
    };
    let hits = idx.search(&q).unwrap();
    assert_eq!(hits.len(), 10);
}

#[test]
fn escape_fts5_doubles_quotes() {
    assert_eq!(escape_fts5_query("hello"), "hello");
    assert_eq!(escape_fts5_query(r#"hello "world""#), r#"hello ""world"""#);
}

#[test]
fn rank_hits_cwd_boost() {
    let hits = vec![
        SearchHit {
            doc: SearchDocument {
                kind: SearchDocumentKind::Block,
                stable_id: "1".to_string(),
                title: "cmd".to_string(),
                body: "out".to_string(),
                cwd: Some("/home/user/project".to_string()),
                updated_at: 0,
            },
            score: 1.0,
        },
        SearchHit {
            doc: SearchDocument {
                kind: SearchDocumentKind::Block,
                stable_id: "2".to_string(),
                title: "cmd".to_string(),
                body: "out".to_string(),
                cwd: Some("/other/path".to_string()),
                updated_at: 0,
            },
            score: 1.0,
        },
    ];
    let ranked = rank_hits(hits, Some("/home/user"));
    // The CWD-matching doc should be first (lower score = better).
    assert_eq!(ranked[0].doc.stable_id, "1");
    assert!(ranked[0].score < ranked[1].score);
}

#[test]
fn rank_hits_no_cwd_preserves_order() {
    let hits = vec![
        SearchHit {
            doc: SearchDocument {
                kind: SearchDocumentKind::Block,
                stable_id: "1".to_string(),
                title: "a".to_string(),
                body: "".to_string(),
                cwd: None,
                updated_at: 0,
            },
            score: 1.0,
        },
        SearchHit {
            doc: SearchDocument {
                kind: SearchDocumentKind::Block,
                stable_id: "2".to_string(),
                title: "b".to_string(),
                body: "".to_string(),
                cwd: None,
                updated_at: 0,
            },
            score: 2.0,
        },
    ];
    let ranked = rank_hits(hits, None);
    // No CWD boost — order preserved by score.
    assert_eq!(ranked[0].doc.stable_id, "1");
    assert_eq!(ranked[1].doc.stable_id, "2");
}

#[test]
fn search_applies_cwd_ranking_in_production_path() {
    let idx = open_test_index();
    let mut remote = make_doc(
        SearchDocumentKind::Block,
        "remote",
        "cargo build",
        "same searchable output",
    );
    remote.cwd = Some("/other/project".to_string());
    let mut local = make_doc(
        SearchDocumentKind::Block,
        "local",
        "cargo build",
        "same searchable output",
    );
    local.cwd = Some("/home/user/project".to_string());
    idx.upsert(&remote).unwrap();
    idx.upsert(&local).unwrap();

    let hits = idx
        .search(&SearchQuery {
            query: "cargo",
            kinds: &[],
            limit: 10,
            cwd: Some("/home/user/project"),
        })
        .unwrap();

    assert_eq!(hits[0].doc.stable_id, "local");
}

#[test]
fn cwd_ranking_considers_candidates_beyond_requested_limit() {
    let idx = open_test_index();
    for id in 0..4 {
        let mut remote = make_doc(
            SearchDocumentKind::Block,
            &format!("remote-{id}"),
            "cargo build",
            "same output",
        );
        remote.cwd = Some(format!("/other/{id}"));
        idx.upsert(&remote).unwrap();
    }

    let mut local = make_doc(
        SearchDocumentKind::Block,
        "local",
        "cargo build",
        "same output",
    );
    local.cwd = Some("/repo".to_string());
    idx.upsert(&local).unwrap();

    let hits = idx
        .search(&SearchQuery {
            query: "cargo",
            kinds: &[],
            limit: 1,
            cwd: Some("/repo"),
        })
        .unwrap();
    assert_eq!(hits[0].doc.stable_id, "local");
}

#[test]
fn cwd_ranking_respects_path_component_boundaries() {
    let hits = vec![SearchHit {
        doc: SearchDocument {
            kind: SearchDocumentKind::Block,
            stable_id: "other-user".to_string(),
            title: "cmd".to_string(),
            body: String::new(),
            cwd: Some("/home/user2/project".to_string()),
            updated_at: 0,
        },
        score: 1.0,
    }];
    let ranked = rank_hits(hits, Some("/home/user"));
    assert_eq!(ranked[0].score, 1.0);
}

#[test]
fn rebuild_is_idempotent() {
    let idx = open_test_index();
    let docs = vec![
        make_doc(SearchDocumentKind::Block, "1", "cmd1", "out1"),
        make_doc(SearchDocumentKind::Block, "2", "cmd2", "out2"),
    ];
    idx.rebuild(&docs).unwrap();
    assert_eq!(idx.count().unwrap(), 2);
    // Rebuild again with the same data.
    idx.rebuild(&docs).unwrap();
    assert_eq!(idx.count().unwrap(), 2);
}

#[test]
fn corrupted_index_can_be_rebuilt() {
    // Simulate corruption: drop the table, then rebuild.
    let idx = open_test_index();
    idx.upsert(&make_doc(SearchDocumentKind::Block, "1", "old", "old out"))
        .unwrap();
    // Simulate corruption.
    idx.conn
        .execute("DROP TABLE IF EXISTS search_docs;", [])
        .unwrap();
    // Rebuild should recreate the table.
    let docs = vec![make_doc(SearchDocumentKind::Block, "2", "new", "new out")];
    idx.rebuild(&docs).unwrap();
    assert_eq!(idx.count().unwrap(), 1);
    let hits = idx.search(&SearchQuery::new("new")).unwrap();
    assert!(!hits.is_empty());
}

#[test]
fn large_batch_rebuild() {
    // V17 §3 exit criteria: 10万条合成记录下查询 p95 < 50ms.
    // This test inserts 1000 docs (enough to validate batch insert
    // correctness; the 100K performance test is a benchmark).
    let idx = open_test_index();
    let docs: Vec<SearchDocument> = (0..1000)
        .map(|i| SearchDocument {
            kind: SearchDocumentKind::Block,
            stable_id: i.to_string(),
            title: format!("command_{i}"),
            body: format!("output line {i} with some content"),
            cwd: Some("/tmp".to_string()),
            updated_at: i,
        })
        .collect();
    idx.rebuild(&docs).unwrap();
    assert_eq!(idx.count().unwrap(), 1000);
    let hits = idx.search(&SearchQuery::new("command_500")).unwrap();
    assert!(!hits.is_empty());
}

#[test]
#[ignore = "release-only 100k search performance gate"]
fn search_100k_documents_p95_under_50ms() {
    let idx = open_test_index();
    let docs: Vec<SearchDocument> = (0..100_000)
        .map(|i| SearchDocument {
            kind: SearchDocumentKind::Block,
            stable_id: i.to_string(),
            title: format!("command_{i}"),
            body: format!("output {i} reusable searchable content"),
            cwd: Some(format!("/tmp/project/{}", i % 100)),
            updated_at: i,
        })
        .collect();
    idx.rebuild(&docs).unwrap();

    let mut samples = Vec::new();
    for _ in 0..30 {
        let started = std::time::Instant::now();
        let hits = idx
            .search(&SearchQuery {
                query: "command_99999",
                kinds: &[],
                limit: 50,
                cwd: Some("/tmp/project/99"),
            })
            .unwrap();
        assert_eq!(hits[0].doc.stable_id, "99999");
        samples.push(started.elapsed());
    }

    samples.sort_unstable();
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
    eprintln!("100k unified search p95: {p95:?}");
    assert!(p95 < std::time::Duration::from_millis(50), "p95={p95:?}");
}

#[test]
fn replace_kinds_preserves_unrelated_documents() {
    let idx = open_test_index();
    let workspace = make_doc(SearchDocumentKind::Workspace, "ws", "workspace", "body");
    let old_block = make_doc(SearchDocumentKind::Block, "old", "old command", "body");
    idx.rebuild(&[workspace, old_block]).unwrap();
    let new_block = make_doc(SearchDocumentKind::Block, "new", "new command", "body");
    idx.replace_kinds(&[SearchDocumentKind::Block], &[new_block])
        .unwrap();
    assert_eq!(idx.count_kind(SearchDocumentKind::Workspace).unwrap(), 1);
    assert!(idx
        .search(&SearchQuery::new("old command"))
        .unwrap()
        .is_empty());
    assert_eq!(
        idx.search(&SearchQuery::new("new command")).unwrap().len(),
        1
    );
}

#[test]
fn no_ghost_records_after_delete() {
    // V17 §3: "删除/更新源数据后索引无幽灵记录".
    let idx = open_test_index();
    idx.upsert(&make_doc(
        SearchDocumentKind::Block,
        "1",
        "unique_command",
        "unique_output",
    ))
    .unwrap();
    idx.delete(SearchDocumentKind::Block, "1").unwrap();
    let hits = idx.search(&SearchQuery::new("unique_command")).unwrap();
    assert!(hits.is_empty(), "ghost record found after delete");
    let hits = idx.search(&SearchQuery::new("unique_output")).unwrap();
    assert!(hits.is_empty(), "ghost record found after delete");
}

#[test]
fn no_ghost_records_after_rebuild() {
    let idx = open_test_index();
    idx.upsert(&make_doc(
        SearchDocumentKind::Block,
        "1",
        "ghost_command",
        "ghost_output",
    ))
    .unwrap();
    // Rebuild with different data — old doc should not survive.
    let new_docs = vec![make_doc(
        SearchDocumentKind::Block,
        "2",
        "real_command",
        "real_output",
    )];
    idx.rebuild(&new_docs).unwrap();
    let hits = idx.search(&SearchQuery::new("ghost")).unwrap();
    assert!(hits.is_empty(), "ghost record survived rebuild");
}

// ---- v1.7.3-D: Bookmark search integration acceptance tests ----

#[test]
fn bookmark_upsert_and_search_by_note() {
    // Acceptance: a bookmark with a note is searchable by note text.
    let idx = open_test_index();
    let doc = SearchDocument::from_bookmark(
        42,
        Some("deploy script for production"),
        "kubectl apply -f deploy.yaml",
        &[],
        Some("/repo"),
        1_700_000_000,
    );
    idx.upsert(&doc).unwrap();
    let hits = idx
        .search(&SearchQuery {
            query: "deploy",
            kinds: &[SearchDocumentKind::Bookmark],
            limit: 50,
            cwd: None,
        })
        .unwrap();
    assert!(!hits.is_empty(), "bookmark should be findable by note text");
    assert_eq!(hits[0].doc.kind, SearchDocumentKind::Bookmark);
    assert_eq!(hits[0].doc.stable_id, "42");
}

#[test]
fn bookmark_search_by_tag() {
    // Acceptance: a bookmark's tags are searchable.
    let idx = open_test_index();
    let tags = vec!["production".to_string(), "critical".to_string()];
    let doc = SearchDocument::from_bookmark(7, None, "git status", &tags, None, 0);
    idx.upsert(&doc).unwrap();
    let hits = idx.search(&SearchQuery::new("critical")).unwrap();
    assert!(!hits.is_empty(), "bookmark should be findable by tag");
    assert_eq!(hits[0].doc.stable_id, "7");
}

#[test]
fn bookmark_search_by_command_fallback() {
    // Acceptance: when a bookmark has no note, the block command is
    // used as the title and is searchable.
    let idx = open_test_index();
    let doc = SearchDocument::from_bookmark(99, None, "cargo build --release", &[], None, 0);
    idx.upsert(&doc).unwrap();
    let hits = idx.search(&SearchQuery::new("cargo")).unwrap();
    assert!(!hits.is_empty(), "bookmark should be findable by command");
    assert_eq!(hits[0].doc.title, "cargo build --release");
}

#[test]
fn bookmark_delete_removes_from_search() {
    // Acceptance: deleting a bookmark removes it from the search index
    // (no ghost records).
    let idx = open_test_index();
    let doc =
        SearchDocument::from_bookmark(5, Some("unique_note_text"), "some_command", &[], None, 0);
    idx.upsert(&doc).unwrap();
    assert!(idx.count_kind(SearchDocumentKind::Bookmark).unwrap() == 1);
    idx.delete(SearchDocumentKind::Bookmark, "5").unwrap();
    assert!(idx.count_kind(SearchDocumentKind::Bookmark).unwrap() == 0);
    let hits = idx.search(&SearchQuery::new("unique_note_text")).unwrap();
    assert!(hits.is_empty(), "deleted bookmark should not be findable");
}

#[test]
fn bookmark_kind_filter_excludes_blocks() {
    // Acceptance: filtering by Bookmark kind excludes Block documents.
    let idx = open_test_index();
    idx.upsert(&make_doc(
        SearchDocumentKind::Block,
        "1",
        "shared_term",
        "block output",
    ))
    .unwrap();
    let bm = SearchDocument::from_bookmark(1, Some("shared_term note"), "cmd", &[], None, 0);
    idx.upsert(&bm).unwrap();
    let q = SearchQuery {
        query: "shared_term",
        kinds: &[SearchDocumentKind::Bookmark],
        limit: 50,
        cwd: None,
    };
    let hits = idx.search(&q).unwrap();
    assert_eq!(hits.len(), 1, "only the bookmark should match");
    assert_eq!(hits[0].doc.kind, SearchDocumentKind::Bookmark);
}

#[test]
fn bookmark_rebuild_includes_bookmarks() {
    // Acceptance: rebuild preserves bookmark documents alongside blocks.
    let idx = open_test_index();
    let docs = vec![
        SearchDocument::from_block(1, "ls", "output", None, 0),
        SearchDocument::from_bookmark(
            1,
            Some("important note"),
            "ls",
            &["urgent".to_string()],
            None,
            0,
        ),
    ];
    idx.rebuild(&docs).unwrap();
    assert_eq!(idx.count().unwrap(), 2);
    assert_eq!(idx.count_kind(SearchDocumentKind::Bookmark).unwrap(), 1);
    let hits = idx.search(&SearchQuery::new("important")).unwrap();
    assert!(!hits.is_empty());
    let hits = idx.search(&SearchQuery::new("urgent")).unwrap();
    assert!(!hits.is_empty());
}

#[test]
fn bookmark_upsert_replaces_existing() {
    // Acceptance: upserting a bookmark with the same stable_id replaces
    // the old content (note edit scenario).
    let idx = open_test_index();
    let old = SearchDocument::from_bookmark(10, Some("old note text"), "cmd", &[], None, 0);
    idx.upsert(&old).unwrap();
    let new = SearchDocument::from_bookmark(10, Some("updated note text"), "cmd", &[], None, 1);
    idx.upsert(&new).unwrap();
    assert_eq!(idx.count_kind(SearchDocumentKind::Bookmark).unwrap(), 1);
    let hits = idx.search(&SearchQuery::new("updated")).unwrap();
    assert!(!hits.is_empty(), "updated note should be findable");
    let hits = idx.search(&SearchQuery::new("old note")).unwrap();
    assert!(
        hits.is_empty(),
        "old note text should no longer be findable"
    );
}

#[test]
fn bookmark_cjk_note_searchable() {
    // Acceptance: CJK notes are searchable (FTS5 unicode61 tokenizer).
    let idx = open_test_index();
    let doc = SearchDocument::from_bookmark(1, Some("部署脚本"), "kubectl apply", &[], None, 0);
    idx.upsert(&doc).unwrap();
    let hits = idx.search(&SearchQuery::new("部署")).unwrap();
    assert!(!hits.is_empty(), "CJK bookmark note should be searchable");
}
