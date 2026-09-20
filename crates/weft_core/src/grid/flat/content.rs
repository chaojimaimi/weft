//! Chunked UTF-8 content storage for flat scrollback (ported from Warp
//! `flat_storage/content.rs`).
//!
//! WHY a flat byte stream: history text only ever appends at the back and is
//! dropped from the front, so it can live in one never-re-wound byte address
//! space instead of one `Vec<Cell>` per [`Row`](crate::grid::Row). Chunking
//! lets us trim the front without copying: dropping old rows removes whole
//! chunks only ([`Content::truncate_front`]), so the offsets of surviving
//! bytes never change. That is what keeps every offset-keyed structure
//! (attribute maps, index entries) stable across eviction (PLAN_S3 §二 D2).

use std::collections::BTreeMap;
use std::ops::{Index, Range};

use super::grapheme::{CharOrStr, Grapheme};

/// An absolute byte offset into the ever-growing content stream.
///
/// Port note: replaces Warp's external `string_offset` crate (weft must not
/// grow dependencies). Offsets are `usize`-backed and never re-zeroed; the
/// arithmetic impls below cover exactly the operations the ported algorithms
/// rely on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ByteOffset(usize);

impl ByteOffset {
    pub(crate) const fn zero() -> Self {
        Self(0)
    }

    pub(crate) const fn from_usize(value: usize) -> Self {
        Self(value)
    }

    pub(crate) const fn as_usize(self) -> usize {
        self.0
    }
}

impl std::ops::Add<ByteOffset> for ByteOffset {
    type Output = ByteOffset;

    fn add(self, rhs: ByteOffset) -> ByteOffset {
        ByteOffset(self.0 + rhs.0)
    }
}

impl std::ops::Add<usize> for ByteOffset {
    type Output = ByteOffset;

    fn add(self, rhs: usize) -> ByteOffset {
        ByteOffset(self.0 + rhs)
    }
}

impl std::ops::AddAssign<ByteOffset> for ByteOffset {
    fn add_assign(&mut self, rhs: ByteOffset) {
        self.0 += rhs.0;
    }
}

impl std::ops::AddAssign<usize> for ByteOffset {
    fn add_assign(&mut self, rhs: usize) {
        self.0 += rhs;
    }
}

impl std::ops::Sub<ByteOffset> for ByteOffset {
    type Output = ByteOffset;

    fn sub(self, rhs: ByteOffset) -> ByteOffset {
        ByteOffset(self.0 - rhs.0)
    }
}

impl std::ops::Sub<usize> for ByteOffset {
    type Output = ByteOffset;

    fn sub(self, rhs: usize) -> ByteOffset {
        ByteOffset(self.0 - rhs)
    }
}

impl std::ops::SubAssign<ByteOffset> for ByteOffset {
    fn sub_assign(&mut self, rhs: ByteOffset) {
        self.0 -= rhs.0;
    }
}

impl std::ops::SubAssign<usize> for ByteOffset {
    fn sub_assign(&mut self, rhs: usize) {
        self.0 -= rhs;
    }
}

impl std::fmt::Display for ByteOffset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The grid content, stored as a series of fixed-size chunks keyed by
/// (inclusive) end offset.
///
/// Conceptually a non-circular circular buffer: the head pointer is
/// [`Content::end_offset`] and only moves forward; the tail pointer is owned
/// by the flat [`Index`](super::index::Index), and when it advances
/// [`Content::truncate_front`] drops chunks that fully precede it. Offsets
/// are never re-based, so a dropped prefix leaves every surviving offset
/// untouched.
#[derive(Debug, Clone)]
pub(crate) struct Content {
    /// Fully-built chunks, keyed by the offset of their final byte
    /// (inclusive). Keying by the *end* makes `BTreeMap::range(start..)`
    /// directly find the chunk containing any given offset.
    filled_chunks: BTreeMap<ByteOffset, Chunk>,

    /// The chunk currently being appended to.
    active_chunk: Chunk,
}

impl Content {
    pub(crate) fn new() -> Self {
        Self {
            filled_chunks: Default::default(),
            active_chunk: Chunk::new(ByteOffset::zero()),
        }
    }

