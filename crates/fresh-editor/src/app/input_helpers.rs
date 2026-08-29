//! Miscellaneous input helpers on `Editor`.
//!
//! Tab navigation (switch_to_previous_tab, start_switch_to_tab_prompt),
//! character insertion (handle_insert_char_prompt, handle_insert_char_editor,
//! apply_action_as_events), cursor-movement tracking
//! (track_cursor_movement), and composite-buffer key routing
//! (try_route_composite_key). Small helpers grouped here so input.rs can
//! contain only the central key/action dispatch.

use anyhow::Result as AnyhowResult;
use rust_i18n::t;

use crate::input::keybindings::Action;
use crate::model::event::{BufferId, Event};
use crate::view::prompt::PromptType;

use super::Editor;

impl Editor {
    /// Refuse the current operation when the active buffer is read-only.
    ///
    /// Returns `true` after posting the standard "Editing disabled" status
    /// message — callers should `return Ok(())` immediately. Centralised so
    /// every mutation entry point (Replace, FormatBuffer, ShellCommandReplace,
    /// …) enforces the same contract instead of hand-rolling the check.
    pub(super) fn refuse_if_editing_disabled(&mut self) -> bool {
        if self.active_window().is_editing_disabled() {
            self.set_status_message(t!("buffer.editing_disabled").to_string());
            true
        } else {
            false
        }
    }

    /// Switch to the previously active tab in the current split.
    /// Handles both buffer tabs and group tabs via the focus-history LRU.
    pub(super) fn switch_to_previous_tab(&mut self) {
        self.switch_to_mru_tab(false);
    }

    /// Switch to the next tab in the MRU cycle (the one before the
    /// immediate predecessor). Mirrors `switch_to_previous_tab` but steps
    /// forward through the focus history.
    pub(super) fn switch_to_next_tab(&mut self) {
        self.switch_to_mru_tab(true);
    }

    /// Shared MRU tab switch. `forward == false` targets the
    /// most-recently-focused tab (`previous_tab`); `forward == true`
    /// targets the one before it (`next_tab`). Both buffer and group
    /// tabs are handled.
    fn switch_to_mru_tab(&mut self, forward: bool) {
        use crate::view::split::TabTarget;
        let active_split = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .active_split();
        let target = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(_, vs)| vs)
            .expect("active window must have a populated split layout")
            .get(&active_split)
            .and_then(|vs| {
                if forward {
                    vs.next_tab()
                } else {
                    vs.previous_tab()
                }
            });

