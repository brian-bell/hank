//! The ready-list renderer: [`draw`] paints a [`super::App`] into a ratatui
//! `Frame`. Pure over `(App, now)` — no clock read, no I/O, no mutation beyond
//! the frame buffer — so every rendered cell is deterministic and testable with
//! `TestBackend`. See `plans/slices/slice-9.md`.
//!
//! Layout (top to bottom): a one-line title with key hints, the grouped ready
//! list, and a one-line status bar (last-refreshed age + a warning summary).
//! Grouping is a view concern: rows are rendered in the App's flat order (so the
//! selection index and the on-screen order stay aligned and `j`/`k` move one
//! displayed row at a time) with a `▸ <repo>` header emitted whenever the repo
//! changes from the previous row.

use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap};

use super::{App, DetailState, Row, SearchPhase, ViewMode};
use crate::cli::{format_row_body, sanitize};

/// Shown when the roster has no rows at all (no repos configured / nothing
/// hydrated yet — see slice-9 decision 3).
const EMPTY_HINT: &str = "no repos configured — run: hank repos discover ~/dev";

/// Shown when there are rows but the active filters hide them all, so the user
/// is not misdirected to reconfigure the roster.
const NO_MATCH_HINT: &str = "no issues match the current filters — press f/p to change";

/// One-line key hints for the list view. Only keys that act are advertised, so
/// the UI never promises an inert command; `enter detail` is live as of Slice 10
/// and `/ search` as of Slice 11.
const LIST_HINTS: &str =
    "hank · q quit · r refresh · / search · f repos · p prio · j/k move · enter detail";

/// One-line key hints for the detail pane: `j`/`k` move through beads, while
/// `J`/`K` and PageUp/PageDown scroll the pane.
const DETAIL_HINTS: &str = "hank · esc back · j/k move · J/K scroll · PgUp/PgDn page · q quit";

/// One-line key hints while editing the search query: the keys that act there.
const SEARCH_EDIT_HINTS: &str = "hank search · type query · enter run · esc cancel";

/// One-line key hints while browsing search results: the keys that act there
/// (`f`/`p` filter the results the same as the ready list).
const SEARCH_RESULTS_HINTS: &str =
    "hank search · j/k move · f repos · p prio · enter open · esc edit · q quit";

/// One-line key hints while a search is pending or failed: only these act there
/// (navigation and detail-open are inert until results arrive).
const SEARCH_WAIT_HINTS: &str = "hank search · esc edit · q quit";

/// Render the whole screen for the current [`App`] state and clock `now`: a title
/// hint row, the mode-specific content (ready list or list+detail split), and the
/// status bar.
pub fn draw(frame: &mut Frame, app: &App, now: SystemTime) -> Option<u16> {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title / key hints
            Constraint::Min(0),    // content (list, or list + detail)
            Constraint::Length(2), // separator + status bar
        ])
        .split(frame.area());

    let hints = match app.view_mode() {
        ViewMode::Detail => DETAIL_HINTS,
        // Match the hint to the phase's real key routing: editing keys type the
        // query and Enter runs it; results enable j/k + Enter (open) + Esc (edit);
        // while loading or after an error only Esc (edit) and q act.
        ViewMode::Search => match app.search_phase() {
            Some(SearchPhase::Editing) => SEARCH_EDIT_HINTS,
            Some(SearchPhase::Results) => SEARCH_RESULTS_HINTS,
            _ => SEARCH_WAIT_HINTS,
        },
        ViewMode::List | ViewMode::Loading => LIST_HINTS,
    };
    frame.render_widget(Paragraph::new(hints), chunks[0]);
    let detail_max_scroll = match app.view_mode() {
        ViewMode::Detail => Some(draw_detail_split(frame, app, chunks[1])),
        ViewMode::Search => {
            draw_search(frame, app, chunks[1]);
            None
        }
        ViewMode::List | ViewMode::Loading => {
            draw_list(frame, app, chunks[1]);
            None
        }
    };
    let status_block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().add_modifier(Modifier::DIM));
    let status_area = status_block.inner(chunks[2]);
    frame.render_widget(status_block, chunks[2]);
    frame.render_widget(
        Paragraph::new(status_line(app, now)).style(Style::default().add_modifier(Modifier::DIM)),
        status_area,
    );
    if app.repo_picker_open() {
        draw_repo_picker(frame, app);
    }
    detail_max_scroll
}

/// Render the repository picker last so it overlays, rather than replaces, the
/// current ready/search screen.
fn draw_repo_picker(frame: &mut Frame, app: &App) {
    let Some(choices) = app.repo_picker_choices() else {
        return;
    };
    let Some(labels) = app.repo_picker_labels() else {
        return;
    };
    let frame_area = frame.area();
    if frame_area.width == 0 || frame_area.height == 0 || choices.len() != labels.len() {
        return;
    }

    let longest = labels
        .iter()
        .map(|label| sanitize(label))
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0);
    let desired_width = longest.saturating_add(6).max(40);
    let width = desired_width.min(frame_area.width as usize) as u16;
    let desired_height = choices.len().saturating_add(3);
    let height = desired_height.min(frame_area.height as usize) as u16;
    let area = Rect {
        x: frame_area
            .x
            .saturating_add(frame_area.width.saturating_sub(width) / 2),
        y: frame_area
            .y
            .saturating_add(frame_area.height.saturating_sub(height) / 2),
        width,
        height,
    };

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Select repository");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let cursor = app.repo_picker_cursor().unwrap_or(0).min(choices.len() - 1);
    let option_height = inner.height.saturating_sub(1) as usize;
    let start = if option_height == 0 {
        cursor
    } else {
        cursor.saturating_sub(option_height - 1)
    };
    let mut lines = Vec::with_capacity(option_height);
    for (index, (choice, label)) in choices
        .iter()
        .zip(labels)
        .enumerate()
        .skip(start)
        .take(option_height)
    {
        let active = if choice == app.repo_view() {
            "●"
        } else {
            " "
        };
        let line = Line::from(format!("{active} {}", sanitize(label)));
        lines.push(if index == cursor {
            line.style(Style::default().add_modifier(Modifier::REVERSED))
        } else {
            line
        });
    }
    if option_height > 0 {
        frame.render_widget(
            Paragraph::new(lines),
            Rect {
                height: inner.height.saturating_sub(1),
                ..inner
            },
        );
    }
    let hint_area = Rect {
        y: inner.y.saturating_add(inner.height.saturating_sub(1)),
        height: 1,
        ..inner
    };
    frame.render_widget(
        Paragraph::new("j/k move · enter select · esc cancel")
            .style(Style::default().add_modifier(Modifier::DIM)),
        hint_area,
    );
}

