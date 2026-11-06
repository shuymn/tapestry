//! The practice page (design.md "Play Lifecycle", "Mid-Chart Start and
//! Section Repeat"): the practice screen's model ([`game::PracticeScreen`])
//! as lines the render thread draws ([`MenuPage`]), and the menus' input
//! as the page's ([`PracticeInput`]). Everything here runs on menus, where
//! allocation is allowed.

use game::{PracticeInput, PracticeRow, PracticeScreen};
use input::MenuEvent;
use rules::GaugeValue;

use crate::menu_screen::{LineStyle, MenuLine, MenuPage, PAGE_LINES};
use crate::select::MenuKey;
use crate::ui_text::{Label, Message};

/// The page's input for a window key: the arrows, Enter and Escape; every
/// other key means nothing there.
pub const fn practice_key(key: MenuKey) -> Option<PracticeInput> {
    Some(match key {
        MenuKey::Up => PracticeInput::Up,
        MenuKey::Down => PracticeInput::Down,
        MenuKey::Left => PracticeInput::Left,
        MenuKey::Right => PracticeInput::Right,
        MenuKey::Enter => PracticeInput::Confirm,
        MenuKey::Escape => PracticeInput::Back,
        _ => return None,
    })
}

/// The page's input for a controller's menu event (design.md "Input"): the
/// scratch moves between rows, the white keys (7 included) confirm and the
/// black keys go back.
pub const fn practice_event(event: MenuEvent) -> PracticeInput {
    match event {
        MenuEvent::Up => PracticeInput::Up,
        MenuEvent::Down => PracticeInput::Down,
        MenuEvent::Confirm | MenuEvent::Extra => PracticeInput::Confirm,
        MenuEvent::Back => PracticeInput::Back,
    }
}

/// `us` as seconds to a tenth.
fn seconds(us: i64) -> String {
    format!("{}.{}", us / 1_000_000, us % 1_000_000 / 100_000)
}