        match target {
            Some(TabTarget::Buffer(prev_id)) => {
                let is_valid = self
                    .windows
                    .get(&self.active_window)
                    .and_then(|w| w.buffers.splits())
                    .map(|(_, vs)| vs)
                    .expect("active window must have a populated split layout")
                    .get(&active_split)
                    .is_some_and(|vs| vs.has_buffer(prev_id));

                if is_valid && prev_id != self.active_buffer() {
                    self.active_window_mut()
                        .position_history
                        .commit_pending_movement();
                    let cursors = self.active_cursors();
                    let position = cursors.primary().position;
                    let anchor = cursors.primary().anchor;
                    let active_buffer_id = self.active_buffer();
                    let ph = &mut self.active_window_mut().position_history;
                    ph.record_movement(active_buffer_id, position, anchor);
                    ph.commit_pending_movement();
                    // Pre-remove the target from the focus history so the
                    // switch's own push_focus(leaving) yields a clean ring
                    // (the active tab must not linger as a stale entry).
                    if let Some(vs) = self
                        .split_view_states_mut()
                        .get_mut(&active_split)
                    {
                        vs.remove_focus(TabTarget::Buffer(prev_id));
                    }
                    self.set_active_buffer(prev_id);
                } else if !is_valid {
                    self.set_status_message(t!("status.previous_tab_closed").to_string());
                }
            }
            Some(TabTarget::Group(leaf_id)) => {
                if self.active_window().grouped_subtrees.contains_key(&leaf_id) {
                    // `activate_group_tab` does not touch focus_history, so
                    // record the leaving target and drop the group target
                    // to keep the ring clean (mirrors set_active_buffer).
                    if let Some(vs) = self
                        .split_view_states_mut()
                        .get_mut(&active_split)
                    {
                        let leaving = vs.active_target();
                        vs.remove_focus(TabTarget::Group(leaf_id));
                        vs.push_focus(leaving);
                    }
                    self.activate_group_tab(active_split, leaf_id);
                } else {
                    self.set_status_message(t!("status.previous_tab_closed").to_string());
                }
            }
            None => {
                self.set_status_message(t!("status.no_previous_tab").to_string());
            }
        }
    }

    /// Start the switch-to-tab-by-name prompt with suggestions from open buffers
    pub(super) fn start_switch_to_tab_prompt(&mut self) {
        let active_split = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .active_split();
        let open_buffers: Vec<BufferId> = if let Some(view_state) = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(_, vs)| vs)
            .expect("active window must have a populated split layout")
            .get(&active_split)
        {
            view_state.buffer_tab_ids_vec()
        } else {
            return;
        };

        if open_buffers.is_empty() {
            self.set_status_message(t!("status.no_tabs_in_split").to_string());
            return;
        }

        // Find the current buffer's index
        let current_index = open_buffers
            .iter()
            .position(|&id| id == self.active_buffer())
            .unwrap_or(0);

        let suggestions: Vec<crate::input::commands::Suggestion> = open_buffers
            .iter()
            .map(|&buffer_id| {
                let display_name = self
                    .active_window()
                    .buffer_metadata
                    .get(&buffer_id)
                    .map(|m| m.display_name.clone())
                    .unwrap_or_else(|| format!("Buffer {:?}", buffer_id));

                let is_current = buffer_id == self.active_buffer();
                let is_modified = self
                    .buffers()
                    .get(&buffer_id)
                    .is_some_and(|b| b.buffer.is_modified());

                let description = match (is_current, is_modified) {
                    (true, true) => Some("(current, modified)".to_string()),
                    (true, false) => Some("(current)".to_string()),
                    (false, true) => Some("(modified)".to_string()),
                    (false, false) => None,
                };

                crate::input::commands::Suggestion {
                    description_spans: None,
                    text: display_name,
                    description,
                    value: Some(buffer_id.0.to_string()),
                    disabled: false,
                    keybinding: None,
                    source: None,
                }
            })
            .collect();

        self.active_window_mut().prompt = Some(crate::view::prompt::Prompt::with_suggestions(
            "Switch to tab: ".to_string(),
            PromptType::SwitchToTab,
            suggestions,
        ));

        if let Some(prompt) = self.active_window_mut().prompt.as_mut() {
            if !prompt.suggestions.is_empty() {
                prompt.selected_suggestion = Some(current_index);
            }
        }
    }

    /// Switch to a tab by its BufferId
    pub(crate) fn switch_to_tab(&mut self, buffer_id: BufferId) {
        // Verify the buffer exists and is open in the current split
        let active_split = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .active_split();
        let is_valid = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(_, vs)| vs)
            .expect("active window must have a populated split layout")
            .get(&active_split)
            .is_some_and(|vs| vs.has_buffer(buffer_id));

        if !is_valid {
            self.set_status_message(t!("status.tab_not_found").to_string());
            return;
        }

        if buffer_id != self.active_buffer() {
            // Save current position before switching
            self.active_window_mut()
                .position_history
                .commit_pending_movement();

            let cursors = self.active_cursors();
            let position = cursors.primary().position;
            let anchor = cursors.primary().anchor;
            let active_buffer_id = self.active_buffer();
            let ph = &mut self.active_window_mut().position_history;
            ph.record_movement(active_buffer_id, position, anchor);
            ph.commit_pending_movement();

            self.set_active_buffer(buffer_id);
        }
    }

    /// Switch to a tab target (buffer or group) in the active split.
    /// Shared by the MRU popup confirm path; records position history and
    /// keeps the focus-history ring clean exactly like `switch_to_mru_tab`.
    pub(crate) fn switch_to_tab_target(&mut self, target: crate::view::split::TabTarget) {
        use crate::view::split::TabTarget;
        let active_split = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .active_split();

        match target {
            TabTarget::Buffer(buffer_id) => {
                let is_valid = self
                    .windows
                    .get(&self.active_window)
                    .and_then(|w| w.buffers.splits())
                    .map(|(_, vs)| vs)
                    .expect("active window must have a populated split layout")
                    .get(&active_split)
                    .is_some_and(|vs| vs.has_buffer(buffer_id));
                if is_valid && buffer_id != self.active_buffer() {
                    self.active_window_mut()
                        .position_history
                        .commit_pending_movement();
                    let cursors = self.active_cursors();
                    let position = cursors.primary().position;
                    let anchor = cursors.primary().anchor;
                    let active_buffer_id = self.active_buffer();
                    let ph = &mut self.active_window_mut().position_history;
                    ph.record_movement(active_buffer_id, position, anchor);
                    ph.commit_pending_movement();
                    if let Some(vs) = self.split_view_states_mut().get_mut(&active_split) {
                        vs.remove_focus(TabTarget::Buffer(buffer_id));
                    }
                    self.set_active_buffer(buffer_id);
                } else if !is_valid {
                    self.set_status_message(t!("status.previous_tab_closed").to_string());
                }
            }
            TabTarget::Group(leaf_id) => {
                if self.active_window().grouped_subtrees.contains_key(&leaf_id) {
                    if let Some(vs) = self.split_view_states_mut().get_mut(&active_split) {
                        let leaving = vs.active_target();
                        vs.remove_focus(TabTarget::Group(leaf_id));
                        vs.push_focus(leaving);
                    }
                    self.activate_group_tab(active_split, leaf_id);
                } else {
                    self.set_status_message(t!("status.previous_tab_closed").to_string());
                }
            }
        }
    }

    /// Open the MRU tab picker popup listing recently focused tabs
    /// (most-recent first), with arrow-key navigation and Enter to switch.
    pub(super) fn show_mru_tab_popup(&mut self) {
        use crate::model::event::{
            PopupContentData, PopupData, PopupKindHint, PopupListItemData, PopupPositionData,
        };
        use crate::view::split::TabTarget;

        let active_split = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .active_split();

        // Snapshot the split's tab state (focus history + open tabs) so we
        // can build the list without holding a borrow across the popup show.
        let (focus_history, open_buffers, active_target) = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(_, vs)| vs)
            .expect("active window must have a populated split layout")
            .get(&active_split)
            .map(|vs| (vs.focus_history.clone(), vs.open_buffers.clone(), vs.active_target()))
            .unwrap_or_else(|| (Vec::new(), Vec::new(), TabTarget::Buffer(self.active_buffer())));

        // Build the MRU list: most-recent-first from focus_history, then any
        // remaining open tabs, deduped and filtered to still-valid targets.
        let mut seen: Vec<TabTarget> = Vec::new();
        let mut ordered: Vec<TabTarget> = Vec::new();
        for t in focus_history.iter().rev().chain(open_buffers.iter()) {
            if seen.contains(t) {
                continue;
            }
            seen.push(*t);
            ordered.push(*t);
        }

        let group_names = self.active_window().grouped_subtrees.iter().filter_map(|(leaf, node)| {
            if let crate::view::split::SplitNode::Grouped { name, .. } = node {
                Some((*leaf, name.clone()))
            } else {
                None
            }
        }).collect::<std::collections::HashMap<_, _>>();

        let items: Vec<PopupListItemData> = ordered
            .into_iter()
            .filter_map(|t| {
                let (text, data) = match t {
                    TabTarget::Buffer(id) => {
                        let name = self
                            .active_window()
                            .buffer_metadata
                            .get(&id)
                            .map(|m| m.display_name.clone())
                            .unwrap_or_else(|| format!("Buffer {:?}", id));
                        (name, format!("buffer:{}", id.0))
                    }
                    TabTarget::Group(leaf) => {
                        let name = group_names
                            .get(&leaf)
                            .cloned()
                            .unwrap_or_else(|| format!("Group {:?}", leaf));
                        (name, format!("group:{}", leaf.0.0))
                    }
                };
                let is_current = t == active_target;
                let detail = if is_current { Some("(current)".to_string()) } else { None };
                Some(PopupListItemData {
                    text,
                    detail,
                    icon: None,
                    data: Some(data),
                })
            })
            .collect();

        if items.is_empty() {
            self.set_status_message(t!("status.no_previous_tab").to_string());
            return;
        }

        let popup = PopupData {
            kind: PopupKindHint::List,
            title: Some(t!("cmd.show_mru_tab_popup").to_string()),
            description: None,
            transient: false,
            content: PopupContentData::List { items, selected: 0 },
            position: PopupPositionData::Centered,
            width: 50,
            max_height: 15,
            bordered: true,
        };

        self.show_popup_with_resolver(popup, crate::view::popup::PopupResolver::MruTab);
    }

    /// Handle character insertion in prompt mode.
    pub(super) fn handle_insert_char_prompt(&mut self, c: char) -> AnyhowResult<()> {
        // Check if this is the query-replace confirmation prompt
        if let Some(ref prompt) = self.active_window_mut().prompt {
            if prompt.prompt_type == PromptType::QueryReplaceConfirm {
                return self.handle_interactive_replace_key(c);
            }
        }

        // Reset history navigation when user starts typing
        // This allows them to press Up to get back to history items
        // Reset history navigation when typing in a prompt
        if let Some(ref prompt) = self.active_window_mut().prompt {
            if let Some(key) = Self::prompt_type_to_history_key(&prompt.prompt_type) {
                if let Some(history) = self.active_window_mut().prompt_histories.get_mut(&key) {
                    history.reset_navigation();
                }
            }
        }

        if let Some(prompt) = self.prompt_mut() {
            // Use insert_str to properly handle selection deletion
            let s = c.to_string();
            prompt.insert_str(&s);
        }
        self.update_prompt_suggestions();
        Ok(())
    }

    /// Handle character insertion in normal editor mode.
    pub(super) fn handle_insert_char_editor(&mut self, c: char) -> AnyhowResult<()> {
        // Check if editing is disabled (show_cursors = false)
        if self.active_window().is_editing_disabled() {
            self.set_status_message(t!("buffer.editing_disabled").to_string());
            return Ok(());
        }

        // Cancel any pending LSP requests since the text is changing
        self.active_window_mut().cancel_pending_lsp_requests();

        if let Some(events) = self
            .active_window_mut()
            .action_to_events(Action::InsertChar(c))
        {
            if events.len() > 1 {
                // Multi-cursor: use optimized bulk edit (O(n) instead of O(n²))
                let description = format!("Insert '{}'", c);
                if let Some(bulk_edit) = self.apply_events_as_bulk_edit(events, description.clone())
                {
                    self.active_event_log_mut().append(bulk_edit);
                }
            } else {
                // Single cursor - apply normally
                for event in events {
                    self.active_event_log_mut().append(event.clone());
                    self.apply_event_to_active_buffer(&event);
                }
            }
        }

        // Auto-trigger signature help on '(' and ','
        if c == '(' || c == ',' {
            self.request_signature_help();
        }

        // Auto-trigger completion on trigger characters
        self.maybe_trigger_completion(c);

        Ok(())
    }

    /// Apply an action by converting it to events.
    ///
    /// This is the catch-all handler for actions that can be converted to buffer events
    /// (cursor movements, text edits, etc.). It handles batching for multi-cursor,
    /// position history tracking, and editing permission checks.
    pub(super) fn apply_action_as_events(&mut self, action: Action) -> AnyhowResult<()> {
        // Check if active buffer is a composite buffer - handle scroll/movement specially
        let buffer_id = self.active_buffer();
        if self.active_window().is_composite_buffer(buffer_id) {
            if let Some(_handled) = self.handle_composite_action(buffer_id, &action) {
                return Ok(());
            }
        }

        // Get description before moving action
        let action_description = format!("{:?}", action);

        if let Some(events) = self.active_window_mut().action_to_events(action) {
            // Refuse the action if it would mutate a read-only buffer.
            // Checking at the event level (rather than maintaining a hand-
            // written allowlist of "editing" actions) catches every action
            // that produces Insert/Delete, including SortLines, OpenLine,
            // case transforms, and anything added in the future.
            let has_buffer_mods = events
                .iter()
                .any(|e| matches!(e, Event::Insert { .. } | Event::Delete { .. }));
            if has_buffer_mods && self.active_window().is_editing_disabled() {
                self.set_status_message(t!("buffer.editing_disabled").to_string());
                return Ok(());
            }

            if events.len() > 1 {
                if has_buffer_mods {
                    // Multi-cursor buffer edit: use optimized bulk edit (O(n) instead of O(n²))
                    if let Some(bulk_edit) =
                        self.apply_events_as_bulk_edit(events.clone(), action_description)
                    {
                        self.active_event_log_mut().append(bulk_edit);
                    }
                } else {
                    // Multi-cursor non-buffer operation: use Batch for atomic undo
                    let batch = Event::Batch {
                        events: events.clone(),
                        description: action_description,
                    };
                    self.active_event_log_mut().append(batch.clone());
                    self.apply_event_to_active_buffer(&batch);
                }

                // Track position history for all events
                for event in &events {
                    self.track_cursor_movement(event);
                }
            } else {
                // Single cursor - apply normally
                for event in events {
                    self.log_and_apply_event(&event);
                    self.track_cursor_movement(&event);
                }
            }
        }

        Ok(())
    }

    /// Track cursor movement in position history if applicable.
    pub(super) fn track_cursor_movement(&mut self, event: &Event) {
        if self.active_window().in_navigation {
            return;
        }

        if let Event::MoveCursor {
            new_position,
            new_anchor,
            ..
        } = event
        {
            let buffer_id = self.active_buffer();
            self.active_window_mut().position_history.record_movement(
                buffer_id,
                *new_position,
                *new_anchor,
            );
        }
    }

    /// Route a key event through the CompositeInputRouter for a composite
    /// buffer.  Returns `Some(Ok(()))` if the event was handled (or blocked),
    /// `None` if the router returned `Unhandled` (let fallthrough continue).
    pub(super) fn try_route_composite_key(
        &mut self,
        split_id: crate::model::event::LeafId,
        buffer_id: crate::model::event::BufferId,
        key_event: &crossterm::event::KeyEvent,
    ) -> Option<AnyhowResult<()>> {
        use crate::input::composite_router::{
            CompositeInputRouter, Direction, RoutedEvent, ScrollAction,
        };

        let composite = self.active_window().composite_buffers.get(&buffer_id)?;
        let view_state = self
            .active_window()
            .composite_view_states
            .get(&(split_id, buffer_id))?;

        match CompositeInputRouter::route_key_event(composite, view_state, key_event) {
            RoutedEvent::Unhandled => None,

            RoutedEvent::CompositeScroll(action) => {
                let delta = match action {
                    ScrollAction::Up(n) => -(n as isize),
                    ScrollAction::Down(n) => n as isize,
                    _ => return Some(Ok(())),
                };
                self.active_window_mut()
                    .composite_scroll(split_id, buffer_id, delta);
                Some(Ok(()))
            }

            RoutedEvent::SwitchPane(dir) => {
                match dir {
                    Direction::Next => self
                        .active_window_mut()
                        .composite_focus_next(split_id, buffer_id),
                    Direction::Prev => self
                        .active_window_mut()
                        .composite_focus_prev(split_id, buffer_id),
                }
                Some(Ok(()))
            }

            // Anything else the router might return — let normal dispatch handle it
            _ => None,
        }
    }
}
