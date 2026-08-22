//! Choose one of a list, inline, with fuzzy filtering.
//!
//! The whole terminal interface sits behind one function and one row type:
//! callers hand over labels and get back an index. Rendering, the key map,
//! raw mode, the viewport, the theme and the matcher are all private.

use std::io::{self, IsTerminal};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

/// The picker is drawn inline rather than in the alternate screen, so it is
/// kept to one line per project and no taller than it needs to be.
const MAX_VISIBLE: usize = 10;

/// Rows the picker spends on itself rather than on projects: the query line
/// above the list and the help line below it.
const CHROME: usize = 2;

/// One row of the picker.
pub struct Item {
    /// Main text, e.g. "tobi-try".
    pub label: String,
    /// Secondary text, e.g. "3 days ago".
    pub hint: String,
}

/// Shows the picker and returns the index of the chosen item.
///
/// The UI is drawn on stderr so that stdout stays free for ordinary output,
/// and input comes from /dev/tty, so both still work when the caller has
/// redirected either one.
pub fn choose(items: &[Item]) -> Result<Option<usize>> {
    if !io::stderr().is_terminal() {
        bail!("no terminal available for the picker");
    }

    let visible = items.len().clamp(1, MAX_VISIBLE);
    let height = u16::try_from(visible + CHROME).unwrap_or(u16::MAX);

    enable_raw_mode().context("cannot put the terminal in raw mode")?;
    let backend = CrosstermBackend::new(io::stderr());
    let mut term = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .context("cannot start the picker");

    let result = match term.as_mut() {
        Ok(term) => select_loop(term, items),
        Err(_) => Err(anyhow!("cannot start the picker")),
    };

    if let Ok(term) = term.as_mut() {
        // `Terminal::clear` puts the cursor back where the last draw left it,
        // which is the bottom of the viewport — the cleared rows would then sit
        // above the next shell prompt as blank lines. Park the cursor on the
        // viewport's first row instead, so the prompt reclaims them.
        let top = term.get_frame().area().y;
        let _ = execute!(
            io::stderr(),
            MoveTo(0, top),
            Clear(ClearType::FromCursorDown),
            Show
        );
    }
    let _ = disable_raw_mode();
    result
}

fn select_loop(
    term: &mut Terminal<CrosstermBackend<io::Stderr>>,
    items: &[Item],
) -> Result<Option<usize>> {
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut query = String::new();
    let mut order = filter(items, &query, &mut matcher);
    let mut selected = 0usize;
    let mut offset = 0usize;
    let width = label_width(items);

    loop {
        let chrome = u16::try_from(CHROME).unwrap_or(u16::MAX);
        let rows = usize::from(term.get_frame().area().height.saturating_sub(chrome)).max(1);
        if selected < offset {
            offset = selected;
        } else if selected >= offset + rows {
            offset = selected + 1 - rows;
        }

        term.draw(|frame| {
            render(frame, &query, items, &order, selected, offset, width);
        })?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            KeyCode::Char('c') if ctrl => return Ok(None),
            KeyCode::Esc => {
                if query.is_empty() {
                    return Ok(None);
                }
                query.clear();
                order = filter(items, &query, &mut matcher);
                selected = 0;
                offset = 0;
            }
            KeyCode::Enter => {
                if let Some(hit) = order.get(selected) {
                    return Ok(Some(hit.index));
                }
            }
            // Only the control-prefixed forms navigate: with type-to-filter a
            // bare `j` or `n` has to reach the query like any other letter.
            // In raw mode crossterm reports Ctrl+J as Char('j'), not as Enter,
            // so binding it here does not shadow select.
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Char('p' | 'k') if ctrl => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = next(selected, order.len()),
            KeyCode::Char('n' | 'j') if ctrl => selected = next(selected, order.len()),
            KeyCode::Backspace => {
                query.pop();
                order = filter(items, &query, &mut matcher);
                selected = 0;
                offset = 0;
            }
            KeyCode::Char('u') if ctrl => {
                query.clear();
                order = filter(items, &query, &mut matcher);
                selected = 0;
                offset = 0;
            }
            KeyCode::Char(ch) if !ctrl && !alt => {
                query.push(ch);
                order = filter(items, &query, &mut matcher);
                selected = 0;
                offset = 0;
            }
            _ => {}
        }
    }
}

