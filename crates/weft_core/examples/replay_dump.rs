//! Forensics: replay raw PTY bytes through the VT parser and dump the grid.
//!
//! Usage:
//!   cargo run -p weft_core --example replay_dump -- <raw-bytes-file> [row_from row_to]
//! Rows print as EXACT strings: '·' = NUL cell, '␣' = space cell, others verbatim.

use weft_core::grid::CellColor;
use weft_core::vt::Terminal;

fn color_str(c: &CellColor) -> String {
    match c {
        CellColor::Default => "def".into(),
        CellColor::Palette(i) => format!("p{}", i),
        CellColor::Rgb(rgb) => format!("#{:02X}{:02X}{:02X}", rgb.r, rgb.g, rgb.b),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args[1].clone();
    let cell_mode = args.len() >= 6 && args[2] == "cell";
    let (from, to) = if cell_mode {
        (0, 34)
    } else if args.len() >= 4 {
        (
            args[2].parse::<usize>().unwrap(),
            args[3].parse::<usize>().unwrap(),
        )
    } else {
        (0, 34)
    };
    let bytes = std::fs::read(&path).expect("read input");
    let (rows, cols) = (34usize, 120usize);
    let mut term = Terminal::new(rows, cols);
    term.process(&bytes);

    if cell_mode {
        let r: usize = args[3].parse().unwrap();
        let (c0, c1): (usize, usize) = (args[4].parse().unwrap(), args[5].parse().unwrap());
        let grid = term.grid();
        for c in c0..=c1 {
            let cell = grid.cell(r, c);
            let g = grid.grapheme_at(r, c);
            let ch: String = match g {
                Some(s) => s.to_string(),
                None => cell.character.to_string(),
            };
            let w = match cell.width {
                weft_core::grid::CellWidth::Full => 2,
                weft_core::grid::CellWidth::Half => 1,
            };
            println!(
                "r{r}c{c}: {:?} w={} fg={} flags={:?}",
                ch,
                w,
                color_str(&cell.fg),
                cell.flags
            );
        }
        return;
    }

    let grid = term.grid();
    for r in from..to.min(rows) {
        let mut line = String::new();
        let mut colors: Vec<String> = Vec::new();
        let mut c = 0usize;
        let mut prev_fg = String::from("?");
        while c < cols {
            let cell = grid.cell(r, c);
            let g = grid.grapheme_at(r, c);
            let ch = match g {
                Some(s) => s.to_string(),
                None => cell.character.to_string(),
            };
            let shown: String = if ch.chars().all(|x| x == '\0') {
                '·'.to_string()
            } else if ch == " " {
                '␣'.to_string()
            } else {
                ch
            };
            line.push_str(&shown);
            let w = match cell.width {
                weft_core::grid::CellWidth::Full => 2,
                weft_core::grid::CellWidth::Half => 1,
            };
            let fg = color_str(&cell.fg);
            if fg != prev_fg {
                colors.push(format!("{}@{}", c, fg));
                prev_fg = fg;
            }
            c += w.max(1);
        }
        let trimmed = line.trim_end_matches('·');
        let meaningful = trimmed.trim_end_matches('␣');
        let tlen = meaningful.chars().count();
        println!("r{r:02} w={tlen:3} |{meaningful}|");
        if !colors.is_empty() {
            println!("      fg changes: {}", colors.join(" "));
        }
    }
}