    /// Appends one grapheme to the end of the content.
    pub(crate) fn push_grapheme(&mut self, grapheme: &Grapheme) {
        let grapheme_len = grapheme.len().as_usize();
        assert!(
            grapheme_len < Chunk::CHUNK_SIZE,
            "grapheme with length {grapheme_len} exceeds chunk size of {}",
            Chunk::CHUNK_SIZE
        );

        // Roll the active chunk into `filled_chunks` when the grapheme would
        // overflow it; the grapheme itself starts the next chunk.
        if self.active_chunk.len() + grapheme_len > self.active_chunk.capacity() {
            let new_start_offset = self.active_chunk.content_range().end;
            let full_chunk =
                std::mem::replace(&mut self.active_chunk, Chunk::new(new_start_offset));

            let chunk_end_byte_offset = full_chunk.content_range().end - ByteOffset::from_usize(1);
            self.filled_chunks.insert(chunk_end_byte_offset, full_chunk);
        }

        match grapheme.content() {
            CharOrStr::Char(c) => self.active_chunk.push_char(c),
            CharOrStr::Str(s) => self.active_chunk.push_str(s),
        }
    }

    /// Truncates the content tail to the given absolute byte offset.
    ///
    /// The offset must be a row boundary maintained by the index (always a
    /// grapheme boundary), so remaining bytes stay valid UTF-8.
    pub(crate) fn truncate(&mut self, new_len: ByteOffset) {
        let mut truncated_chunks = self.filled_chunks.split_off(&new_len);
        // The first chunk past the new tail becomes the active chunk again.
        if let Some((_, active_chunk)) = truncated_chunks.pop_first() {
            self.active_chunk = active_chunk;
        }

        self.active_chunk.truncate(new_len);

        debug_assert_eq!(self.end_offset(), new_len.as_usize());
    }

    /// Drops every chunk that entirely precedes the given start offset.
    ///
    /// Called when the index evicts rows from the front. Whole-chunk removal
    /// is what keeps this O(1) amortized — surviving bytes never move, so
    /// their offsets (and everything keyed on them) stay valid.
    pub(crate) fn truncate_front(&mut self, new_start_offset: ByteOffset) {
        // Explicit pop loop instead of `split_off`: the common case removes
        // 0 or 1 chunks, which is cheaper than building a second map.
        loop {
            match self.filled_chunks.first_entry() {
                Some(entry) if entry.key() < &new_start_offset => {
                    entry.remove();
                }
                _ => break,
            }
        }
    }

    /// Offset one past the last content byte (the head pointer). This is
    /// where the next push will start; it is not a measure of bytes stored.
    pub(crate) fn end_offset(&self) -> usize {
        self.active_chunk.start_offset.as_usize() + self.active_chunk.len()
    }
}

impl Index<Range<ByteOffset>> for Content {
    type Output = str;

    fn index(&self, index: Range<ByteOffset>) -> &Self::Output {
        // First chunk whose end offset reaches `index.start`: the chunk that
        // contains the range start. Ranges never straddle chunk boundaries
        // because graphemes never straddle them.
        let chunk = self
            .filled_chunks
            .range(index.start..)
            .next()
            .map(|(_, v)| v)
            .unwrap_or(&self.active_chunk);

        debug_assert!(
            index.start >= chunk.start_offset,
            "range start ({}) must be >= chunk.start_offset ({})",
            index.start,
            chunk.start_offset
        );
        debug_assert!(
            index.end <= chunk.content_range().end,
            "range end ({}) must be <= chunk.end_offset ({})",
            index.end,
            chunk.content_range().end
        );
        let start = index.start - chunk.start_offset;
        let end = index.end - chunk.start_offset;

        chunk.as_str(start.as_usize()..end.as_usize())
    }
}

/// One inline fixed-capacity chunk of content bytes.
#[derive(Debug, Clone)]
struct Chunk {
    /// Bytes stored in `content`.
    len: usize,
    /// Inline storage (avoids a heap allocation + deref per chunk).
    content: [u8; Chunk::CHUNK_SIZE],
    /// Absolute offset of this chunk's first byte.
    start_offset: ByteOffset,
}

impl Chunk {
    const CHUNK_SIZE: usize = 1024;

    fn new(start_offset: ByteOffset) -> Self {
        Self {
            len: 0,
            content: [0; Chunk::CHUNK_SIZE],
            start_offset,
        }
    }