/// The row below `selected`, stopping at the end of the list.
fn next(selected: usize, len: usize) -> usize {
    if selected + 1 < len { selected + 1 } else { selected }
}

/// A row that survived the filter, with the character positions the query
/// matched so they can be picked out when the row is drawn.
struct Hit {
    index: usize,
    positions: Vec<u32>,
}

/// The visible rows, in order: everything when the query is empty, otherwise
/// the fuzzy matches ranked by score.
fn filter(items: &[Item], query: &str, matcher: &mut Matcher) -> Vec<Hit> {
    if query.is_empty() {
        return (0..items.len())
            .map(|index| Hit {
                index,
                positions: Vec::new(),
            })
            .collect();
    }

    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut positions = Vec::new();
    let mut scored: Vec<(Hit, u32)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            positions.clear();
            let score = pattern.indices(
                Utf32Str::new(&item.label, &mut buf),
                matcher,
                &mut positions,
            )?;
            positions.sort_unstable();
            positions.dedup();
            let hit = Hit {
                index,
                positions: positions.clone(),
            };
            Some((hit, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.index.cmp(&b.0.index)));
    scored.into_iter().map(|(hit, _)| hit).collect()
}

/// The column the hints line up on. Rows without a hint do not widen it.
fn label_width(items: &[Item]) -> usize {
    items
        .iter()
        .filter(|item| !item.hint.is_empty())
        .map(|item| item.label.chars().count())
        .max()
        .unwrap_or(0)
}

/// Catppuccin Mocha, with the role names and values herdr uses in
/// `~/repos/herdr/src/app/state.rs` (`Palette::catppuccin`).
///
/// Pinned to the flavour's own values rather than ANSI names, so the picker is
/// the same under tmux, over ssh, and in a terminal set to something else.
/// tmux passes these through: term.nix sets `terminal-features *:RGB`.
mod theme {
    use ratatui::style::Color;

    /// `text` — the rows you are not on
    pub const TEXT: Color = Color::Rgb(0xcd, 0xd6, 0xf4);
    /// `subtext0` — subdued text, i.e. the age column
    pub const SUBTEXT: Color = Color::Rgb(0xa6, 0xad, 0xc8);
    /// `overlay0` — muted chrome: the prompt and the help line
    pub const OVERLAY: Color = Color::Rgb(0x6c, 0x70, 0x86);
    /// `yellow` — the characters the query actually hit
    pub const YELLOW: Color = Color::Rgb(0xf9, 0xe2, 0xaf);
    /// `surface0` — herdr's surface for selected and focused items
    pub const SURFACE: Color = Color::Rgb(0x31, 0x32, 0x44);
    /// `accent` (blue) — the text of the row you are on
    pub const ACCENT: Color = Color::Rgb(0x89, 0xb4, 0xfa);
}

/// The palette, kept deliberately small.
///
/// The selection is a background bar in the theme's own selected-item surface,
/// with its text in the accent; weight and the pointer say the same thing
/// again, so the picker still reads with colour turned off. Matched characters
/// are the only other saturated colour on screen, because they are the only
/// thing colour tells you that position and weight cannot.
const DIM: Style = Style::new().fg(theme::OVERLAY);

const MUTED: Style = Style::new().fg(theme::SUBTEXT);

const BOLD: Style = Style::new()
    .fg(theme::TEXT)
    .add_modifier(Modifier::BOLD);

const TEXT: Style = Style::new().fg(theme::TEXT);

const MATCH: Style = Style::new()
    .fg(theme::YELLOW)
    .add_modifier(Modifier::BOLD);