/// Split the content area for the detail view: the list keeps its full rendering
/// in one half and the detail pane fills the other. Side by side (list left,
/// detail right) on a wide terminal (≥ 100 cols, the frame width unchanged by the
/// vertical title/status split); stacked (list top, detail bottom) when narrower,
/// so neither pane is squeezed below usefulness. The pane gets a faint (dim)
/// rule on the edge it shares with the list plus one column of left padding, so
/// it reads as its own region without a heavy full frame.
fn draw_detail_split(frame: &mut Frame, app: &App, area: Rect) -> u16 {
    let horizontal = area.width >= 100;
    let direction = if horizontal {
        Direction::Horizontal
    } else {
        Direction::Vertical
    };
    let parts = Layout::default()
        .direction(direction)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    draw_list(frame, app, parts[0]);
    let divider = Block::default()
        .borders(if horizontal {
            Borders::LEFT
        } else {
            Borders::TOP
        })
        .border_style(Style::default().add_modifier(Modifier::DIM))
        .padding(Padding::left(1));
    let inner = divider.inner(parts[1]);
    frame.render_widget(divider, parts[1]);
    draw_detail(frame, app, inner)
}

/// Render the detail pane for the app's current [`DetailState`]: a loading line, a
/// fetch-error message, or the human-readable `bd show` output. All bd-sourced
/// text is [`sanitize`]d at the boundary and wrapped to the pane width.
fn draw_detail(frame: &mut Frame, app: &App, area: Rect) -> u16 {
    let lines = match app.detail() {
        Some(DetailState::Loading { id }) => {
            vec![Line::from(format!("Loading {}…", sanitize(id)))]
        }
        Some(DetailState::Error { id, message }) => vec![
            Line::styled(sanitize(id), Style::default().add_modifier(Modifier::BOLD)),
            Line::from(""),
            Line::from(sanitize(message)),
        ],
        Some(DetailState::Loaded(detail)) => detail_output_lines(&detail.output),
        // Unreachable while `view_mode == Detail` (they are set together), but
        // rendering nothing is a safe fallback rather than a panic.
        None => Vec::new(),
    };
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    // Clamp the requested scroll to the wrapped content so a long detail (whose
    // dependency lines would otherwise fall off the bottom of the narrow stacked
    // pane) is fully reachable with `j`/`k`, while an over-scroll shows no blank.
    let content = paragraph.line_count(area.width) as u16;
    let max_scroll = content.saturating_sub(area.height);
    let offset = app.detail_scroll().min(max_scroll);
    frame.render_widget(paragraph.scroll((offset, 0)), area);
    max_scroll
}

/// Split `bd show` stdout into renderable rows without reformatting its sections.
/// Each row is sanitized independently so line breaks survive while terminal
/// controls cannot reach the renderer.
fn detail_output_lines(output: &str) -> Vec<Line<'static>> {
    output
        .lines()
        .map(|line| Line::from(sanitize(line)))
        .collect()
}

/// Build and render the middle list area: the loading placeholder, the empty
/// hint, or the repo-grouped rows with the selection scrolled into view.
fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    // Show "Loading…" only while the first refresh is actually in flight. A
    // failed initial refresh (`RefreshCompleted { snapshot: None }`) leaves the
    // App in `Loading` with `stale` cleared; falling through renders the empty
    // hint instead of a permanent, misleading spinner (the status bar carries
    // the failure warning).
    if app.view_mode() == ViewMode::Loading && app.is_stale() {
        frame.render_widget(Paragraph::new("Loading…"), area);
        return;
    }

    let rows = app.filtered_rows();
    if rows.is_empty() {
        // Zero rows at all vs. rows hidden by the active filters: only the former
        // is a roster problem, so only it points at `repos discover`.
        let hint = if app.rows().is_empty() {
            EMPTY_HINT
        } else {
            NO_MATCH_HINT
        };
        frame.render_widget(Paragraph::new(hint), area);
        return;
    }

    draw_rows(frame, &rows, app.selection(), area);
}

/// Render the cross-repo search screen: a query input line, a status/count line,
/// and the attributed results (through the same grouped row renderer as the ready
/// list). The results list, its selection, and its filters are the app's active
/// [`super::RowList`], so `draw_rows` and every read accessor behave identically
/// to the ready view.
fn draw_search(frame: &mut Frame, app: &App, area: Rect) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // query input
            Constraint::Length(1), // status / result count
            Constraint::Min(0),    // results
        ])
        .split(area);

    let query = sanitize(app.search_query().unwrap_or(""));
    let editing = app.search_editing();
    // A block cursor while editing so the input reads as focused.
    let input = if editing {
        format!("search: {query}\u{2588}")
    } else {
        format!("search: {query}")
    };
    frame.render_widget(Paragraph::new(input), parts[0]);

    let count = app.search_result_count();
    let status = match app.search_phase() {
        Some(SearchPhase::Editing) => "type a query · enter to search · esc to cancel".to_string(),
        Some(SearchPhase::Loading) => format!("searching for \"{query}\"…"),
        Some(SearchPhase::Results) => format!(
            "{count} result{} for \"{query}\"",
            if count == 1 { "" } else { "s" }
        ),
        // The worker already prefixes ("search failed: …") and the boundary
        // re-sanitizes; don't prefix again (was "search failed: search failed: …").
        Some(SearchPhase::Error(msg)) => sanitize(msg),
        None => String::new(),
    };
    frame.render_widget(Paragraph::new(status), parts[1]);

    // Draw the results only in the `Results` phase. The list is retained
    // internally across `Esc`→edit and a re-submit (so returning from a detail
    // re-shows it), but must not linger under a new query being edited or loaded —
    // that would misattribute a prior query's hits to the current one. (Empty is
    // fine — the count line already says "0 results"; no ready-list "no repos"
    // hint here, which would misdirect.)
    if matches!(app.search_phase(), Some(SearchPhase::Results)) {
        let rows = app.filtered_rows();
        if !rows.is_empty() {
            draw_rows(frame, &rows, app.selection(), parts[2]);
        } else if app.search_result_count() > 0 {
            // Results exist but the active repo/priority filter hides them all —
            // explain the filter (and how to clear it) instead of a blank pane.
            frame.render_widget(Paragraph::new(NO_MATCH_HINT), parts[2]);
        }
        // else: the query genuinely returned nothing; the count line says so.
    }
}