    fn push_char(&mut self, c: char) {
        let len = c.len_utf8();
        let end = self.len + len;
        debug_assert!(end <= Self::CHUNK_SIZE, "chunk capacity exceeded");
        match len {
            1 => self.content[self.len] = c as u8,
            _ => self.content[self.len..end].copy_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
        self.len = end;
    }

    fn push_str(&mut self, s: &str) {
        let end = self.len + s.len();
        debug_assert!(end <= Self::CHUNK_SIZE, "chunk capacity exceeded");
        self.content[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
    }

    /// Shortens the chunk to the given absolute offset (a row/grapheme
    /// boundary, hence no UTF-8 validation concerns).
    fn truncate(&mut self, offset: ByteOffset) {
        debug_assert!(
            offset >= self.start_offset,
            "cannot apply truncate({offset:?}) to chunk with start_offset of {:?}",
            self.start_offset
        );
        self.len = (offset - self.start_offset).as_usize();
    }

    fn len(&self) -> usize {
        self.len
    }

    fn capacity(&self) -> usize {
        Self::CHUNK_SIZE
    }

    /// The absolute byte range covered by this chunk.
    fn content_range(&self) -> Range<ByteOffset> {
        self.start_offset..self.start_offset + self.len()
    }

    /// Safe equivalent of Warp's `from_utf8_unchecked`: only valid UTF-8 is
    /// ever pushed (chars / `&str`), so validation always succeeds, and its
    /// cost is bounded by the requested range (one grapheme in practice).
    fn as_str(&self, range: Range<usize>) -> &str {
        std::str::from_utf8(&self.content[range])
            .expect("chunk content is valid UTF-8 by construction")
    }
}

#[cfg(test)]
mod tests {
    use unicode_segmentation::UnicodeSegmentation as _;

    use super::*;

    #[test]
    fn large_grapheme_starts_new_chunk() {
        let mut content = Content::new();
        let a = Grapheme::new_from_str("a");
        for _ in 0..Chunk::CHUNK_SIZE - 1 {
            content.push_grapheme(&a);
        }

        assert!(content.filled_chunks.is_empty());

        let grapheme = Grapheme::new_from_str("🚀");
        assert!(grapheme.len().as_usize() > 1);
        assert!(content.end_offset() + grapheme.len().as_usize() > Chunk::CHUNK_SIZE);

        content.push_grapheme(&grapheme);

        assert_eq!(content.filled_chunks.len(), 1);
        assert_eq!(grapheme.len().as_usize(), content.active_chunk.len());
        assert_eq!(
            content.active_chunk.start_offset,
            ByteOffset::from_usize(Chunk::CHUNK_SIZE - 1)
        );
    }

    #[test]
    fn truncate_front_drops_old_chunks() {
        let mut content = Content::new();
        let a = Grapheme::new_from_str("a");
        for _ in 0..Chunk::CHUNK_SIZE - 1 {
            content.push_grapheme(&a);
        }
        let grapheme = Grapheme::new_from_str("🚀");
        content.push_grapheme(&grapheme);

        // Drop everything before the active chunk except for one byte.
        content.truncate_front(content.active_chunk.start_offset - ByteOffset::from_usize(1));
        // This shouldn't affect the one filled chunk.
        assert_eq!(content.filled_chunks.len(), 1);

        // Drop everything before the active chunk.
        content.truncate_front(content.active_chunk.start_offset);
        // Ensure the filled chunk was dropped.
        assert!(content.filled_chunks.is_empty());
    }

    #[test]
    fn offsets_never_rezero_across_front_truncation() {
        // PLAN_S3 D3: the head pointer only ever moves forward. Eviction
        // must not rewind `end_offset`, and reads of surviving bytes must
        // keep returning the same text at the same offsets.
        let mut content = Content::new();
        for g in "hello chunked world".graphemes(true) {
            content.push_grapheme(&Grapheme::new_from_str(g));
        }
        let before = content.end_offset();

        content.truncate_front(ByteOffset::from_usize(6));
        assert_eq!(
            content.end_offset(),
            before,
            "front truncation is a tail-pointer move, not a rewind"
        );

        assert_eq!(
            &content[ByteOffset::from_usize(6)..ByteOffset::from_usize(11)],
            "chunk"
        );

        content.push_grapheme(&Grapheme::new_from_str("!"));
        assert_eq!(content.end_offset(), before + 1);
    }
}