const SELECTED: Style = Style::new().bg(theme::SURFACE);

const SELECTED_FG: Style = Style::new()
    .fg(theme::ACCENT)
    .add_modifier(Modifier::BOLD);

#[allow(clippy::too_many_arguments)]
fn render(
    frame: &mut Frame,
    query: &str,
    items: &[Item],
    order: &[Hit],
    selected: usize,
    offset: usize,
    width: usize,
) {
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // The top line is an input line and nothing else: the query sits where you
    // type it. There is no matched-out-of-total count, because the list is
    // right there — a count would spend a row saying what you can already see.
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" > ", DIM),
            Span::styled(query.to_string(), BOLD),
        ])),
        head,
    );

    let rows = usize::from(body.height);
    let lines: Vec<Line> = if order.is_empty() {
        vec![Line::from(Span::styled("   no matches", DIM))]
    } else {
        order
            .iter()
            .enumerate()
            .skip(offset)
            .take(rows)
            .map(|(position, hit)| {
                row(&items[hit.index], position == selected, width, &hit.positions)
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), body);

    // The bar is painted after the rows so it spans the full width rather than
    // stopping where the text does. Setting only a background patches it in
    // without disturbing the foregrounds already there.
    if !order.is_empty()
        && let Some(offset_row) = selected.checked_sub(offset)
        && let Ok(offset_row) = u16::try_from(offset_row)
        && offset_row < body.height
    {
        let bar = Rect {
            y: body.y + offset_row,
            height: 1,
            ..body
        };
        frame.buffer_mut().set_style(bar, SELECTED);
    }

    let help = " type to filter · ↑↓ move · enter select · esc clear/cancel";
    frame.render_widget(Paragraph::new(Line::from(Span::styled(help, DIM))), foot);
}

fn row(item: &Item, selected: bool, width: usize, positions: &[u32]) -> Line<'static> {
    let (marker, base) = if selected {
        (" > ", SELECTED_FG)
    } else {
        ("   ", TEXT)
    };

    let mut spans = vec![Span::styled(marker, base)];
    spans.extend(highlight(&item.label, positions, base));
    if !item.hint.is_empty() {
        let pad = width.saturating_sub(item.label.chars().count());
        spans.push(Span::raw(" ".repeat(pad + 2)));
        spans.push(Span::styled(item.hint.clone(), MUTED));
    }
    Line::from(spans)
}

