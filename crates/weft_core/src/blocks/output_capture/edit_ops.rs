//! OutputCapture 的 ICH/DCH/ECH 行内编辑三原语（v1.18.8 捕获镜像）。
//! 语义模板 = erase_line：入口记水印、char 空间计数、行内受限
//! （line_start..line_end 不跨行）、cursor 不动；样式经 shift_style_at
//! 在 char 空间全局平移（splice_style 同域，style.rs 零改动）。

use super::{CapturedStyleRun, OutputCapture, MAX_STYLE_RUNS_PER_BLOCK};

impl OutputCapture {
    /// ICH：在 cursor 处插入 `count` 个空格，行尾内容在 char 空间整体后移。
    /// cap 放不下时只插入放得下的前缀并置 truncated（对齐 goto 截断先例）。
    pub(crate) fn insert_blank(&mut self, count: usize, max_bytes: usize) {
        self.align_cursor();
        self.note_min_write_offset(self.cursor);
        let budget = max_bytes.saturating_sub(self.text.len());
        let take = count.min(budget);
        if take < count {
            self.truncated = true;
        }
        if take == 0 {
            return;
        }
        let spaces = " ".repeat(take);
        let at_char = self.char_cursor;
        self.text.insert_str(self.cursor, &spaces);
        self.text_char_count += take;
        self.shift_style_at(at_char, take as isize);
    }

    /// DCH：从 cursor 起删除至多 `count` 个 char。行内受限，行尾截断
    /// 不跨行；删除点之后的内容（含后续行 run）在 char 空间全局前移。
    pub(crate) fn delete_chars(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        self.align_cursor();
        self.note_min_write_offset(self.cursor);
        let line_end = self.line_end();
        let mut probe = self.cursor;
        let mut removed = 0usize;
        while probe < line_end && removed < count {
            probe += self.text[probe..].chars().next().map_or(0, char::len_utf8);
            removed += 1;
        }
        if removed == 0 {
            return;
        }
        let at_char = self.char_cursor;
        self.text.replace_range(self.cursor..probe, "");
        self.text_char_count -= removed;
        self.shift_style_at(at_char, -(removed as isize));
    }

    /// ECH：从 cursor 起原地改写至多 `count` 个 char 为空格。同 char 数
    /// 替换、ledger 不动（erase_line mode-1 的 "wide chars carry no
    /// filler" 技巧），擦除区间样式一并清为默认。
    pub(crate) fn erase_chars(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        self.align_cursor();
        self.note_min_write_offset(self.cursor);
        let line_end = self.line_end();
        let mut probe = self.cursor;
        let mut wiped = 0usize;
        while probe < line_end && wiped < count {
            probe += self.text[probe..].chars().next().map_or(0, char::len_utf8);
            wiped += 1;
        }
        if wiped == 0 {
            return;
        }
        self.text
            .replace_range(self.cursor..probe, &" ".repeat(wiped));
        self.erase_style_range(self.char_cursor, self.char_cursor + wiped as u32);
    }

    /// 防御：把 cursor 拉回 char 边界。正常操作序列不会产生半个 char 的
    /// cursor（backspace 的字节回退先例同源），但三原语要按 char 计数，
    /// 先守住边界再动手。
    fn align_cursor(&mut self) {
        if self.text.is_char_boundary(self.cursor) {
            return;
        }
        let aligned = (0..=self.cursor)
            .rev()
            .find(|&i| self.text.is_char_boundary(i))
            .unwrap_or(0);
        self.char_cursor = self.text[..aligned].chars().count() as u32;
        self.cursor = aligned;
    }

    /// 样式 RLE 平移：`at_char` 起的全部 run 在 char 空间平移 `delta`
    /// （delta>0=insert 后移；delta<0=delete 前移，移除区间
    /// [at_char, at_char-delta) 内的 run 段丢弃）。跨边界 run 拆头尾，
    /// 同 style 相邻段合并，超帽置 style_overflow 并清空
    /// （镜像 splice_style 的收尾 pass）。
    fn shift_style_at(&mut self, at_char: u32, delta: isize) {
        if delta == 0 || self.style_runs.is_empty() {
            return;
        }
        let mut shifted: Vec<CapturedStyleRun> = Vec::with_capacity(self.style_runs.len() + 2);
        for item in self.style_runs.drain(..) {
            if item.end_char <= at_char {
                shifted.push(item);
                continue;
            }
            if delta > 0 {
                let off = delta as u32;
                if item.start_char >= at_char {
                    shifted.push(CapturedStyleRun {
                        start_char: item.start_char + off,
                        end_char: item.end_char + off,
                        style: item.style,
                    });
                } else {
                    // 跨插入点：头段留在插入点前，尾段后移。
                    shifted.push(CapturedStyleRun {
                        end_char: at_char,
                        ..item
                    });
                    shifted.push(CapturedStyleRun {
                        start_char: at_char + off,
                        end_char: item.end_char + off,
                        ..item
                    });
                }
            } else {
                let back = (-delta) as u32;
                let removed_end = at_char + back;
                if item.start_char >= removed_end {
                    shifted.push(CapturedStyleRun {
                        start_char: item.start_char - back,
                        end_char: item.end_char - back,
                        style: item.style,
                    });
                } else if item.start_char >= at_char {
                    // 起点在移除区内：尾巴伸出区外则前移成尾段，否则整段丢弃。
                    if item.end_char > removed_end {
                        shifted.push(CapturedStyleRun {
                            start_char: at_char,
                            end_char: item.end_char - back,
                            style: item.style,
                        });
                    }
                } else if item.end_char > removed_end {
                    // 跨移除点且尾巴伸出：头段保留 + 尾段前移（合并 pass 收拢）。
                    shifted.push(CapturedStyleRun {
                        end_char: at_char,
                        ..item
                    });
                    shifted.push(CapturedStyleRun {
                        start_char: at_char,
                        end_char: item.end_char - back,
                        ..item
                    });
                } else {
                    // 跨移除点但尾部全在区内：只留头段。
                    shifted.push(CapturedStyleRun {
                        end_char: at_char,
                        ..item
                    });
                }
            }
        }
        let mut merged: Vec<CapturedStyleRun> = Vec::with_capacity(shifted.len());
        for item in shifted {
            if let Some(last) = merged.last_mut() {
                if last.style == item.style && last.end_char == item.start_char {
                    last.end_char = item.end_char;
                    continue;
                }
            }
            merged.push(item);
        }
        if merged.len() > MAX_STYLE_RUNS_PER_BLOCK {
            self.style_overflow = true;
            self.style_runs.clear();
            return;
        }
        self.style_runs = merged;
    }
}