/// `v` as a percentage to a hundredth.
pub fn percent(v: GaugeValue) -> String {
    let hundredths = v.0 / (GaugeValue::PERCENT / 100);
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

/// A row's label and value.
fn row(screen: &PracticeScreen, row: PracticeRow) -> (String, String) {
    let n = screen.measures().len();
    match row {
        PracticeRow::From => (
            Label::PracticeFrom.text().to_owned(),
            format!("{}", screen.from() + 1),
        ),
        PracticeRow::To => (
            Label::PracticeTo.text().to_owned(),
            if screen.to() == n {
                Message::PracticeToEnd { to: screen.to() }.text()
            } else {
                format!("{} / {n}", screen.to())
            },
        ),
        PracticeRow::Speed => (
            Label::PracticeSpeed.text().to_owned(),
            format!("{}%", screen.speed().twentieths() * 5),
        ),
        PracticeRow::LeadIn => (
            Label::PracticeLeadIn.text().to_owned(),
            Message::PracticeLeadInValue {
                seconds: &seconds(screen.lead_in().0),
            }
            .text(),
        ),
        PracticeRow::Gauge => (
            Message::PracticeGauge {
                gauge: screen.gauge_name().unwrap_or("-"),
            }
            .text(),
            format!("{}%", percent(screen.value())),
        ),
        PracticeRow::Repeat => (
            Label::PracticeRepeat.text().to_owned(),
            if screen.repeat() {
                Label::On
            } else {
                Label::Off
            }
            .text()
            .to_owned(),
        ),
        PracticeRow::Start => (Label::PracticeStart.text().to_owned(), String::new()),
    }
}

/// The page for `screen` of the chart `name`: the title, a line per row
/// (the cursor's marked, its value in brackets while edited), the notice
/// and the keys.
pub fn page(screen: &PracticeScreen, name: &str) -> MenuPage {
    let line = |text: String, style| MenuLine { text, style };
    let mut lines = vec![line(
        Message::PracticeTitle { name }.text(),
        LineStyle::Title,
    )];
    for r in PracticeRow::ALL {
        let (label, value) = row(screen, r);
        let cursor = r == screen.row();
        let value = if cursor && screen.editing() {
            format!("◀ {value} ▶")
        } else {
            value
        };
        let text = if value.is_empty() {
            format!("    {label}")
        } else {
            format!("    {label}: {value}")
        };
        let style = if cursor {
            LineStyle::Cursor
        } else {
            LineStyle::Chart
        };
        lines.push(line(text, style));
    }
    lines.push(line(
        screen.notice().unwrap_or_default().to_owned(),
        LineStyle::Notice,
    ));
    while lines.len() < PAGE_LINES - 1 {
        lines.push(line(String::new(), LineStyle::Chart));
    }
    lines.push(line(Label::PracticeHelp.text().to_owned(), LineStyle::Help));
    MenuPage { lines, caret: None }
}

#[cfg(test)]
mod tests {
    use game::{PracticeInput, PracticeScreen};
    use input::MenuEvent;

    use super::{page, practice_event, practice_key};
    use crate::menu_screen::{LineStyle, PAGE_LINES};
    use crate::select::MenuKey;

    fn screen() -> PracticeScreen {
        let chart = bms_parser::load_bms(
            b"#BPM 120\n#00111:01\n#00211:01\n",
            std::path::Path::new("."),
            &Default::default(),
        )
        .unwrap()
        .chart;
        PracticeScreen::new(&chart, &rules::tapestry(), 0)
    }

    /// A line per row, top down, the cursor's marked and, while edited,
    /// its value in brackets; the notice under them and the keys last.
    #[test]
    fn practice_page_rows_and_cursor() {
        let mut s = screen();
        let n = s.measures().len();
        let p = page(&s, "a.bms");
        assert_eq!(p.lines.len(), PAGE_LINES);
        assert_eq!(p.lines[0].text, "練習  a.bms");
        let rows: Vec<(&str, LineStyle)> = p.lines[1..8]
            .iter()
            .map(|l| (l.text.as_str(), l.style))
            .collect();
        let gauge = format!(
            "    初期ゲージ ({}): {}%",
            rules::tapestry().gauges[0].id,
            super::percent(rules::tapestry().gauges[0].start)
        );
        assert_eq!(
            rows,
            [
                ("    開始小節: 1", LineStyle::Cursor),
                (&*format!("    終了小節: {n} (最後まで)"), LineStyle::Chart),
                ("    速度: 100%", LineStyle::Chart),
                ("    リードイン: 2.0 秒", LineStyle::Chart),
                (&*gauge, LineStyle::Chart),
                ("    リピート: オン", LineStyle::Chart),
                ("    開始", LineStyle::Chart),
            ]
        );
        assert_eq!(
            p.lines[8],
            super::MenuLine {
                text: String::new(),
                style: LineStyle::Notice
            }
        );
        assert_eq!(p.lines[PAGE_LINES - 1].style, LineStyle::Help);
        // The speed edited at 95 %, a notice shown.
        s.input(PracticeInput::Down);
        s.input(PracticeInput::Down);
        s.input(PracticeInput::Confirm);
        s.input(PracticeInput::Down);
        s.input(PracticeInput::Down);
        s.input(PracticeInput::Right);
        s.set_notice(Some("refused".to_owned()));
        let p = page(&s, "a.bms");
        assert_eq!(p.lines[2].style, LineStyle::Chart);
        assert_eq!(
            (p.lines[3].text.as_str(), p.lines[3].style),
            ("    速度: ◀ 95% ▶", LineStyle::Cursor)
        );
        assert_eq!(p.lines[8].text, "refused");
        // A range short of the end.
        s.input(PracticeInput::Back);
        s.input(PracticeInput::Up);
        s.input(PracticeInput::Left);
        assert_eq!(
            page(&s, "").lines[2].text,
            format!("    終了小節: {} / {n}", n - 1)
        );
    }

    /// The wording of the lines the other tests check only by style: the
    /// keys, repeat off, a preset without a gauge, and the title naming a
    /// non-ASCII file as it is.
    #[test]
    fn practice_page_wording() {
        let mut s = screen();
        let p = page(&s, "曲.bms");
        assert_eq!(p.lines[0].text, "練習  曲.bms");
        assert_eq!(
            p.lines[PAGE_LINES - 1].text,
            "↑↓ 選択  ←→ 変更  Enter 編集・開始  Esc 戻る  ウィンドウを閉じて終了"
        );
        for _ in 0..5 {
            s.input(PracticeInput::Down);
        }
        s.input(PracticeInput::Right);
        assert_eq!(page(&s, "").lines[6].text, "    リピート: オフ");
        let chart = bms_parser::load_bms(
            b"#BPM 120\n#00111:01\n",
            std::path::Path::new("."),
            &Default::default(),
        )
        .unwrap()
        .chart;
        let mut preset = rules::tapestry();
        preset.gauges.clear();
        let none = PracticeScreen::new(&chart, &preset, 0);
        assert!(
            page(&none, "")
                .lines
                .iter()
                .any(|l| l.text.starts_with("    初期ゲージ (-): ")),
            "{:?}",
            page(&none, "").lines
        );
    }

    /// The keyboard's arrows, Enter and Escape are the page's inputs and
    /// nothing else is, F11 and F12 among them; the controller's scratch
    /// moves, its white keys (7 included) confirm and its black keys go
    /// back.
    #[test]
    fn practice_input_maps_keys_and_controller_events() {
        use PracticeInput::{Back, Confirm, Down, Left, Right, Up};
        for (key, input) in [
            (MenuKey::Up, Some(Up)),
            (MenuKey::Down, Some(Down)),
            (MenuKey::Left, Some(Left)),
            (MenuKey::Right, Some(Right)),
            (MenuKey::Enter, Some(Confirm)),
            (MenuKey::Escape, Some(Back)),
            (MenuKey::Tab, None),
            (MenuKey::Backspace, None),
            (MenuKey::PageUp, None),
            (MenuKey::PageDown, None),
            (MenuKey::Home, None),
            (MenuKey::End, None),
            (MenuKey::Practice, None),
            (MenuKey::PrevPreset, None),
            (MenuKey::NextPreset, None),
        ] {
            assert_eq!(practice_key(key), input, "{key:?}");
        }
        for (event, input) in [
            (MenuEvent::Up, Up),
            (MenuEvent::Down, Down),
            (MenuEvent::Confirm, Confirm),
            (MenuEvent::Extra, Confirm),
            (MenuEvent::Back, Back),
        ] {
            assert_eq!(practice_event(event), input, "{event:?}");
        }
    }
}
