//! v1.7.1: Unified local search — SearchDocument model + FTS5 index.
//!
//! V17_IMPLEMENTATION_PLAN §3 "统一本地搜索":
//! - `SearchDocument { kind, stable_id, title, body, cwd, updated_at }`
//! - SQLite FTS5 独立索引 blocks/workflows/workspaces/bookmarks
//! - 启动时做能力探测；FTS5 不可用时保留 substring fallback
//! - 索引可丢弃并重建，不成为源数据
//! - 排序函数只使用 exact/prefix、CWD、使用频率和 recency，保持纯逻辑和可测试

mod search_document;
mod search_index;

pub use search_document::{SearchDocument, SearchDocumentKind, SearchHit};
pub use search_index::{rank_hits, SearchIndex, SearchQuery};