/// Render a grouped, selectable, scrolling row list into `area`: a `▸ <repo>`
/// header whenever the repo changes, `P<pri> <id> <title>` rows in the list's flat
/// (selection) order, the selected row highlighted, and a sticky repo header when
/// the viewport scrolls. Shared by the ready list and the search results so the
/// two render identically.
fn draw_rows(frame: &mut Frame, rows: &[&Row], selection: Option<usize>, area: Rect) {
    let header_style = Style::default().add_modifier(Modifier::BOLD);
    let selected_style = Style::default().add_modifier(Modifier::REVERSED);

    // Render rows in the App's flat order (the selection space) and emit a repo
    // header whenever the repo changes, so the on-screen order matches the
    // navigation order — `j`/`k` always move one displayed row. A repo whose
    // rows are non-contiguous in the flat order gets its header again, which is
    // an honest label for the priority-sorted run beneath it.
    let mut lines: Vec<Line> = Vec::new();
    // The rendered line index (headers included) of the selected row, so the
    // list can be scrolled to keep it on screen once it exceeds the area height.
    let mut selected_line: Option<usize> = None;
    // The header text governing each rendered line, so a scrolled viewport can
    // re-show the repo of its topmost line (rows omit the repo by design).
    let mut line_header: Vec<String> = Vec::new();
    let mut current_repo: Option<&str> = None;
    let mut header_text = String::new();
    for (i, row) in rows.iter().enumerate() {
        if current_repo != Some(row.repo_name.as_str()) {
            header_text = format!("▸ {}", sanitize(&row.repo_name));
            lines.push(Line::styled(header_text.clone(), header_style));
            line_header.push(header_text.clone());
            current_repo = Some(row.repo_name.as_str());
        }
        let text = format!("  {}", format_row_body(row));
        if selection == Some(i) {
            selected_line = Some(lines.len());
            // Pad to the full width so the reversed highlight fills the row.
            let padded = format!("{text:<width$}", width = area.width as usize);
            lines.push(Line::styled(padded, selected_style));
        } else {
            lines.push(Line::from(text));
        }
        line_header.push(header_text.clone());
    }

    // Scroll just enough to keep the selected line inside the viewport: nothing
    // until it would fall off the bottom, then anchor it to the last visible
    // row. Stateless (recomputed each draw), so no cross-frame offset to track.
    let offset = scroll_offset(selected_line, area.height as usize, lines.len());
    if offset == 0 {
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }

    // Scrolled: the governing header may have scrolled off the top. Freeze it on
    // the first row and scroll the rest beneath (reserving that one row), so no
    // visible row is left without its repo attribution.
    let body_height = area.height.saturating_sub(1);
    let body_offset = scroll_offset(selected_line, body_height as usize, lines.len());
    let sticky = line_header
        .get(body_offset as usize)
        .cloned()
        .unwrap_or_default();
    let header_area = Rect { height: 1, ..area };
    let body_area = Rect {
        y: area.y + 1,
        height: body_height,
        ..area
    };
    frame.render_widget(
        Paragraph::new(Line::styled(sticky, header_style)),
        header_area,
    );
    frame.render_widget(Paragraph::new(lines).scroll((body_offset, 0)), body_area);
}

/// The vertical scroll offset that keeps `selected` visible within `height`
/// rendered rows. Returns 0 when the selection fits above the fold or there is
/// nothing to scroll.
fn scroll_offset(selected: Option<usize>, height: usize, total: usize) -> u16 {
    let (Some(sel), true) = (selected, height > 0 && total > height) else {
        return 0;
    };
    let offset = if sel < height { 0 } else { sel - height + 1 };
    offset.min(u16::MAX as usize) as u16
}

/// The status-bar text: last-refreshed age (or a refreshing indicator) plus the
/// actual first warning (with a `(+N more)` count when several), so a degraded
/// refresh is diagnosable from the TUI itself rather than deferred to a command
/// that cannot reproduce these warnings. The age leads, so it is never clipped
/// when the warning text is long; ratatui truncates the line to the width.
fn status_line(app: &App, now: SystemTime) -> String {
    let mut status = match app.fetched_at() {
        Some(t) => format!("refreshed {}", format_age(now, t)),
        None if app.is_stale() => "refreshing…".to_string(),
        None => "never refreshed".to_string(),
    };
    // A refresh over already-shown rows: annotate without hiding the age.
    if app.is_stale() && app.fetched_at().is_some() {
        status.push_str(" · refreshing…");
    }
    // A recent copy confirmation, ahead of any warnings so it is not clipped.
    if let Some(flash) = app.copy_flash() {
        status.push_str(&format!(" · copied: {}", sanitize(flash)));
    }
    if let Some(warning) = app.persistence_warning() {
        status.push_str(&format!(" · {}", sanitize(warning)));
    }
    let warnings = app.status_warnings();
    if let Some(first) = warnings.first() {
        // Sanitize at the render boundary: warnings embed bd stderr / paths and
        // are written straight to the terminal (the runtime pre-sanitizes, but
        // the view must not assume its inputs are clean).
        status.push_str(&format!(" · {}", sanitize(first)));
        if warnings.len() > 1 {
            status.push_str(&format!(" (+{} more)", warnings.len() - 1));
        }
    }
    status
}

