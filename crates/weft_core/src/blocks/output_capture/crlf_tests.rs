use super::{CapturedStyle, OutputCapture};
use crate::grid::{CellColor, CellFlags, Color, UnderlineStyle};

fn foreground(color: CellColor) -> CapturedStyle {
    CapturedStyle::from_attrs(
        color,
        CellColor::Default,
        CellFlags::empty(),
        UnderlineStyle::Single,
        None,
    )
}

#[test]
fn multiline_styles_follow_pty_crlf_lines() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"first", foreground(CellColor::Palette(1)), 1024);
    output.carriage_return();
    output.newline(1024);
    output.print_ascii(b"second", foreground(CellColor::Palette(2)), 1024);
    output.carriage_return();
    output.newline(1024);
    output.print_ascii(
        b"third",
        foreground(CellColor::Rgb(Color::rgb(12, 34, 56))),
        1024,
    );

    let (text, styled) = output.take_styled();
    assert_eq!(text, "first\nsecond\nthird");
    let styled = styled.expect("styled");
    assert_eq!(
        styled.line(0).unwrap().foreground_at(0),
        Some(CellColor::Palette(1))
    );
    assert_eq!(
        styled.line(1).unwrap().foreground_at(0),
        Some(CellColor::Palette(2))
    );
    assert_eq!(
        styled.line(2).unwrap().foreground_at(0),
        Some(CellColor::Rgb(Color::rgb(12, 34, 56)))
    );
}
