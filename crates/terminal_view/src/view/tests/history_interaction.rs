use super::*;

#[test]
fn late_history_matches_do_not_restore_prompt_after_tab_or_retyping() {
    let mut state = HistoryPromptState::from_input("out_");
    let revision = state.query_revision();
    state.dismiss();
    state.append_text("out_");
    assert!(!state.apply_query_matches(revision, vec!["out_file".into()]));
    assert!(state.matches().is_empty());
    let revision = state.query_revision();
    assert!(state.apply_query_matches(revision, vec!["out_file".into()]));
    assert_eq!(state.matches(), &["out_file"]);
    state.append_text("x");
    assert!(!state.apply_query_matches(revision, vec!["out_file".into()]));
}

#[test]
fn terminal_echo_does_not_repeat_history_database_queries() {
    let source = include_str!("../terminal_events.rs");
    let wakeup = source
        .split("TerminalModelEvent::Wakeup => {")
        .nth(1)
        .unwrap()
        .split("TerminalModelEvent::HostKeyVerificationRequired")
        .next()
        .unwrap();
    assert!(!wakeup.contains("refresh_history_prompt_matches"));
}

#[test]
fn history_query_results_are_rejected_after_hiding_or_changing_search_mode() {
    let mut state = HistoryPromptState::from_input("out_");
    let revision = state.query_revision();
    state.hide_dropdown();
    state.show_dropdown();
    assert!(!state.apply_query_matches(revision, vec!["out_file".into()]));
    let revision = state.query_revision();
    state.enter_search();
    assert!(!state.apply_query_matches(revision, vec!["out_file".into()]));
    let revision = state.query_revision();
    state.append_text("out_");
    assert!(!state.apply_query_matches(revision, vec!["out_file".into()]));
}

#[test]
fn inline_history_queries_run_on_background_executor() {
    let source = include_str!("../history_query.rs");
    let refresh = source
        .split("pub(super) fn refresh_history_prompt_matches")
        .nth(1)
        .unwrap()
        .split("pub(super) fn current_cd_completion_query")
        .next()
        .unwrap();
    let background = refresh
        .split("cx.background_spawn(async move {")
        .nth(1)
        .unwrap()
        .split("self.history_query_task =")
        .next()
        .unwrap();
    assert!(background.contains("snapshot.history_suggestions"));
    assert!(background.contains("snapshot.history_search_results"));
    assert!(!refresh.contains("terminal.history_suggestions"));
    assert!(!refresh.contains("terminal.history_search_results"));
}

#[test]
fn history_prompt_invalidates_multiline_paste() {
    let mut state = HistoryPromptState::from_input("git");

    state.apply_paste("status\nlog");

    assert!(!state.is_valid());
    assert_eq!(state.input(), "");

    state.append_text("c");

    assert_eq!(state.input(), "c");
    assert!(state.matches().is_empty());
}

#[test]
fn history_prompt_dismiss_keeps_current_input() {
    let mut state = HistoryPromptState::from_input("git s");
    state.set_matches(vec!["git status".to_string(), "git stash".to_string()]);

    state.dismiss_matches();

    assert_eq!(state.input(), "git s");
    assert_eq!(state.query_input(), "git s");
    assert!(state.matches().is_empty());
}

#[test]
fn history_prompt_dismisses_on_non_linear_inline_keys() {
    assert!(should_dismiss_history_prompt_for_keystroke(
        &Keystroke::parse("left").unwrap()
    ));
    assert!(should_dismiss_history_prompt_for_keystroke(
        &Keystroke::parse("ctrl-a").unwrap()
    ));
    assert!(should_dismiss_history_prompt_for_keystroke(
        &Keystroke::parse("ctrl-e").unwrap()
    ));
    assert!(should_dismiss_history_prompt_for_keystroke(
        &Keystroke::parse("alt-backspace").unwrap()
    ));
}

#[test]
fn history_prompt_keeps_tracking_for_linear_typing_keys() {
    assert!(!should_dismiss_history_prompt_for_keystroke(
        &Keystroke::parse("a").unwrap()
    ));
    assert!(!should_dismiss_history_prompt_for_keystroke(
        &Keystroke::parse("space").unwrap()
    ));
    assert!(!should_dismiss_history_prompt_for_keystroke(
        &Keystroke::parse("backspace").unwrap()
    ));
    assert!(!should_dismiss_history_prompt_for_keystroke(
        &Keystroke::parse("down").unwrap()
    ));
}

#[test]
fn printable_inline_input_is_deferred_to_text_system() {
    assert!(should_defer_inline_history_prompt_input_to_text_system(
        &Keystroke::parse("a").unwrap()
    ));
    assert!(should_defer_inline_history_prompt_input_to_text_system(
        &Keystroke::parse("shift-a").unwrap()
    ));
    assert!(should_defer_inline_history_prompt_input_to_text_system(
        &Keystroke::parse("space").unwrap()
    ));
}

#[test]
fn special_keys_still_bypass_text_system_defer() {
    assert!(!should_defer_inline_history_prompt_input_to_text_system(
        &Keystroke::parse("backspace").unwrap()
    ));
    assert!(!should_defer_inline_history_prompt_input_to_text_system(
        &Keystroke::parse("left").unwrap()
    ));
    assert!(!should_defer_inline_history_prompt_input_to_text_system(
        &Keystroke::parse("ctrl-a").unwrap()
    ));
}

#[test]
fn history_prompt_dismisses_on_mouse_interaction() {
    assert!(should_dismiss_history_prompt_for_mouse(MouseButton::Left));
    assert!(should_dismiss_history_prompt_for_mouse(MouseButton::Middle));
    assert!(should_dismiss_history_prompt_for_mouse(MouseButton::Right));
}

#[test]
fn right_click_uses_context_menu_when_quick_paste_is_disabled() {
    assert!(!should_direct_paste_on_right_click(
        false,
        MouseButton::Right
    ));
}

#[test]
fn right_click_directly_pastes_when_quick_paste_is_enabled() {
    assert!(should_direct_paste_on_right_click(true, MouseButton::Right));
    assert!(!should_direct_paste_on_right_click(true, MouseButton::Left));
}

#[test]
fn history_prompt_dismisses_on_scroll_navigation() {
    assert!(should_dismiss_history_prompt_for_scroll(1));
    assert!(should_dismiss_history_prompt_for_scroll(-2));
    assert!(!should_dismiss_history_prompt_for_scroll(0));
}

#[test]
fn history_prompt_resets_on_shell_input_start_event() {
    assert!(should_reset_history_prompt_for_terminal_event(
        &TerminalModelEvent::InputStart
    ));
    assert!(should_reset_history_prompt_for_terminal_event(
        &TerminalModelEvent::PromptStart
    ));
    assert!(should_reset_history_prompt_for_terminal_event(
        &TerminalModelEvent::CommandStart
    ));
    assert!(!should_reset_history_prompt_for_terminal_event(
        &TerminalModelEvent::Wakeup
    ));
}

#[test]
fn user_input_scroll_clears_pending_offset_even_when_already_at_bottom() {
    let pending_display_offset = StdCell::new(Some(12));

    assert!(!should_scroll_to_bottom_on_user_input(
        0,
        &pending_display_offset
    ));
    assert_eq!(pending_display_offset.take(), None);
}

#[test]
fn user_input_scroll_requests_bottom_when_terminal_is_scrolled_up() {
    let pending_display_offset = StdCell::new(Some(12));

    assert!(should_scroll_to_bottom_on_user_input(
        5,
        &pending_display_offset
    ));
    assert_eq!(pending_display_offset.take(), None);
}
