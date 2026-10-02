//! Keypress → [`Msg`] mapping. The **only** file that imports `crossterm`, so
//! [`super::App::reduce`] stays backend-agnostic. Unmapped keys and key-release
//! events yield `None` (the runtime ignores them).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use super::{InputContext, Msg};

/// Rows PageDown/PageUp scroll the health panel.
const HEALTH_PAGE_ROWS: i16 = 10;

/// Decode a crossterm key event into a [`Msg`], or `None` for an unmapped key or
/// a key-release event (so a press+release fires a single message).
///
/// `context` selects the mapping: while the search query input is focused, keys
/// edit text (any `Char` appends, `Backspace` deletes, `Enter` submits, `Esc`
/// cancels); otherwise the command/navigation mapping applies (so `/` opens
/// search, `j`/`k` move, `Enter` opens the detail, etc.). The runtime supplies
/// `context` from the app's live overlay/search state.
pub fn map_key(event: KeyEvent, context: InputContext) -> Option<Msg> {
    // Ignore key releases; map presses and repeats (holding `j` scrolls). On
    // terminals without the kitty keyboard protocol crossterm reports `Press`.
    if event.kind == KeyEventKind::Release {
        return None;
    }
    if context == InputContext::SearchEditing {
        return match event.code {
            KeyCode::Char(c) => Some(Msg::SearchInput(c)),
            KeyCode::Backspace => Some(Msg::SearchBackspace),
            KeyCode::Enter => Some(Msg::SubmitSearch),
            KeyCode::Esc => Some(Msg::Back),
            _ => None,
        };
    }
    if context == InputContext::RepoPicker {
        return match event.code {
            KeyCode::Char('q') => Some(Msg::Quit),
            KeyCode::Char('j') | KeyCode::Down => Some(Msg::RepoPickerNext),
            KeyCode::Char('k') | KeyCode::Up => Some(Msg::RepoPickerPrev),
            KeyCode::Enter => Some(Msg::ConfirmRepoPicker),
            KeyCode::Esc => Some(Msg::Back),
            _ => None,
        };
    }
    if context == InputContext::Health {
        return match event.code {
            KeyCode::Char('q') => Some(Msg::Quit),
            KeyCode::Char('r') => Some(Msg::Refresh),
            KeyCode::Char('h') => Some(Msg::ToggleHealth),
            KeyCode::Char('j' | 'J') | KeyCode::Down => Some(Msg::HealthScroll(1)),
            KeyCode::Char('k' | 'K') | KeyCode::Up => Some(Msg::HealthScroll(-1)),
            KeyCode::PageDown => Some(Msg::HealthScroll(HEALTH_PAGE_ROWS)),
            KeyCode::PageUp => Some(Msg::HealthScroll(-HEALTH_PAGE_ROWS)),
            KeyCode::Esc => Some(Msg::Back),
            _ => None,
        };
    }
    match event.code {
        KeyCode::Char('q') => Some(Msg::Quit),
        KeyCode::Char('/') => Some(Msg::OpenSearch),
        KeyCode::Char('h') => Some(Msg::ToggleHealth),
        KeyCode::Char('r') => Some(Msg::Refresh),
        KeyCode::Char('y') => Some(Msg::CopyContext),
        KeyCode::Char('Y') => Some(Msg::CopyMarkdown),
        KeyCode::Char('f') => Some(Msg::OpenRepoPicker),
        KeyCode::Char('p') => Some(Msg::TogglePriorityFilter),
        KeyCode::Char('j') | KeyCode::Down => Some(Msg::SelectNext),
        KeyCode::Char('k') | KeyCode::Up => Some(Msg::SelectPrev),
        KeyCode::Char('J') => Some(Msg::DetailScrollDown),
        KeyCode::Char('K') => Some(Msg::DetailScrollUp),
        KeyCode::PageDown => Some(Msg::DetailPageDown),
        KeyCode::PageUp => Some(Msg::DetailPageUp),
        KeyCode::Enter => Some(Msg::OpenDetail),
        KeyCode::Esc => Some(Msg::Back),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    /// A press event with no modifiers (crossterm's `new` sets kind = Press).
    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn maps_command_keys() {
        assert_eq!(
            map_key(press(KeyCode::Char('q')), InputContext::Normal),
            Some(Msg::Quit)
        );
        assert_eq!(
            map_key(press(KeyCode::Char('/')), InputContext::Normal),
            Some(Msg::OpenSearch)
        );
        assert_eq!(
            map_key(press(KeyCode::Char('r')), InputContext::Normal),
            Some(Msg::Refresh)
        );
        assert_eq!(
            map_key(press(KeyCode::Char('y')), InputContext::Normal),
            Some(Msg::CopyContext)
        );
        assert_eq!(
            map_key(press(KeyCode::Char('Y')), InputContext::Normal),
            Some(Msg::CopyMarkdown),
            "shifted Y copies the markdown block"
        );
        assert_eq!(
            map_key(press(KeyCode::Enter), InputContext::Normal),
            Some(Msg::OpenDetail)
        );
        assert_eq!(
            map_key(press(KeyCode::Esc), InputContext::Normal),
            Some(Msg::Back)
        );
    }

    #[test]
    fn maps_navigation_keys() {
        assert_eq!(
            map_key(press(KeyCode::Char('j')), InputContext::Normal),
            Some(Msg::SelectNext)
        );
        assert_eq!(
            map_key(press(KeyCode::Down), InputContext::Normal),
            Some(Msg::SelectNext)
        );
        assert_eq!(
            map_key(press(KeyCode::Char('k')), InputContext::Normal),
            Some(Msg::SelectPrev)
        );
        assert_eq!(
            map_key(press(KeyCode::Up), InputContext::Normal),
            Some(Msg::SelectPrev)
        );
        // Shifted j/k scroll the detail pane rather than moving the selection.
        assert_eq!(
            map_key(press(KeyCode::Char('J')), InputContext::Normal),
            Some(Msg::DetailScrollDown)
        );
        assert_eq!(
            map_key(press(KeyCode::Char('K')), InputContext::Normal),
            Some(Msg::DetailScrollUp)
        );
        assert_eq!(
            map_key(press(KeyCode::PageDown), InputContext::Normal),
            Some(Msg::DetailPageDown)
        );
        assert_eq!(
            map_key(press(KeyCode::PageUp), InputContext::Normal),
            Some(Msg::DetailPageUp)
        );
        // While editing a query the same keys are literal text.
        assert_eq!(
            map_key(press(KeyCode::Char('J')), InputContext::SearchEditing),
            Some(Msg::SearchInput('J'))
        );
    }

    #[test]
    fn h_opens_the_health_panel_and_its_keys_scroll_and_close_it() {
        assert_eq!(
            map_key(press(KeyCode::Char('h')), InputContext::Normal),
            Some(Msg::ToggleHealth)
        );
        let health = |code| map_key(press(code), InputContext::Health);
        assert_eq!(health(KeyCode::Char('h')), Some(Msg::ToggleHealth));
        assert_eq!(health(KeyCode::Esc), Some(Msg::Back));
        assert_eq!(health(KeyCode::Char('j')), Some(Msg::HealthScroll(1)));
        assert_eq!(health(KeyCode::Up), Some(Msg::HealthScroll(-1)));
        assert_eq!(
            health(KeyCode::PageDown),
            Some(Msg::HealthScroll(HEALTH_PAGE_ROWS))
        );
        assert_eq!(health(KeyCode::Char('r')), Some(Msg::Refresh));
        assert_eq!(health(KeyCode::Char('q')), Some(Msg::Quit));
        assert_eq!(
            health(KeyCode::Enter),
            None,
            "list commands are inert under the panel"
        );
        assert_eq!(
            map_key(press(KeyCode::Char('h')), InputContext::SearchEditing),
            Some(Msg::SearchInput('h')),
            "h types into a search query"
        );
    }

    #[test]
    fn maps_filter_keys() {
        assert_eq!(
            map_key(press(KeyCode::Char('f')), InputContext::Normal),
            Some(Msg::OpenRepoPicker)
        );
        assert_eq!(
            map_key(press(KeyCode::Char('p')), InputContext::Normal),
            Some(Msg::TogglePriorityFilter)
        );
    }

    #[test]
    fn maps_search_input_keys() {
        // While editing the query, every char is text — including keys that are
        // commands otherwise (`q`, `/`, `j`) — and the special keys drive submit/
        // edit/cancel.
        assert_eq!(
            map_key(press(KeyCode::Char('f')), InputContext::SearchEditing),
            Some(Msg::SearchInput('f'))
        );
        assert_eq!(
            map_key(press(KeyCode::Char('q')), InputContext::SearchEditing),
            Some(Msg::SearchInput('q')),
            "a command key is literal text while editing"
        );
        assert_eq!(
            map_key(press(KeyCode::Char('Y')), InputContext::SearchEditing),
            Some(Msg::SearchInput('Y')),
            "a shifted copy key is literal text while editing"
        );
        assert_eq!(
            map_key(press(KeyCode::Backspace), InputContext::SearchEditing),
            Some(Msg::SearchBackspace)
        );
        assert_eq!(
            map_key(press(KeyCode::Enter), InputContext::SearchEditing),
            Some(Msg::SubmitSearch)
        );
        assert_eq!(
            map_key(press(KeyCode::Esc), InputContext::SearchEditing),
            Some(Msg::Back)
        );
        // Not editing: the same `j` is a navigation command.
        assert_eq!(
            map_key(press(KeyCode::Char('j')), InputContext::Normal),
            Some(Msg::SelectNext)
        );
    }

    #[test]
    fn picker_context_captures_only_modal_keys_and_quit() {
        let context = InputContext::RepoPicker;
        assert_eq!(
            map_key(press(KeyCode::Char('j')), context),
            Some(Msg::RepoPickerNext)
        );
        assert_eq!(
            map_key(press(KeyCode::Down), context),
            Some(Msg::RepoPickerNext)
        );
        assert_eq!(
            map_key(press(KeyCode::Char('k')), context),
            Some(Msg::RepoPickerPrev)
        );
        assert_eq!(
            map_key(press(KeyCode::Up), context),
            Some(Msg::RepoPickerPrev)
        );
        assert_eq!(
            map_key(press(KeyCode::Enter), context),
            Some(Msg::ConfirmRepoPicker)
        );
        assert_eq!(map_key(press(KeyCode::Esc), context), Some(Msg::Back));
        assert_eq!(map_key(press(KeyCode::Char('q')), context), Some(Msg::Quit));
        assert_eq!(map_key(press(KeyCode::Char('r')), context), None);
        assert_eq!(map_key(press(KeyCode::Char('p')), context), None);
        assert_eq!(map_key(press(KeyCode::Char('/')), context), None);
    }

    #[test]
    fn ignores_unmapped_and_release() {
        // An unmapped character.
        assert_eq!(
            map_key(press(KeyCode::Char('z')), InputContext::Normal),
            None
        );
        // A release event for an otherwise-mapped key: ignored, so a press+release
        // does not fire the message twice.
        let release = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
            KeyEventState::NONE,
        );
        assert_eq!(map_key(release, InputContext::Normal), None);
        assert_eq!(
            map_key(release, InputContext::SearchEditing),
            None,
            "release ignored while editing too"
        );
    }
}