/// Humanize the elapsed time since a snapshot was fetched into a compact
/// "… ago" label. Saturates to `just now` when `now` precedes `fetched` (clock
/// skew) so the age is never negative or misleading.
fn format_age(now: SystemTime, fetched: SystemTime) -> String {
    let secs = now
        .duration_since(fetched)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if secs < 5 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Msg, RepoFilter};
    use crate::bd::Issue;
    use crate::snapshot::{Row, Snapshot};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use std::time::{Duration, UNIX_EPOCH};

    const W: u16 = 80;
    const H: u16 = 24;

    fn row(repo: &str, id: &str, priority: i64, title: &str) -> Row {
        Row {
            issue: Issue {
                id: id.to_string(),
                title: title.to_string(),
                status: "open".into(),
                priority,
                description: None,
                issue_type: None,
                owner: None,
                labels: Vec::new(),
                created_at: None,
                created_by: None,
                updated_at: None,
                dependency_count: None,
                dependent_count: None,
                comment_count: None,
            },
            repo_id: Some(repo.to_string()),
            repo_name: repo.to_string(),
            attribution_generation: None,
        }
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// An `App` advanced to `List` with the given rows (fetched at `at(1000)`)
    /// and warnings, via a single `RefreshCompleted`.
    fn app_with(rows: Vec<Row>, warnings: Vec<String>) -> App {
        let mut app = App::new();
        app.reduce(Msg::RefreshCompleted {
            snapshot: Some(Snapshot {
                rows,
                fetched_at: at(1000),
            }),
            warnings,
        });
        app
    }

    fn render(app: &App, now: SystemTime) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(W, H)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw(f, app, now);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// The full text of buffer row `y`, cell symbols concatenated.
    fn line_text(buf: &Buffer, y: u16) -> String {
        (0..W).map(|x| buf.cell((x, y)).unwrap().symbol()).collect()
    }

    /// The `y` of the first buffer row whose text contains `needle`.
    fn find_line(buf: &Buffer, needle: &str) -> Option<u16> {
        (0..H).find(|&y| line_text(buf, y).contains(needle))
    }

    #[test]
    fn renders_group_headers_and_rows() {
        // Contiguous by repo so each repo reads as one section.
        let app = app_with(
            vec![
                row("session-tui", "ra-2hc", 1, "Ready task one"),
                row("session-tui", "ra-9", 2, "Second task"),
                row("megaclock", "mc-1", 0, "Third task"),
            ],
            vec![],
        );
        let buf = render(&app, at(1000));

        let header = find_line(&buf, "▸ session-tui").expect("session-tui header present");
        assert!(
            find_line(&buf, "▸ megaclock").is_some(),
            "megaclock header present"
        );
        let row_y = find_line(&buf, "P1 ra-2hc").expect("row line present");
        assert!(
            line_text(&buf, row_y).contains("Ready task one"),
            "row carries its title"
        );
        assert_ne!(header, row_y, "header and row are on distinct lines");
    }

    #[test]
    fn repo_picker_overlays_with_active_and_pending_choices() {
        let mut app = app_with(
            vec![row("repo-a", "a-1", 1, "A"), row("repo-b", "b-1", 1, "B")],
            vec![],
        );
        app.reduce(Msg::OpenRepoPicker);
        app.reduce(Msg::RepoPickerNext);

        let buf = render(&app, at(1000));

        assert!(find_line(&buf, "Select repository").is_some());
        assert!(find_line(&buf, "● All repos").is_some());
        assert!(
            (0..H).any(|y| {
                line_text(&buf, y).contains("repo-a")
                    && (0..W).any(|x| {
                        buf.cell((x, y))
                            .unwrap()
                            .modifier
                            .contains(Modifier::REVERSED)
                    })
            }),
            "pending choice is highlighted independently of the active marker"
        );
        assert!(find_line(&buf, "j/k move · enter select · esc cancel").is_some());
    }

    #[test]
    fn repo_picker_scrolls_sanitizes_and_handles_tiny_frames() {
        let mut rows = Vec::new();
        for index in 0..12 {
            let repo = if index == 11 {
                "zz\nrepo".to_string()
            } else {
                format!("repo-{index:02}")
            };
            rows.push(row(&repo, &format!("id-{index}"), 1, "work"));
        }
        let mut app = app_with(rows, vec![]);
        app.reduce(Msg::OpenRepoPicker);
        for _ in 0..12 {
            app.reduce(Msg::RepoPickerNext);
        }

        let buf = render_sized(&app, at(1000), 50, 8);
        assert!(
            find_at(&buf, "zz\u{FFFD}repo", 50, 8).is_some(),
            "sanitized pending choice scrolls into the short modal"
        );

        let _ = render_sized(&app, at(1000), 1, 1);
    }

    #[test]
    fn renders_selection_highlight() {
        let mut app = app_with(
            vec![
                row("session-tui", "ra-2hc", 1, "Ready task one"),
                row("session-tui", "ra-9", 2, "Second task"),
            ],
            vec![],
        );
        app.reduce(Msg::SelectNext); // selection = 1 -> ra-9
        let buf = render(&app, at(1000));

        let sel_y = find_line(&buf, "ra-9").expect("selected row present");
        assert!(
            buf.cell((2, sel_y))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED),
            "the selected row is highlighted (REVERSED)"
        );
        let other_y = find_line(&buf, "ra-2hc").expect("other row present");
        assert!(
            !buf.cell((2, other_y))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED),
            "a non-selected row is not highlighted"
        );
    }

    #[test]
    fn renders_status_bar_with_age_and_warnings() {
        let app = app_with(
            vec![row("session-tui", "ra-2hc", 1, "Ready task one")],
            vec!["export failed for reading-lite".into()],
        );
        // fetched at 1000; now 180s later.
        let buf = render(&app, at(1180));

        let status = line_text(&buf, H - 1);
        assert!(
            status.contains("refreshed 3m ago"),
            "status shows humanized age: {status:?}"
        );
        assert!(
            status.contains("export failed for reading-lite"),
            "status shows the actual warning text, not a doctor redirect: {status:?}"
        );
    }

    #[test]
    fn status_keeps_persistence_warning_separate_from_refresh_warnings() {
        let mut app = app_with(
            vec![row("repo-a", "a-1", 1, "A")],
            vec!["refresh warning".into()],
        );
        app.reduce(Msg::RepoViewPersisted {
            repo: RepoFilter::All,
            result: Err("save warning".into()),
        });

        let status = status_line(&app, at(1000));

        assert!(status.contains("save warning"));
        assert!(status.contains("refresh warning"));
    }

    #[test]
    fn status_bar_shows_first_warning_and_count() {
        // Multiple warnings: the first is shown verbatim with a remaining count,
        // and a non-repo warning (version gate) is not mislabeled "repo failed".
        let app = app_with(
            vec![row("repo-a", "ra-1", 1, "t")],
            vec![
                "hank requires bd >= 1.1.0".into(),
                "id prefix `dup` claimed by 2 repos".into(),
            ],
        );
        let status = line_text(&render(&app, at(1000)), H - 1);

        assert!(
            status.contains("hank requires bd >= 1.1.0"),
            "the first warning is shown verbatim: {status:?}"
        );
        assert!(
            status.contains("(+1 more)"),
            "remaining warnings are counted: {status:?}"
        );
        assert!(
            !status.contains("repo failed"),
            "a version-gate warning is not mislabeled as a repo failure: {status:?}"
        );
    }

    #[test]
    fn renders_empty_state_hint() {
        let app = app_with(vec![], vec![]);
        let buf = render(&app, at(1000));

        assert!(
            find_line(&buf, EMPTY_HINT).is_some(),
            "an empty ready list shows the discover hint"
        );
    }

    #[test]
    fn renders_loading_before_first_snapshot() {
        let app = App::new(); // Loading, no snapshot yet
        let buf = render(&app, at(1000));

        assert!(find_line(&buf, "Loading…").is_some(), "loading placeholder");
        assert!(
            find_line(&buf, EMPTY_HINT).is_none(),
            "loading is not the empty state"
        );
    }

    #[test]
    fn navigation_follows_display_order() {
        // Interleaved repos in flat (priority) order: the rendered order must
        // match the flat order so selection index N is the Nth displayed row.
        let app = app_with(
            vec![
                row("repo-a", "ra-0", 0, "a first"),
                row("repo-b", "rb-0", 0, "b first"),
                row("repo-a", "ra-1", 1, "a second"),
            ],
            vec![],
        );
        let buf = render(&app, at(1000));

        let y0 = find_line(&buf, "ra-0").expect("ra-0 present");
        let y1 = find_line(&buf, "rb-0").expect("rb-0 present");
        let y2 = find_line(&buf, "ra-1").expect("ra-1 present");
        assert!(
            y0 < y1 && y1 < y2,
            "rows keep flat order on screen: ra-0@{y0} rb-0@{y1} ra-1@{y2}"
        );
        // repo-a's run is split by repo-b, so its header is drawn twice.
        let repo_a_headers = (0..H)
            .filter(|&y| line_text(&buf, y).contains("▸ repo-a"))
            .count();
        assert_eq!(
            repo_a_headers, 2,
            "a non-contiguous repo re-emits its header"
        );
    }

    #[test]
    fn filtered_empty_shows_filter_hint() {
        // Rows exist but the priority filter hides them all: the user must be
        // told it is a filter, not an unconfigured roster.
        let mut app = app_with(vec![row("repo-a", "ra-2", 2, "low prio")], vec![]);
        app.reduce(Msg::TogglePriorityFilter); // HighOnly -> hides the P2 row
        let buf = render(&app, at(1000));

        assert!(
            find_line(&buf, NO_MATCH_HINT).is_some(),
            "filtered-empty shows the filter hint"
        );
        assert!(
            find_line(&buf, EMPTY_HINT).is_none(),
            "filtered-empty is not misreported as an unconfigured roster"
        );
    }

    #[test]
    fn keeps_selection_in_viewport() {
        // A list far taller than the 24-row screen: selecting the last row must
        // scroll it into view (and the first row out).
        let rows: Vec<Row> = (0..40)
            .map(|n| row("session-tui", &format!("ra-{n:02}"), 1, "task"))
            .collect();
        let mut app = app_with(rows, vec![]);
        for _ in 0..39 {
            app.reduce(Msg::SelectNext); // selection -> last row (ra-39)
        }
        let buf = render(&app, at(1000));

        let sel_y = find_line(&buf, "ra-39").expect("selected last row is on screen");
        assert!(
            buf.cell((2, sel_y))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED),
            "the scrolled-to selection is highlighted"
        );
        assert!(
            find_line(&buf, "ra-00").is_none(),
            "the first row has scrolled off the top"
        );
        // The repo header scrolled off, but a sticky header keeps attribution.
        assert!(
            find_line(&buf, "▸ session-tui").is_some(),
            "a scrolled viewport still shows the governing repo header"
        );
    }

    #[test]
    fn scroll_offset_keeps_selection_visible() {
        // Fits above the fold: no scroll.
        assert_eq!(scroll_offset(Some(3), 10, 40), 0);
        assert_eq!(scroll_offset(None, 10, 40), 0);
        // Short list (fits entirely): never scroll.
        assert_eq!(scroll_offset(Some(5), 10, 8), 0);
        // Past the fold: anchor selection to the last visible row.
        assert_eq!(scroll_offset(Some(10), 10, 40), 1);
        assert_eq!(scroll_offset(Some(39), 10, 40), 30);
    }

    #[test]
    fn failed_initial_refresh_shows_hint_not_loading() {
        // The first refresh fails (no snapshot): the App stays in `Loading` with
        // `stale` cleared. The list must not show a permanent spinner.
        let mut app = App::new();
        app.reduce(Msg::RefreshCompleted {
            snapshot: None,
            warnings: vec!["hub sync failed".into()],
        });
        let buf = render(&app, at(1000));

        assert!(
            find_line(&buf, "Loading…").is_none(),
            "a concluded (failed) refresh is not still 'Loading…'"
        );
        assert!(
            find_line(&buf, EMPTY_HINT).is_some(),
            "the empty hint is shown instead"
        );
        assert!(
            line_text(&buf, H - 1).contains("hub sync failed"),
            "the failure text is surfaced in the status bar"
        );
    }

    // ---- Detail pane (Slice 10) ----

    fn dep(id: &str, title: &str, status: &str, kind: &str) -> String {
        format!("⛔ {kind}: {id} {title} ({status})")
    }

    /// Build representative human-readable `bd show` output for layout tests.
    fn issue_detail(id: &str, title: &str, desc: &str, deps: Vec<String>) -> String {
        let mut output = format!("{id}  {title}\nopen · P2 · task · 0 comments\n\n{desc}");
        if !deps.is_empty() {
            output.push_str("\n\nDependencies:\n");
            output.push_str(&deps.join("\n"));
        }
        output
    }

    /// An app in `Detail` on the (single) row `id`, with the pane advanced to the
    /// given detail via the real reduce path. `None` leaves it `Loading`.
    fn app_in_detail(id: &str, ready: Option<Result<String, String>>) -> App {
        let mut app = app_with(vec![row("session-tui", id, 2, "Blocked task")], vec![]);
        // The single OpenDetail on a fresh app yields request token 1.
        let token = match app.reduce(Msg::OpenDetail).as_slice() {
            [crate::app::Effect::FetchDetail { token, .. }] => *token,
            other => panic!("expected one FetchDetail, got {other:?}"),
        };
        if let Some(detail) = ready {
            app.reduce(Msg::DetailReady { token, detail });
        }
        app
    }

    /// Render at a chosen size (detail split needs width variety).
    fn render_sized(app: &App, now: SystemTime, w: u16, h: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| {
                let _ = draw(f, app, now);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// The row `y` and starting column of the first buffer cell where `needle`
    /// begins (searching within the given width).
    fn find_at(buf: &Buffer, needle: &str, w: u16, h: u16) -> Option<(u16, usize)> {
        (0..h).find_map(|y| {
            let text: String = (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect();
            text.find(needle).map(|col| (y, col))
        })
    }

    #[test]
    fn renders_detail_pane() {
        let app = app_in_detail(
            "ra-4zf",
            Some(Ok(issue_detail(
                "ra-4zf",
                "Blocked task",
                "This one is blocked by the blocker",
                vec![dep("ra-z70", "Blocker task", "open", "blocks")],
            ))),
        );
        let (w, h) = (120, 24);
        let buf = render_sized(&app, at(1000), w, h);

        assert!(find_at(&buf, "Blocked task", w, h).is_some(), "issue title");
        assert!(
            find_at(&buf, "blocked by the blocker", w, h).is_some(),
            "description text"
        );
        let (dep_y, _) = find_at(&buf, "blocks:", w, h).expect("dependency line");
        let dep_row: String = (0..w)
            .map(|x| buf.cell((x, dep_y)).unwrap().symbol())
            .collect();
        assert!(dep_row.contains("ra-z70"), "dep id: {dep_row:?}");
        assert!(dep_row.contains("Blocker task"), "dep title: {dep_row:?}");
        assert!(dep_row.contains("(open)"), "dep status: {dep_row:?}");
        assert!(dep_row.contains('⛔'), "blocker glyph: {dep_row:?}");
    }

    #[test]
    fn renders_bd_show_output_without_reformatting() {
        let output = "\
○ ra-4zf [TASK] · Native bd title [● P2 · OPEN]
Owner: Brian Bell · Type: task

DESIGN

  First design line.
  Second design line.

CHILDREN
  ↳ ○ ra-child: Follow-up ● P2";
        let app = app_in_detail("ra-4zf", Some(Ok(output.to_string())));
        let (w, h) = (120, 24);
        let buf = render_sized(&app, at(1000), w, h);

        let (header_y, _) = find_at(&buf, "Native bd title", w, h).expect("native bd header");
        let (design_y, _) = find_at(&buf, "DESIGN", w, h).expect("native design heading");
        let (children_y, _) = find_at(&buf, "CHILDREN", w, h).expect("native children heading");
        assert_eq!(header_y, 1, "bd output starts at the top of the pane");
        assert!(
            header_y < design_y && design_y < children_y,
            "bd output line order is preserved"
        );
    }

    #[test]
    fn renders_detail_design_from_bd_show() {
        let output = "\
○ ra-4zf [TASK] · Designed task [● P2 · OPEN]

DESIGN

  Keep the reducer pure.
  Perform I/O in the runtime.";
        let app = app_in_detail("ra-4zf", Some(Ok(output.into())));
        let (w, h) = (120, 24);
        let buf = render_sized(&app, at(1000), w, h);

        assert!(
            find_at(&buf, "DESIGN", w, h).is_some(),
            "design section is labeled"
        );
        let (first_y, _) = find_at(&buf, "Keep the reducer pure", w, h).expect("first design line");
        let (second_y, _) =
            find_at(&buf, "Perform I/O in the runtime", w, h).expect("second design line");
        assert!(second_y > first_y, "design newlines render as new rows");
    }

    #[test]
    fn detail_pane_splits_right_when_wide() {
        let app = app_in_detail(
            "ra-4zf",
            Some(Ok(issue_detail(
                "ra-4zf",
                "Blocked task",
                "desc",
                vec![dep("ra-z70", "Blocker task", "open", "blocks")],
            ))),
        );
        let (w, h) = (120, 24);
        let buf = render_sized(&app, at(1000), w, h);

        // The detail-only dependency line sits in the right half.
        let (_, dep_col) = find_at(&buf, "blocks:", w, h).expect("dependency line");
        assert!(dep_col >= 60, "detail is in the right half: col {dep_col}");
        // A list-only marker (the repo header) sits in the left half.
        let (_, hdr_col) = find_at(&buf, "▸ ", w, h).expect("repo header");
        assert!(hdr_col < 60, "list is in the left half: col {hdr_col}");
    }

    #[test]
    fn detail_pane_stacks_below_when_narrow() {
        let app = app_in_detail(
            "ra-4zf",
            Some(Ok(issue_detail(
                "ra-4zf",
                "Blocked task",
                "desc",
                vec![dep("ra-z70", "Blocker task", "open", "blocks")],
            ))),
        );
        let (w, h) = (80, 24);
        let buf = render_sized(&app, at(1000), w, h);

        let (hdr_y, _) = find_at(&buf, "▸ ", w, h).expect("repo header");
        let (dep_y, dep_col) = find_at(&buf, "blocks:", w, h).expect("dependency line");
        assert!(
            dep_col < 40,
            "detail spans full width when stacked: col {dep_col}"
        );
        assert!(
            dep_y > hdr_y,
            "detail is stacked below the list: {dep_y} > {hdr_y}"
        );
    }

    #[test]
    fn detail_pane_divider_and_padding_when_wide() {
        let app = app_in_detail(
            "ra-4zf",
            Some(Ok(issue_detail("ra-4zf", "Blocked task", "desc", vec![]))),
        );
        let (w, h) = (120, 24);
        let buf = render_sized(&app, at(1000), w, h);

        // The split column carries a faint (dim) vertical rule between the panes.
        let cell = buf.cell((60, 1)).unwrap();
        assert_eq!(cell.symbol(), "│", "a vertical rule separates the panes");
        assert!(cell.modifier.contains(Modifier::DIM), "the rule is faint");
        // The pane's text is inset past the rule plus one column of padding.
        // (Checked cell-by-cell: `find_at` reports byte offsets, and the row
        // holds multi-byte glyphs — `▸`, `│` — before the title.)
        assert_eq!(
            buf.cell((61, 1)).unwrap().symbol(),
            " ",
            "a padding column follows the rule"
        );
        let inset: String = (62..70)
            .map(|x| buf.cell((x, 1)).unwrap().symbol())
            .collect();
        assert_eq!(
            inset, "ra-4zf  ",
            "detail text starts after the rule and padding"
        );
    }

    #[test]
    fn detail_pane_divider_and_padding_when_stacked() {
        let app = app_in_detail(
            "ra-4zf",
            Some(Ok(issue_detail("ra-4zf", "Blocked task", "desc", vec![]))),
        );
        let (w, h) = (80, 24);
        let buf = render_sized(&app, at(1000), w, h);

        // A faint horizontal rule tops the stacked pane…
        let rule_y = (0..h)
            .find(|&y| buf.cell((0, y)).unwrap().symbol() == "─")
            .expect("horizontal rule between the panes");
        assert!(
            buf.cell((0, rule_y))
                .unwrap()
                .modifier
                .contains(Modifier::DIM),
            "the rule is faint"
        );
        // …and the pane's text below it is left-padded by one column.
        let (y, col) = find_at(&buf, "ra-4zf  Blocked task", w, h).expect("detail title");
        assert!(y > rule_y, "detail sits below the rule");
        assert_eq!(col, 1, "one column of left padding");
    }

    #[test]
    fn renders_detail_loading() {
        let app = app_in_detail("ra-4zf", None); // opened, no DetailReady yet
        let (w, h) = (80, 24);
        let buf = render_sized(&app, at(1000), w, h);

        let (y, _) = find_at(&buf, "Loading", w, h).expect("loading placeholder");
        let line: String = (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect();
        assert!(line.contains("ra-4zf"), "loading names the id: {line:?}");
    }

    #[test]
    fn renders_detail_error_message() {
        let app = app_in_detail("ra-4zf", Some(Err("couldn't load ra-4zf: boom".into())));
        let (w, h) = (80, 24);
        let buf = render_sized(&app, at(1000), w, h);

        assert!(find_at(&buf, "boom", w, h).is_some(), "error message shown");
        // The list is still present behind/above the pane.
        assert!(
            find_at(&buf, "ra-4zf", w, h).is_some(),
            "list row still rendered"
        );
    }

    #[test]
    fn long_detail_dependencies_reachable_by_scroll() {
        // In the narrow stacked layout the pane is short; a long description would
        // push the dependency line off the bottom. Scrolling must bring it back,
        // and the clamp must not scroll past the content into blank.
        let long = "word ".repeat(200); // ~1000 cols → many rows even at full width
        let mut app = app_in_detail(
            "ra-4zf",
            Some(Ok(issue_detail(
                "ra-4zf",
                "Blocked task",
                long.trim(),
                vec![dep("ra-z70", "Blocker task", "open", "blocks")],
            ))),
        );
        let (w, h) = (80, 24);

        // Off the bottom initially.
        assert!(
            find_at(&render_sized(&app, at(1000), w, h), "blocks:", w, h).is_none(),
            "dependency starts below the fold"
        );

        // Page down enough to reach the end (clamped by the view).
        for _ in 0..10 {
            app.reduce(Msg::DetailPageDown);
        }
        let buf = render_sized(&app, at(1000), w, h);
        assert!(
            find_at(&buf, "blocks:", w, h).is_some(),
            "the dependency is reachable by scrolling"
        );
        // Clamped: the last content row is visible, not scrolled into blank.
        assert!(
            find_at(&buf, "(open)", w, h).is_some(),
            "over-scroll clamps to the content's end, showing no blank gap"
        );
    }

    #[test]
    fn renders_detail_labels() {
        let output = "ra-4zf  Blocked task\nLabels: urgent, backend";
        let app = app_in_detail("ra-4zf", Some(Ok(output.into())));
        let (w, h) = (80, 24);
        let buf = render_sized(&app, at(1000), w, h);

        let (y, _) = find_at(&buf, "Labels:", w, h).expect("labels line present");
        let line: String = (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect();
        assert!(line.contains("urgent"), "first label shown: {line:?}");
        assert!(line.contains("backend"), "second label shown: {line:?}");
    }

    #[test]
    fn wraps_long_description() {
        let long = "word ".repeat(60); // far wider than an 80-col pane
        let app = app_in_detail(
            "ra-4zf",
            Some(Ok(issue_detail("ra-4zf", "T", long.trim(), vec![]))),
        );
        let (w, h) = (80, 24);
        let buf = render_sized(&app, at(1000), w, h);

        // The repeated word appears on two or more distinct rows (it wrapped).
        let rows_with_word = (0..h)
            .filter(|&y| {
                let line: String = (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect();
                line.contains("word")
            })
            .count();
        assert!(
            rows_with_word >= 2,
            "long description wraps: {rows_with_word} rows"
        );
    }

    #[test]
    fn key_hints_are_mode_aware() {
        let list = app_with(vec![row("ra", "ra-1", 1, "t")], vec![]);
        let (w, h) = (80, 24);
        let list_title = line_text(&render_sized(&list, at(1000), w, h), 0);
        assert!(
            list_title.contains("enter"),
            "list advertises enter: {list_title:?}"
        );
        assert!(
            list_title.contains("/ search"),
            "list advertises the search key: {list_title:?}"
        );

        let detail = app_in_detail("ra-1", None);
        let detail_title = line_text(&render_sized(&detail, at(1000), w, h), 0);
        assert!(
            detail_title.contains("esc"),
            "detail advertises esc: {detail_title:?}"
        );
    }

    // ---- Cross-repo search (Slice 11) ----

    /// An app in `Search`+`Results` for `query`, holding `rows` (attributed),
    /// driven through the real reduce path (OpenSearch → type → SubmitSearch →
    /// SearchResults).
    fn app_in_search(query: &str, rows: Vec<Row>) -> App {
        let mut app = app_with(vec![row("ra", "ra-1", 1, "ready")], vec![]);
        app.reduce(Msg::OpenSearch);
        for c in query.chars() {
            app.reduce(Msg::SearchInput(c));
        }
        let token = match app.reduce(Msg::SubmitSearch).as_slice() {
            [crate::app::Effect::Search { token, .. }] => *token,
            other => panic!("expected one Search effect, got {other:?}"),
        };
        app.reduce(Msg::SearchResults {
            token,
            rows: Ok(rows),
        });
        app
    }

    #[test]
    fn renders_search_input_and_result_count() {
        let rows: Vec<Row> = (0..12)
            .map(|n| row("megaclock", &format!("mc-{n:02}"), 1, "a hit"))
            .collect();
        let app = app_in_search("foo", rows);
        let (w, h) = (80, 24);
        let buf = render_sized(&app, at(1000), w, h);

        assert!(
            find_at(&buf, "foo", w, h).is_some(),
            "the query appears on the input line"
        );
        assert!(
            find_at(&buf, "12 results for \"foo\"", w, h).is_some(),
            "the result count line is shown"
        );
        // A result row renders through the shared grouped renderer.
        assert!(
            find_at(&buf, "mc-00", w, h).is_some(),
            "a result row's id is rendered"
        );
        assert!(
            find_at(&buf, "▸ megaclock", w, h).is_some(),
            "results carry their repo header (shared row renderer)"
        );
    }

    #[test]
    fn renders_search_editing_and_empty() {
        // Editing: the input line shows the query with a cursor and a hint.
        let mut app = app_with(vec![row("ra", "ra-1", 1, "ready")], vec![]);
        app.reduce(Msg::OpenSearch);
        for c in "wip".chars() {
            app.reduce(Msg::SearchInput(c));
        }
        let (w, h) = (80, 24);
        let buf = render_sized(&app, at(1000), w, h);
        assert!(find_at(&buf, "wip", w, h).is_some(), "query while editing");
        assert!(
            find_at(&buf, "type a query", w, h).is_some(),
            "an editing hint is shown"
        );

        // Zero results shows "0 results", not the ready-list "no repos" hint.
        let empty = app_in_search("nope", vec![]);
        let buf = render_sized(&empty, at(1000), w, h);
        assert!(
            find_at(&buf, "0 results for \"nope\"", w, h).is_some(),
            "empty results are counted, not mistaken for an unconfigured roster"
        );
        assert!(
            find_at(&buf, EMPTY_HINT, w, h).is_none(),
            "the ready-list discover hint must not appear for an empty search"
        );
    }

    #[test]
    fn filtered_empty_search_shows_filter_hint() {
        // A filter that hides every result must explain itself, not blank out.
        let (w, h) = (80, 24);
        let mut app = app_in_search("foo", vec![row("ra", "ra-1", 2, "low prio hit")]);
        app.reduce(Msg::TogglePriorityFilter); // HighOnly hides the P2 result
        let buf = render_sized(&app, at(1000), w, h);
        assert!(
            find_at(&buf, NO_MATCH_HINT, w, h).is_some(),
            "filtered-empty search shows the filter hint, not a blank pane"
        );
        // The results-phase hint advertises the live filter keys.
        let title = line_text(&buf, 0);
        assert!(
            title.contains("f repo"),
            "results hint advertises f/p: {title:?}"
        );
    }

    #[test]
    fn editing_after_results_hides_stale_rows() {
        // After results for one query, `Esc` to edit must not leave the old rows
        // showing under the new (now empty) query.
        let (w, h) = (80, 24);
        let mut app = app_in_search("foo", vec![row("megaclock", "mc-1", 0, "old hit")]);
        assert!(
            find_at(&render_sized(&app, at(1000), w, h), "mc-1", w, h).is_some(),
            "the result is visible in the Results phase"
        );

        app.reduce(Msg::Back); // Results -> Editing
        assert_eq!(app.search_phase(), Some(&SearchPhase::Editing));
        assert!(
            find_at(&render_sized(&app, at(1000), w, h), "mc-1", w, h).is_none(),
            "the prior query's results are hidden while editing a new query"
        );
    }

    #[test]
    fn search_hints_are_phase_aware_and_error_renders_once() {
        let (w, h) = (80, 24);

        // Editing: the title hint advertises the editing keys.
        let mut editing = app_with(vec![row("ra", "ra-1", 1, "ready")], vec![]);
        editing.reduce(Msg::OpenSearch);
        let title = line_text(&render_sized(&editing, at(1000), w, h), 0);
        assert!(title.contains("type query"), "editing hint: {title:?}");

        // Results: the hint advertises navigation, not the inert "type query".
        let results = app_in_search("foo", vec![row("ra", "ra-1", 1, "hit")]);
        let title = line_text(&render_sized(&results, at(1000), w, h), 0);
        assert!(title.contains("j/k move"), "results hint: {title:?}");
        assert!(
            !title.contains("type query"),
            "results hint drops editing-only bindings: {title:?}"
        );

        // Error: the worker-prefixed message renders once, not double-prefixed.
        let mut err = app_with(vec![row("ra", "ra-1", 1, "ready")], vec![]);
        err.reduce(Msg::OpenSearch);
        for c in "foo".chars() {
            err.reduce(Msg::SearchInput(c));
        }
        let token = match err.reduce(Msg::SubmitSearch).as_slice() {
            [crate::app::Effect::Search { token, .. }] => *token,
            other => panic!("expected one Search effect, got {other:?}"),
        };
        err.reduce(Msg::SearchResults {
            token,
            rows: Err("search failed: boom".into()),
        });
        let buf = render_sized(&err, at(1000), w, h);
        let once = (0..h).any(|y| {
            let l: String = (0..w).map(|x| buf.cell((x, y)).unwrap().symbol()).collect();
            l.contains("search failed: boom") && !l.contains("search failed: search failed:")
        });
        assert!(once, "the error message renders once, not double-prefixed");

        // Error phase: the title hint omits the inert nav/open keys.
        let err_title = line_text(&buf, 0);
        assert!(
            !err_title.contains("j/k move"),
            "error hint omits inert navigation: {err_title:?}"
        );
        assert!(
            err_title.contains("esc edit"),
            "error hint keeps the active esc: {err_title:?}"
        );
    }

    #[test]
    fn renders_copy_confirmation() {
        // After a copy worker reports back, the status bar confirms it.
        let mut app = app_with(vec![row("megaclock", "mc-abc", 1, "task")], vec![]);
        let token = match app.reduce(Msg::CopyContext).as_slice() {
            [crate::app::Effect::Copy { token, .. }] => *token,
            other => panic!("expected one Copy effect, got {other:?}"),
        };
        app.reduce(Msg::Copied {
            token,
            payload: "cd /dev/megaclock && bd show mc-abc".into(),
            summary: "cd /dev/megaclock && bd show mc-abc".into(),
        });
        let status = line_text(&render(&app, at(1000)), H - 1);
        assert!(
            status.contains("copied: cd /dev/megaclock && bd show mc-abc"),
            "the status bar confirms the copy: {status:?}"
        );
    }

    #[test]
    fn format_age_humanizes() {
        let base = at(1_000_000);
        assert_eq!(format_age(base, base), "just now");
        assert_eq!(format_age(at(1_000_042), base), "42s ago");
        assert_eq!(format_age(at(1_000_180), base), "3m ago");
        assert_eq!(format_age(at(1_007_200), base), "2h ago");
        assert_eq!(format_age(at(1_432_000), base), "5d ago");
        // Clock skew: now precedes fetched -> saturates, never negative.
        assert_eq!(format_age(base, at(1_000_100)), "just now");
    }
}