/// Splits a label into runs of matched and unmatched characters, so the part
/// the query actually hit stands out.
fn highlight(label: &str, positions: &[u32], base: Style) -> Vec<Span<'static>> {
    if positions.is_empty() {
        return vec![Span::styled(label.to_string(), base)];
    }

    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_matched = false;
    for (i, ch) in label.chars().enumerate() {
        let matched = positions.binary_search(&(i as u32)).is_ok();
        if matched != run_matched && !run.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut run),
                if run_matched { base.patch(MATCH) } else { base },
            ));
        }
        run_matched = matched;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(
            run,
            if run_matched { base.patch(MATCH) } else { base },
        ));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn item(label: &str, hint: &str) -> Item {
        Item {
            label: label.to_string(),
            hint: hint.to_string(),
        }
    }

    fn items(labels: &[&str]) -> Vec<Item> {
        labels.iter().map(|label| item(label, "")).collect()
    }

    fn matches(items: &[Item], query: &str) -> Vec<usize> {
        let mut matcher = Matcher::new(Config::DEFAULT);
        filter(items, query, &mut matcher)
            .into_iter()
            .map(|hit| hit.index)
            .collect()
    }

    /// The text of a line, with the styling dropped.
    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    /// Every row of a rendered frame, trailing blanks trimmed.
    fn screen(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                let row: String = (0..buffer.area.width)
                    .filter_map(|x| buffer.cell((x, y)))
                    .map(|cell| cell.symbol())
                    .collect();
                row.trim_end().to_string()
            })
            .collect()
    }

    fn draw(query: &str, list: &[Item], selected: usize, offset: usize) -> Buffer {
        let width = label_width(list);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let order = filter(list, query, &mut matcher);
        let height = u16::try_from(list.len().clamp(1, MAX_VISIBLE) + CHROME).unwrap();
        let mut term = Terminal::new(TestBackend::new(60, height)).unwrap();
        term.draw(|frame| render(frame, query, list, &order, selected, offset, width))
            .unwrap();
        term.backend().buffer().clone()
    }

    #[test]
    fn an_empty_query_keeps_every_row_in_order() {
        let list = items(&["redis", "notes", "try"]);
        let hits = filter(&list, "", &mut Matcher::new(Config::DEFAULT));
        assert_eq!(
            hits.iter().map(|hit| hit.index).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(hits.iter().all(|hit| hit.positions.is_empty()));
    }

    #[test]
    fn a_query_keeps_only_what_it_matches() {
        let list = items(&["redis", "notes", "rust-docs"]);
        assert_eq!(matches(&list, "red"), [0]);
        assert_eq!(matches(&list, "zzz"), Vec::<usize>::new());
    }

    /// Fuzzy, so the letters only have to appear in order.
    #[test]
    fn a_query_matches_letters_spread_through_the_label() {
        let list = items(&["goofansu-try-pr-123"]);
        assert_eq!(matches(&list, "gtry"), [0]);
        assert_eq!(matches(&list, "pr123"), [0]);
    }

    #[test]
    fn a_lowercase_query_ignores_case() {
        let list = items(&["Redis"]);
        assert_eq!(matches(&list, "red"), [0]);
    }

    #[test]
    fn better_matches_come_first() {
        let list = items(&["my-redis-fork", "redis"]);
        assert_eq!(
            matches(&list, "redis"),
            [1, 0],
            "the whole label beats a fragment of one"
        );
    }

    /// The positions are what the row highlights, so they have to be usable as
    /// a sorted, duplicate-free index into the label.
    #[test]
    fn matched_positions_are_sorted_and_unique() {
        let list = items(&["goofansu-try"]);
        let hits = filter(&list, "try", &mut Matcher::new(Config::DEFAULT));
        let positions = &hits[0].positions;
        assert!(!positions.is_empty());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(positions.iter().all(|&i| (i as usize) < 12));
    }

    #[test]
    fn the_hint_column_clears_the_longest_label_that_has_one() {
        let list = vec![
            item("short", "today"),
            item("a-much-longer-name", ""),
            item("middling", "yesterday"),
        ];
        assert_eq!(
            label_width(&list),
            "middling".len(),
            "a row with no hint does not widen the column"
        );
        assert_eq!(label_width(&items(&["anything"])), 0);
        assert_eq!(label_width(&[]), 0);
    }

    #[test]
    fn the_hint_column_counts_characters_not_bytes() {
        let list = vec![item("réseau", "today")];
        assert_eq!(label_width(&list), 6);
    }

    #[test]
    fn moving_down_stops_at_the_last_row() {
        assert_eq!(next(0, 3), 1);
        assert_eq!(next(1, 3), 2);
        assert_eq!(next(2, 3), 2);
        assert_eq!(next(0, 0), 0, "an empty list has nowhere to go");
    }

    #[test]
    fn an_unmatched_label_is_one_plain_run() {
        let spans = highlight("redis", &[], TEXT);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "redis");
        assert_eq!(spans[0].style, TEXT);
    }

    #[test]
    fn highlighting_splits_the_label_into_runs() {
        let spans = highlight("redis", &[0, 1, 2], TEXT);
        let runs: Vec<(&str, bool)> = spans
            .iter()
            .map(|span| (span.content.as_ref(), span.style == TEXT.patch(MATCH)))
            .collect();
        assert_eq!(runs, [("red", true), ("is", false)]);

        let spans = highlight("redis", &[1, 3], TEXT);
        let runs: Vec<&str> = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(runs, ["r", "e", "d", "i", "s"]);
    }

    #[test]
    fn highlighting_a_whole_label_is_one_matched_run() {
        let spans = highlight("try", &[0, 1, 2], TEXT);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "try");
        assert_eq!(spans[0].style, TEXT.patch(MATCH));
    }

    /// The positions are character offsets, not byte offsets.
    #[test]
    fn highlighting_counts_characters_not_bytes() {
        let spans = highlight("réseau", &[0, 1], TEXT);
        assert_eq!(spans[0].content, "ré");
        assert_eq!(spans[1].content, "seau");
    }

    #[test]
    fn a_row_marks_the_one_you_are_on() {
        assert!(text(&row(&item("redis", ""), true, 0, &[])).starts_with(" > "));
        assert!(text(&row(&item("redis", ""), false, 0, &[])).starts_with("   "));
    }

    #[test]
    fn a_row_pads_its_label_out_to_the_hint_column() {
        let line = row(&item("try", "today"), false, 8, &[]);
        assert_eq!(text(&line), "   try       today");

        let line = row(&item("try", ""), false, 8, &[]);
        assert_eq!(text(&line), "   try", "no hint, no padding");
    }

    #[test]
    fn the_frame_shows_the_query_the_rows_and_the_help() {
        let list = vec![item("redis", "today"), item("notes", "3 days ago")];
        let screen = screen(&draw("", &list, 0, 0));
        assert_eq!(screen[0], " >");
        assert_eq!(screen[1], " > redis  today");
        assert_eq!(screen[2], "   notes  3 days ago");
        assert!(screen[3].contains("enter select"), "{:?}", screen[3]);
    }

    #[test]
    fn the_frame_echoes_what_was_typed() {
        let list = vec![item("redis", "")];
        let screen = screen(&draw("red", &list, 0, 0));
        assert_eq!(screen[0], " > red");
    }

    #[test]
    fn a_query_that_matches_nothing_says_so() {
        let list = vec![item("redis", ""), item("notes", "")];
        let screen = screen(&draw("zzz", &list, 0, 0));
        assert_eq!(screen[1], "   no matches");
        assert_eq!(screen[2], "");
    }

    /// The bar spans the whole width, not just the text, so it is painted over
    /// the row after the rows are drawn.
    #[test]
    fn the_selected_row_is_a_full_width_bar() {
        let list = vec![item("redis", ""), item("notes", "")];
        let buffer = draw("", &list, 1, 0);
        let bar_row = 2;
        for x in 0..buffer.area.width {
            assert_eq!(
                buffer.cell((x, bar_row)).unwrap().bg,
                theme::SURFACE,
                "column {x} of the selected row"
            );
        }
        for x in 0..buffer.area.width {
            assert_ne!(buffer.cell((x, 1)).unwrap().bg, theme::SURFACE);
        }
    }

    /// The list is taller than the viewport, so it scrolls: the offset is the
    /// first row drawn.
    #[test]
    fn a_long_list_is_drawn_from_the_offset() {
        let labels: Vec<String> = (0..20).map(|n| format!("project-{n:02}")).collect();
        let list: Vec<Item> = labels.iter().map(|l| item(l, "")).collect();

        let screen = screen(&draw("", &list, 12, 8));
        assert_eq!(screen.len(), MAX_VISIBLE + CHROME);
        assert_eq!(screen[1], "   project-08");
        assert_eq!(screen[5], " > project-12", "the selected row is marked");
        assert_eq!(screen[10], "   project-17");
    }

    #[test]
    fn the_matched_characters_stand_out() {
        let list = vec![item("redis", "")];
        let buffer = draw("red", &list, 0, 0);
        // "   redis": the first three letters matched.
        for (x, matched) in [(3, true), (4, true), (5, true), (6, false), (7, false)] {
            let cell = buffer.cell((x, 1)).unwrap();
            assert_eq!(
                cell.fg == theme::YELLOW,
                matched,
                "column {x} ({})",
                cell.symbol()
            );
        }
    }
}
