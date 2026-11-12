use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use collections::HashMap;
use gpui::{
    App, Bounds, Entity, EntityId, FocusHandle, HitboxId, Pixels, Point, SharedString, WeakEntity,
    Window,
};

use crate::{CopyAsMarkdown, Markdown, RenderedText, SelectMode, Selection};

/// The position of a member within its group: members are ordered by this
/// key when deciding which side of the drag anchor they are on and when
/// assembling the copied text.
pub type SelectionGroupOrder = (usize, usize);

/// Lets a mouse drag that starts in one `MarkdownElement` select text across
/// the other elements of the same group, and lets Copy take the whole span.
#[derive(Clone, Default)]
pub struct SelectionGroup(Rc<RefCell<SelectionGroupState>>);

#[derive(Default)]
struct SelectionGroupState {
    members: HashMap<EntityId, MarkdownMember>,
    text_members: BTreeMap<SelectionGroupOrder, SharedString>,
    drag_anchor: Option<(EntityId, WeakEntity<Markdown>)>,
    focus_target: Option<FocusHandle>,
    head_resolver: Option<HeadResolver>,
}

/// Returns the entry index under the pointer, clamped to the visible entries.
pub type HeadResolver = Rc<dyn Fn(&Window, &App) -> Option<usize>>;

struct MarkdownMember {
    order: SelectionGroupOrder,
    markdown: WeakEntity<Markdown>,
    hitbox_id: Option<HitboxId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemberSpan {
    Nothing,
    StartToPointer,
    PointerToEnd,
    Everything,
}

/// How much of a non-anchor member a group drag selects, decided only by the
/// pointer's vertical position against the member's bounds.
pub(crate) fn member_span(
    member_is_after_anchor: bool,
    pointer_y: Pixels,
    member_top: Pixels,
    member_bottom: Pixels,
) -> MemberSpan {
    if member_is_after_anchor {
        if pointer_y < member_top {
            MemberSpan::Nothing
        } else if pointer_y > member_bottom {
            MemberSpan::Everything
        } else {
            MemberSpan::StartToPointer
        }
    } else if pointer_y > member_bottom {
        MemberSpan::Nothing
    } else if pointer_y < member_top {
        MemberSpan::Everything
    } else {
        MemberSpan::PointerToEnd
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragEndResolution {
    Keep,
    SelectAll,
    Clear,
    ExtendToEnd,
    ExtendToStart,
}

/// What happens to the member at `order` when a drag that began in the
/// anchor ends with the pointer over entry `head_entry`. Members in the head
/// entry are painted and already follow the pointer, so they are kept.
pub(crate) fn resolve_at_drag_end(
    order: SelectionGroupOrder,
    anchor_order: SelectionGroupOrder,
    head_entry: usize,
) -> DragEndResolution {
    let anchor_entry = anchor_order.0;
    if order == anchor_order {
        return if head_entry > anchor_entry {
            DragEndResolution::ExtendToEnd
        } else if head_entry < anchor_entry {
            DragEndResolution::ExtendToStart
        } else {
            DragEndResolution::Keep
        };
    }
    if order.0 == head_entry {
        return DragEndResolution::Keep;
    }
    let between = if head_entry > anchor_entry {
        order > anchor_order && order.0 < head_entry
    } else if head_entry < anchor_entry {
        order < anchor_order && order.0 > head_entry
    } else {
        false
    };
    if between {
        DragEndResolution::SelectAll
    } else {
        DragEndResolution::Clear
    }
}

/// Applies `change` to a member, through `current` when it is the entity
/// whose listener is running, since updating that one by handle would panic.
fn with_member(
    current: &mut Option<(EntityId, &mut Markdown)>,
    entity_id: EntityId,
    markdown: &WeakEntity<Markdown>,
    cx: &mut App,
    change: impl FnOnce(&mut Markdown) -> bool,
) {
    match current.as_mut() {
        Some((current_id, current_markdown)) if *current_id == entity_id => {
            change(current_markdown);
        }
        _ => {
            if let Some(markdown) = markdown.upgrade() {
                markdown.update(cx, |markdown, cx| {
                    if change(markdown) {
                        cx.notify();
                    }
                });
            }
        }
    }
}

fn member_has_selection(
    current: &Option<(EntityId, &mut Markdown)>,
    entity_id: EntityId,
    markdown: &WeakEntity<Markdown>,
    cx: &App,
) -> bool {
    match current.as_ref() {
        Some((current_id, current_markdown)) if *current_id == entity_id => {
            current_markdown.has_selection()
        }
        _ => markdown
            .upgrade()
            .is_some_and(|markdown| markdown.read(cx).has_selection()),
    }
}

impl SelectionGroup {
    pub fn new() -> Self {
        Self::default()
    }

    /// Where focus goes when a drag ends with a selection beyond its anchor.
    /// The anchor may be scrolled out of view, and an unpainted element's
    /// focus handle receives no key actions, so the owner must keep this
    /// handle rendered and handle `Copy` and `CopyAsMarkdown` on it with the
    /// group's text.
    pub fn set_focus_target(&self, focus_handle: FocusHandle) {
        self.0.borrow_mut().focus_target = Some(focus_handle);
    }

    /// Lets the group find which entry (the first order component) the
    /// pointer is over when a drag ends. Only painted members follow the
    /// pointer during a drag, so without this a member that was selected while
    /// scrolled into view keeps its selection after the pointer moves back.
    pub fn set_head_resolver(&self, resolve_head: HeadResolver) {
        self.0.borrow_mut().head_resolver = Some(resolve_head);
    }

    /// Registers a markdown member whether or not it is painted, so a drag
    /// can span members that never scroll into view.
    pub fn register_markdown(&self, order: SelectionGroupOrder, markdown: &Entity<Markdown>) {
        let mut state = self.0.borrow_mut();
        let member = state
            .members
            .entry(markdown.entity_id())
            .or_insert_with(|| MarkdownMember {
                order,
                markdown: markdown.downgrade(),
                hitbox_id: None,
            });
        member.order = order;
    }

    /// Registers content that is not a `MarkdownElement` (such as the user's
    /// own messages) so it is copied when it lies inside a selected span.
    pub fn register_text(&self, order: SelectionGroupOrder, text: SharedString) {
        self.0.borrow_mut().text_members.insert(order, text);
    }

    /// Forgets every member whose first order component is at least
    /// `first_entry`, clearing their selections first so none is left behind
    /// in an element the group can no longer reach.
    pub fn remove_entries_from(&self, first_entry: usize, cx: &mut App) {
        let removed = {
            let mut state = self.0.borrow_mut();
            state.text_members.retain(|order, _| order.0 < first_entry);
            let mut removed = Vec::new();
            state.members.retain(|_, member| {
                if member.order.0 >= first_entry {
                    removed.push(member.markdown.clone());
                    false
                } else {
                    true
                }
            });
            removed
        };
        for markdown in removed {
            clear_selection(&markdown, cx);
        }
    }

    /// The text of the whole selected span, or `None` when nothing is selected.
    pub fn selected_text(&self, cx: &App) -> Option<String> {
        self.selected_text_beyond(None, cx)
    }

    /// Whether more than one markdown member has a selection.
    pub fn spans_multiple_members(&self, cx: &App) -> bool {
        let state = self.0.borrow();
        state
            .members
            .values()
            .filter(|member| {
                member
                    .markdown
                    .upgrade()
                    .is_some_and(|markdown| markdown.read(cx).has_selection())
            })
            .take(2)
            .count()
            > 1
    }

    /// For a left mouse down: clears the selection of every member except
    /// `current_id` and those under the pointer. Members that are not painted
    /// get no mouse events, so this is the only way their selection is cleared.
    pub fn clear_unhovered(&self, current_id: Option<EntityId>, window: &Window, cx: &mut App) {
        let to_clear = {
            let mut state = self.0.borrow_mut();
            state.drag_anchor = None;
            state
                .members
                .retain(|_, member| member.markdown.upgrade().is_some());
            state
                .members
                .iter()
                .filter(|(entity_id, member)| {
                    Some(**entity_id) != current_id
                        && !member
                            .hitbox_id
                            .is_some_and(|hitbox_id| hitbox_id.is_hovered(window))
                })
                .map(|(_, member)| member.markdown.clone())
                .collect::<Vec<_>>()
        };
        for markdown in to_clear {
            clear_selection(&markdown, cx);
        }
    }

    /// Ends the drag, if one is active. The anchor may not be painted when the
    /// mouse is released, so its pending flag is cleared here or it would
    /// resume selecting the next time it is painted. Members between the
    /// anchor and the furthest selected member are then selected entirely,
    /// since a fast scroll can skip painting them. `current` is the entity
    /// whose listener is running: it is changed through the given reference
    /// because updating or reading it through its handle would panic.
    pub fn end_drag(
        &self,
        mut current: Option<(EntityId, &mut Markdown)>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some((anchor_id, anchor)) = self.0.borrow_mut().drag_anchor.take() else {
            return;
        };

        match current.as_mut() {
            Some((current_id, markdown)) if *current_id == anchor_id => {
                markdown.selection.pending = false;
            }
            _ => {
                if let Some(anchor) = anchor.upgrade() {
                    anchor.update(cx, |markdown, cx| {
                        if markdown.selection.pending {
                            markdown.selection.pending = false;
                            cx.notify();
                        }
                    });
                }
            }
        }

        let (members, focus_target, head_resolver) = {
            let state = self.0.borrow();
            let mut members = state
                .members
                .iter()
                .map(|(entity_id, member)| (member.order, *entity_id, member.markdown.clone()))
                .collect::<Vec<_>>();
            members.sort_by_key(|(order, _, _)| *order);
            (
                members,
                state.focus_target.clone(),
                state.head_resolver.clone(),
            )
        };
        let Some(anchor_order) = members
            .iter()
            .find(|(_, entity_id, _)| *entity_id == anchor_id)
            .map(|(order, _, _)| *order)
        else {
            return;
        };

        let head_entry = head_resolver.and_then(|resolve_head| resolve_head(window, cx));
        if let Some(head_entry) = head_entry {
            for (order, entity_id, markdown) in &members {
                let resolution = resolve_at_drag_end(*order, anchor_order, head_entry);
                with_member(
                    &mut current,
                    *entity_id,
                    markdown,
                    cx,
                    |markdown| match resolution {
                        DragEndResolution::Keep => false,
                        DragEndResolution::SelectAll => markdown.select_entire_source(),
                        DragEndResolution::Clear => markdown.clear_selection(),
                        DragEndResolution::ExtendToEnd => markdown.extend_selection_to_edge(true),
                        DragEndResolution::ExtendToStart => {
                            markdown.extend_selection_to_edge(false)
                        }
                    },
                );
            }
        } else {
            let selected_orders = members
                .iter()
                .filter(|(_, entity_id, markdown)| {
                    member_has_selection(&current, *entity_id, markdown, cx)
                })
                .map(|(order, _, _)| *order)
                .collect::<Vec<_>>();
            let first_order = selected_orders.first().copied().unwrap_or(anchor_order);
            let last_order = selected_orders.last().copied().unwrap_or(anchor_order);
            for (order, entity_id, markdown) in &members {
                let strictly_inside = (*order > anchor_order && *order < last_order)
                    || (*order < anchor_order && *order > first_order);
                if strictly_inside {
                    with_member(&mut current, *entity_id, markdown, cx, |markdown| {
                        markdown.select_entire_source()
                    });
                }
            }
        }

        let selection_beyond_anchor = members.iter().any(|(_, entity_id, markdown)| {
            *entity_id != anchor_id && member_has_selection(&current, *entity_id, markdown, cx)
        });
        // An owner that is itself not rendered (such as a thread shown inside
        // another) can't take the copy, so focus stays where it is.
        if selection_beyond_anchor
            && let Some(focus_target) = focus_target
            && window.is_action_available_in(&CopyAsMarkdown, &focus_target)
        {
            window.focus(&focus_target, cx);
        }
    }

    pub(crate) fn register_member(
        &self,
        entity_id: EntityId,
        order: SelectionGroupOrder,
        markdown: WeakEntity<Markdown>,
        hitbox_id: Option<HitboxId>,
    ) {
        self.0.borrow_mut().members.insert(
            entity_id,
            MarkdownMember {
                order,
                markdown,
                hitbox_id,
            },
        );
    }

    pub(crate) fn is_dragging(&self) -> bool {
        self.0.borrow().drag_anchor.is_some()
    }

    pub(crate) fn is_anchor(&self, entity_id: EntityId) -> bool {
        self.0
            .borrow()
            .drag_anchor
            .as_ref()
            .is_some_and(|(anchor_id, _)| *anchor_id == entity_id)
    }

    pub(crate) fn begin_drag(&self, anchor_id: EntityId, anchor: WeakEntity<Markdown>) {
        self.0.borrow_mut().drag_anchor = Some((anchor_id, anchor));
    }

    /// While a drag is active, whether `member_id` comes after the anchor.
    /// `None` when there is no drag, when `member_id` is the anchor, or when
    /// either is not registered.
    pub(crate) fn member_is_after_anchor(&self, member_id: EntityId) -> Option<bool> {
        let state = self.0.borrow();
        let (anchor_id, _) = state.drag_anchor.as_ref()?;
        if *anchor_id == member_id {
            return None;
        }
        let anchor_order = state.members.get(anchor_id)?.order;
        let member_order = state.members.get(&member_id)?.order;
        Some(member_order > anchor_order)
    }

    /// The text to copy for the whole group, or `None` when nothing is
    /// selected or the selection lies entirely within `focused_id` (which then
    /// copies as it always has).
    pub(crate) fn selected_text_beyond(
        &self,
        focused_id: Option<EntityId>,
        cx: &App,
    ) -> Option<String> {
        let state = self.0.borrow();
        let mut selected = state
            .members
            .iter()
            .filter_map(|(entity_id, member)| {
                let text = member.markdown.upgrade()?.read(cx).selected_text()?;
                Some((member.order, *entity_id, text))
            })
            .collect::<Vec<_>>();
        if selected.is_empty()
            || (selected.len() == 1
                && selected
                    .first()
                    .is_some_and(|(_, entity_id, _)| Some(*entity_id) == focused_id))
        {
            return None;
        }
        selected.sort_by_key(|(order, _, _)| *order);

        let first_order = selected.first()?.0;
        let last_order = selected.last()?.0;
        let mut pieces = selected
            .into_iter()
            .map(|(order, _, text)| (order, text))
            .collect::<Vec<_>>();
        pieces.extend(
            state
                .text_members
                .iter()
                .filter(|(order, _)| **order > first_order && **order < last_order)
                .map(|(order, text)| (*order, text.to_string())),
        );
        pieces.sort_by_key(|(order, _)| *order);
        Some(
            pieces
                .into_iter()
                .map(|(_, text)| text)
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    }
}

fn clear_selection(markdown: &WeakEntity<Markdown>, cx: &mut App) {
    if let Some(markdown) = markdown.upgrade() {
        markdown.update(cx, |markdown, cx| {
            if markdown.clear_selection() {
                cx.notify();
            }
        });
    }
}

impl Markdown {
    fn has_selection(&self) -> bool {
        self.selection.end > self.selection.start
    }

    /// Returns whether there was a selection to clear.
    fn clear_selection(&mut self) -> bool {
        if !self.selection.pending && !self.has_selection() {
            return false;
        }
        self.selection = Selection::default();
        true
    }

    /// Moves the selection's head to the end (or start) of the source,
    /// keeping its tail, for an anchor whose drag ended in another entry.
    fn extend_selection_to_edge(&mut self, toward_end: bool) -> bool {
        let tail = self.selection.tail();
        let (start, end, reversed) = if toward_end {
            (tail, self.source.len(), false)
        } else {
            (0, tail, true)
        };
        if !self.selection.pending
            && self.selection.start == start
            && self.selection.end == end
            && self.selection.reversed == reversed
        {
            return false;
        }
        self.selection = Selection {
            start,
            end,
            reversed,
            pending: false,
            mode: SelectMode::Character,
        };
        true
    }

    /// Selects all of this member's source. Returns whether that changed it.
    fn select_entire_source(&mut self) -> bool {
        let end = self.source.len();
        if !self.selection.pending && self.selection.start == 0 && self.selection.end == end {
            return false;
        }
        self.selection = Selection {
            start: 0,
            end,
            reversed: false,
            pending: false,
            mode: SelectMode::Character,
        };
        true
    }

    /// Selects the part of this non-anchor member that a group drag covers
    /// with the pointer at `pointer`. Returns whether the selection changed,
    /// so callers only notify when it did.
    pub(crate) fn select_for_group_drag(
        &mut self,
        member_is_after_anchor: bool,
        pointer: Point<Pixels>,
        bounds: Bounds<Pixels>,
        rendered_text: &RenderedText,
    ) -> bool {
        let span = member_span(
            member_is_after_anchor,
            pointer.y,
            bounds.top(),
            bounds.bottom(),
        );
        let pointer_index = match rendered_text.source_index_for_position(pointer) {
            Ok(ix) | Err(ix) => ix,
        };
        let source_end = rendered_text
            .lines
            .last()
            .map(|line| line.source_end)
            .unwrap_or(0);
        let (start, end, reversed) = match span {
            MemberSpan::Nothing => (0, 0, false),
            MemberSpan::StartToPointer => (0, pointer_index, false),
            MemberSpan::PointerToEnd => (pointer_index, source_end, true),
            MemberSpan::Everything => (0, source_end, false),
        };
        if !self.selection.pending
            && self.selection.start == start
            && self.selection.end == end
            && self.selection.reversed == reversed
        {
            return false;
        }
        self.selection = Selection {
            start,
            end,
            reversed,
            pending: false,
            mode: SelectMode::Character,
        };
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Entity, InteractiveElement as _, TestAppContext, px};

    #[test]
    fn test_member_span_follows_pointer_relative_to_anchor() {
        let top = px(100.);
        let bottom = px(200.);

        // A member below the anchor fills from its start as the pointer moves down into it.
        assert_eq!(member_span(true, px(50.), top, bottom), MemberSpan::Nothing);
        assert_eq!(
            member_span(true, px(150.), top, bottom),
            MemberSpan::StartToPointer
        );
        assert_eq!(
            member_span(true, px(250.), top, bottom),
            MemberSpan::Everything
        );

        // A member above the anchor fills from its end as the pointer moves up into it.
        assert_eq!(
            member_span(false, px(250.), top, bottom),
            MemberSpan::Nothing
        );
        assert_eq!(
            member_span(false, px(150.), top, bottom),
            MemberSpan::PointerToEnd
        );
        assert_eq!(
            member_span(false, px(50.), top, bottom),
            MemberSpan::Everything
        );
    }

    fn new_member(
        group: &SelectionGroup,
        source: &str,
        order: SelectionGroupOrder,
        selection: std::ops::Range<usize>,
        cx: &mut TestAppContext,
    ) -> Entity<Markdown> {
        let markdown = cx.new(|cx| Markdown::new(source.to_string().into(), None, None, cx));
        markdown.update(cx, |markdown, _| {
            markdown.selection = Selection {
                start: selection.start,
                end: selection.end,
                ..Selection::default()
            };
        });
        group.register_member(markdown.entity_id(), order, markdown.downgrade(), None);
        markdown
    }

    #[gpui::test]
    fn test_group_copy_joins_members_in_order_with_text_between(cx: &mut TestAppContext) {
        let group = SelectionGroup::new();
        // Registered out of order, to prove the copy follows the chat order.
        let last = new_member(&group, "third message", (4, 0), 0..5, cx);
        let first = new_member(&group, "first message", (0, 1), 6..13, cx);
        let middle = new_member(&group, "second message", (2, 0), 0..14, cx);
        group.register_text((1, 0), "user question".into());
        group.register_text((3, 0), "user follow-up".into());
        group.register_text((5, 0), "outside the span".into());

        let text = cx.read(|cx| group.selected_text_beyond(Some(first.entity_id()), cx));
        assert_eq!(
            text.as_deref(),
            Some("message\n\nuser question\n\nsecond message\n\nuser follow-up\n\nthird")
        );

        // A selection inside the focused member alone copies as it did before.
        middle.update(cx, |markdown, _| markdown.selection = Selection::default());
        last.update(cx, |markdown, _| markdown.selection = Selection::default());
        assert_eq!(
            cx.read(|cx| group.selected_text_beyond(Some(first.entity_id()), cx)),
            None
        );
    }

    struct FocusTargetView(FocusHandle);

    impl gpui::Render for FocusTargetView {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div()
                .track_focus(&self.0)
                .on_action(|_: &CopyAsMarkdown, _, _| {})
        }
    }

    #[gpui::test]
    fn test_end_drag_fills_skipped_members_and_focuses_target(cx: &mut TestAppContext) {
        let group = SelectionGroup::new();
        let before = new_member(&group, "before", (0, 0), 0..0, cx);
        let anchor = new_member(&group, "anchor text", (1, 0), 7..11, cx);
        let skipped = new_member(&group, "skipped", (2, 0), 0..0, cx);
        let furthest = new_member(&group, "partial end", (3, 0), 0..7, cx);
        let after = new_member(&group, "after", (4, 0), 0..0, cx);
        anchor.update(cx, |markdown, _| markdown.selection.pending = true);
        group.begin_drag(anchor.entity_id(), anchor.downgrade());

        let (view, cx) = cx.add_window_view(|_, cx| FocusTargetView(cx.focus_handle()));
        let focus_target = view.read_with(cx, |view, _| view.0.clone());
        group.set_focus_target(focus_target.clone());
        cx.run_until_parked();

        // Released with no member painted, as after a long wheel scroll.
        cx.update(|window, cx| group.end_drag(None, window, cx));

        assert!(!group.is_dragging());
        assert!(!anchor.read_with(cx, |markdown, _| markdown.selection.pending));
        let selected = |markdown: &Entity<Markdown>, cx: &mut gpui::VisualTestContext| {
            markdown.read_with(cx, |markdown, _| markdown.selected_text())
        };
        assert_eq!(selected(&skipped, cx).as_deref(), Some("skipped"));
        assert_eq!(selected(&furthest, cx).as_deref(), Some("partial"));
        assert_eq!(selected(&before, cx), None);
        assert_eq!(selected(&after, cx), None);
        assert!(cx.update(|window, _| focus_target.is_focused(window)));

        // Copying no longer depends on the anchor, even once it is gone.
        drop(anchor);
        cx.run_until_parked();
        assert_eq!(
            cx.read(|cx| group.selected_text(cx)).as_deref(),
            Some("skipped\n\npartial")
        );
    }

    #[test]
    fn test_resolve_at_drag_end_follows_head_entry_in_both_directions() {
        use DragEndResolution::*;
        let anchor = (2, 1);

        // Downward to entry 4: later chunks of the anchor's entry and entry 3
        // are filled, entry 4 follows the pointer, everything else is cleared.
        assert_eq!(resolve_at_drag_end(anchor, anchor, 4), ExtendToEnd);
        assert_eq!(resolve_at_drag_end((2, 2), anchor, 4), SelectAll);
        assert_eq!(resolve_at_drag_end((3, 0), anchor, 4), SelectAll);
        assert_eq!(resolve_at_drag_end((4, 0), anchor, 4), Keep);
        assert_eq!(resolve_at_drag_end((5, 0), anchor, 4), Clear);
        assert_eq!(resolve_at_drag_end((2, 0), anchor, 4), Clear);
        assert_eq!(resolve_at_drag_end((1, 0), anchor, 4), Clear);

        // Upward to entry 0 mirrors it.
        assert_eq!(resolve_at_drag_end(anchor, anchor, 0), ExtendToStart);
        assert_eq!(resolve_at_drag_end((2, 0), anchor, 0), SelectAll);
        assert_eq!(resolve_at_drag_end((1, 0), anchor, 0), SelectAll);
        assert_eq!(resolve_at_drag_end((0, 0), anchor, 0), Keep);
        assert_eq!(resolve_at_drag_end((2, 2), anchor, 0), Clear);
        assert_eq!(resolve_at_drag_end((3, 0), anchor, 0), Clear);

        // Ending in the anchor's own entry clears every other entry.
        assert_eq!(resolve_at_drag_end(anchor, anchor, 2), Keep);
        assert_eq!(resolve_at_drag_end((2, 2), anchor, 2), Keep);
        assert_eq!(resolve_at_drag_end((3, 0), anchor, 2), Clear);
    }

    #[gpui::test]
    fn test_end_drag_clears_members_selected_while_scrolled_past(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let selected = |markdown: &Entity<Markdown>, cx: &mut gpui::VisualTestContext| {
            markdown.read_with(cx, |markdown, _| markdown.selected_text())
        };

        // Dragged down from entry 1, scrolled on to entry 4 and back, released over entry 3.
        let group = SelectionGroup::new();
        let before = new_member(&group, "before", (0, 0), 0..6, cx);
        let anchor = new_member(&group, "anchor text", (1, 0), 7..9, cx);
        let skipped = new_member(&group, "skipped", (2, 0), 0..0, cx);
        let head = new_member(&group, "head part", (3, 0), 0..4, cx);
        let scrolled_past = new_member(&group, "scrolled past", (4, 0), 0..13, cx);
        group.begin_drag(anchor.entity_id(), anchor.downgrade());
        group.set_head_resolver(Rc::new(|_, _| Some(3)));
        cx.update(|window, cx| group.end_drag(None, window, cx));

        assert_eq!(selected(&anchor, cx).as_deref(), Some("text"));
        assert_eq!(selected(&skipped, cx).as_deref(), Some("skipped"));
        assert_eq!(selected(&head, cx).as_deref(), Some("head"));
        assert_eq!(selected(&scrolled_past, cx), None);
        assert_eq!(selected(&before, cx), None);

        // The same upward: from entry 3, released over entry 1.
        let group = SelectionGroup::new();
        let scrolled_past = new_member(&group, "scrolled past", (0, 0), 0..13, cx);
        let head = new_member(&group, "head part", (1, 0), 5..9, cx);
        let skipped = new_member(&group, "skipped", (2, 0), 0..0, cx);
        let anchor = new_member(&group, "anchor text", (3, 0), 0..6, cx);
        let after = new_member(&group, "after", (4, 0), 0..5, cx);
        anchor.update(cx, |markdown, _| markdown.selection.reversed = true);
        group.begin_drag(anchor.entity_id(), anchor.downgrade());
        group.set_head_resolver(Rc::new(|_, _| Some(1)));
        cx.update(|window, cx| group.end_drag(None, window, cx));

        assert_eq!(selected(&anchor, cx).as_deref(), Some("anchor"));
        assert_eq!(selected(&skipped, cx).as_deref(), Some("skipped"));
        assert_eq!(selected(&head, cx).as_deref(), Some("part"));
        assert_eq!(selected(&scrolled_past, cx), None);
        assert_eq!(selected(&after, cx), None);
    }
}
