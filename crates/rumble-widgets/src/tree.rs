//! `Tree` — hierarchical room/user view with click, double-click,
//! right-click context, keyboard navigation, and drag-drop reparenting.
//!
//! `Tree` is a thin builder facade that collects args and dispatches to
//! the theme's `TreeImpl`. The default `show()` owns ~all of the
//! interaction surface: flattening visible rows, allocating geometry,
//! collecting clicks/keyboard/drag-drop, emitting events. Themes only
//! override the paint primitives — caret glyph, row chrome (selection /
//! hover), drop indicator. See `DefaultTree` for the shared baseline
//! (rotated chevron caret, theme-`selection()` highlight) and `LunaTree`
//! for an XP-authentic +/− expander box.
//!
//! ## Data ownership
//!
//! The caller owns the tree. Pass a borrowed `&[TreeNode]`; nothing in
//! this widget mutates it. State changes (expand/collapse, selection,
//! drop) come back as events in [`TreeResponse`]; the caller applies
//! them to its own model.
//!
//! ## Selection vs focus
//!
//! Selection is *what's highlighted in the tree* (the current row).
//! Focus is *whether the tree itself receives keyboard input*. The tree
//! requests focus on click and consumes arrow keys / Enter while
//! focused. Selection lives in caller state — pass it via
//! `.selected(...)`, watch `selection_changed` in the response.

use std::hash::Hash;

use eframe::egui::{
    self, Color32, CursorIcon, Id, Key, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2, emath::GuiRounding,
};

use crate::{
    presence::{UserPresence, UserState},
    theme::UiExt,
    tokens::TextRole,
};

pub type TreeNodeId = u64;

#[derive(Clone, Debug)]
pub enum TreeNodeKind {
    Channel { name: String },
    User { name: String, state: UserState },
}

#[derive(Clone, Debug)]
pub struct TreeNode {
    pub id: TreeNodeId,
    pub kind: TreeNodeKind,
    pub children: Vec<TreeNode>,
    /// Caller-owned. Only meaningful for channels.
    pub expanded: bool,
}

impl TreeNode {
    pub fn channel(id: TreeNodeId, name: impl Into<String>) -> Self {
        Self {
            id,
            kind: TreeNodeKind::Channel { name: name.into() },
            children: Vec::new(),
            expanded: true,
        }
    }

    pub fn user(id: TreeNodeId, name: impl Into<String>, state: UserState) -> Self {
        Self {
            id,
            kind: TreeNodeKind::User {
                name: name.into(),
                state,
            },
            children: Vec::new(),
            expanded: true,
        }
    }

    pub fn with_children(mut self, children: Vec<TreeNode>) -> Self {
        self.children = children;
        self
    }

    pub fn is_channel(&self) -> bool {
        matches!(self.kind, TreeNodeKind::Channel { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropPosition {
    /// Insert as a child of the target (target must be a channel).
    Into,
    /// Insert as a sibling, just above the target.
    Above,
    /// Insert as a sibling, just below the target.
    Below,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropEvent {
    pub source: TreeNodeId,
    pub target: TreeNodeId,
    pub position: DropPosition,
}

#[derive(Default, Debug)]
pub struct TreeResponse {
    pub response: Option<Response>,
    /// Single click on a row.
    pub clicked: Option<TreeNodeId>,
    /// Double click on a row (e.g. activate channel).
    pub double_clicked: Option<TreeNodeId>,
    /// Caret clicked, or keyboard Left/Right toggled visibility.
    pub toggled: Option<TreeNodeId>,
    /// Right-clicked at this screen position. Caller renders the menu.
    pub context: Option<(TreeNodeId, Pos2)>,
    /// Drag completed: source was dropped at `(target, position)`.
    pub dropped: Option<DropEvent>,
    /// Selection changed via keyboard. `Some(None)` means cleared.
    pub selection_changed: Option<Option<TreeNodeId>>,
    /// Keyboard activated current selection (Enter).
    pub activated: Option<TreeNodeId>,
}

impl std::ops::Deref for TreeResponse {
    type Target = Option<Response>;
    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

/// Snapshot of a row's interaction state, passed to `TreeImpl::paint_row_chrome`.
#[derive(Copy, Clone, Debug, Default)]
pub struct TreeRowState {
    pub selected: bool,
    pub hovered: bool,
}

/// Builder arguments, passed to `TreeImpl`.
pub struct TreeArgs<'a> {
    pub id_salt: Id,
    pub nodes: &'a [TreeNode],
    pub selected: Option<TreeNodeId>,
    pub drag_drop: bool,
    pub indent: f32,
    pub row_height: f32,
}

/// Theme-provided implementation of the Tree visual primitives.
///
/// Themes override the `paint_*` methods (and `caret_width` if their
/// expander glyph is wider than the default). The default `show()` owns
/// everything else — flattening, allocation, hit-testing, keyboard,
/// drag/drop, event emission — so themes never reimplement that surface.
pub trait TreeImpl: Send + Sync + 'static {
    /// Width reserved for the caret column (px). Themes whose expander
    /// glyph needs more horizontal room override this.
    fn caret_width(&self) -> f32 {
        14.0
    }

    /// Paint the row background (selection / hover). Default uses the
    /// theme's `selection()` shape and a faint text-tint for hover.
    fn paint_row_chrome(&self, ui: &mut Ui, rect: Rect, state: TreeRowState) {
        let theme = ui.theme();
        let tokens = theme.tokens();
        if state.selected {
            ui.painter().add(theme.selection(rect));
        } else if state.hovered {
            ui.painter().rect_filled(
                rect,
                0.0,
                Color32::from_rgba_unmultiplied(tokens.text.r(), tokens.text.g(), tokens.text.b(), 12),
            );
        }
    }

    /// Paint tree lines connecting a row to its ancestors / siblings,
    /// Windows-Explorer-style. Default is a no-op; Luna overrides with
    /// dotted grey lines.
    ///
    /// `row_left_x` is the row's left edge, `pad` is the indent padding
    /// before the first caret column, `indent` is the per-depth step.
    /// `ancestor_has_next[i]` says "does the ancestor at depth i+1 have
    /// more siblings below this row?" — controls full-height vs
    /// top-half vertical lines per column.
    #[allow(clippy::too_many_arguments)]
    fn paint_row_connectors(
        &self,
        _ui: &mut Ui,
        _rect: Rect,
        _row_left_x: f32,
        _depth: usize,
        _ancestor_has_next: &[bool],
        _indent: f32,
        _caret_w: f32,
        _pad: f32,
    ) {
    }

    /// Paint the expand/collapse caret glyph inside `rect`. `color` is
    /// the row's text color (so the caret reads at the same emphasis).
    fn paint_caret(&self, ui: &mut Ui, rect: Rect, expanded: bool, color: Color32);

    /// Paint the drop-target indicator for an in-flight drag.
    fn paint_drop_indicator(&self, ui: &mut Ui, rect: Rect, pos: DropPosition, accent: Color32) {
        let stroke = Stroke::new(2.0, accent);
        match pos {
            DropPosition::Above => {
                ui.painter().line_segment(
                    [Pos2::new(rect.left(), rect.top()), Pos2::new(rect.right(), rect.top())],
                    stroke,
                );
            }
            DropPosition::Below => {
                ui.painter().line_segment(
                    [
                        Pos2::new(rect.left(), rect.bottom()),
                        Pos2::new(rect.right(), rect.bottom()),
                    ],
                    stroke,
                );
            }
            DropPosition::Into => {
                ui.painter()
                    .rect_stroke(rect.shrink(1.0), 4.0, stroke, egui::StrokeKind::Inside);
            }
        }
    }

    /// Allocate, render rows, sense input, emit events. Almost no theme
    /// will want to override this; doing so loses the shared interaction
    /// model.
    fn show(&self, ui: &mut Ui, args: TreeArgs<'_>) -> TreeResponse {
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut out = TreeResponse::default();

        let mut visible: Vec<VisibleRow> = Vec::new();
        flatten(args.nodes, 0, Vec::new(), &mut visible);

        let mut row_rects: Vec<(TreeNodeId, Rect, bool /* is_channel */)> = Vec::with_capacity(visible.len());

        for row in &visible {
            let rect = self.render_row(ui, row, &args, &mut out);
            row_rects.push((row.node.id, rect, row.node.is_channel()));
        }

        let tree_rect = if let (Some(first), Some(last)) = (row_rects.first(), row_rects.last()) {
            Rect::from_min_max(first.1.min, last.1.max)
        } else {
            return out;
        };

        let tree_id = ui.make_persistent_id(args.id_salt);
        let tree_response = ui.interact(tree_rect, tree_id, Sense::focusable_noninteractive());

        if out.clicked.is_some() || out.double_clicked.is_some() {
            tree_response.request_focus();
        }

        // Keyboard handling fires when the tree itself has focus —
        // the obvious case. We *also* allow it when the caller is
        // supplying a selection AND no other widget claims focus, so
        // tests (which never click) can drive navigation purely from
        // keys. Without the second clause's focus guard, a focused
        // text input elsewhere on the page (e.g. the chat composer)
        // would have its Enter eaten by the tree.
        let other_focus = ui
            .ctx()
            .memory(|m| m.focused())
            .map(|id| id != tree_id)
            .unwrap_or(false);
        if tree_response.has_focus() || (args.selected.is_some() && !other_focus) {
            handle_keyboard(ui, args.selected, &row_rects, &mut out);
        }

        if args.drag_drop
            && let Some(drop) = take_drop(ui, &row_rects)
        {
            out.dropped = Some(drop);
        }

        out.response = Some(tree_response);
        out
    }

    /// Render a single row. Provided as a default so themes can override
    /// just the paint primitives without rebuilding the whole row layout
    /// (which is tightly coupled to the keyboard/drag-drop bookkeeping).
    fn render_row(&self, ui: &mut Ui, row: &VisibleRow<'_>, args: &TreeArgs, out: &mut TreeResponse) -> Rect {
        let theme = ui.theme();
        let tokens = theme.tokens();

        let avail = ui.available_width();
        let sense = if args.drag_drop {
            Sense::click_and_drag()
        } else {
            Sense::click()
        };
        let (rect, response) = ui.allocate_exact_size(Vec2::new(avail, args.row_height), sense);

        self.paint_row_chrome(
            ui,
            rect,
            TreeRowState {
                selected: args.selected == Some(row.node.id),
                hovered: response.hovered(),
            },
        );

        let pad = tokens.pad_sm;
        let caret_w = self.caret_width();
        self.paint_row_connectors(
            ui,
            rect,
            rect.left(),
            row.depth,
            &row.ancestor_has_next,
            args.indent,
            caret_w,
            pad,
        );
        let x_indent = rect.left() + (row.depth as f32) * args.indent + pad;
        let caret_rect = Rect::from_min_size(
            Pos2::new(x_indent, rect.top()),
            Vec2::new(caret_w, if row.node.is_channel() { args.row_height } else { 0.0 }),
        );

        if row.node.is_channel() {
            let caret_resp = ui.interact(caret_rect, ui.id().with(("tree_caret", row.node.id)), Sense::click());
            self.paint_caret(ui, caret_rect, row.node.expanded, tokens.text);
            if caret_resp.clicked() {
                out.toggled = Some(row.node.id);
            }
        }

        let content_x = x_indent + caret_w + pad;
        let content_rect = Rect::from_min_max(
            Pos2::new(content_x, rect.top()),
            Pos2::new(rect.right() - pad, rect.bottom()),
        );

        match &row.node.kind {
            TreeNodeKind::Channel { name } => {
                let font = theme.font(TextRole::Body);
                ui.put(
                    content_rect,
                    egui::Label::new(egui::RichText::new(name).font(font).color(tokens.text)).selectable(false),
                );
            }
            TreeNodeKind::User { name, state } => {
                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(content_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                UserPresence::new()
                    .name(name.clone())
                    .state(*state)
                    .size(20.0)
                    .show(&mut child);
            }
        }

        // Click / double / right interactions on the row body. The caret
        // already swallowed clicks within its rect; egui's interaction
        // layering means the row response still fires for the larger
        // rect, so we discriminate by pointer x.
        let pointer_in_caret = ui
            .input(|i| i.pointer.interact_pos())
            .map(|p| caret_rect.contains(p))
            .unwrap_or(false);

        if !pointer_in_caret {
            if response.double_clicked() {
                out.double_clicked = Some(row.node.id);
            } else if response.clicked() {
                out.clicked = Some(row.node.id);
            }
            if response.secondary_clicked()
                && let Some(p) = response.interact_pointer_pos()
            {
                out.context = Some((row.node.id, p));
            }
        }

        if args.drag_drop && response.drag_started() {
            egui::DragAndDrop::set_payload(ui.ctx(), row.node.id);
        }
        if args.drag_drop && egui::DragAndDrop::has_payload_of_type::<TreeNodeId>(ui.ctx()) {
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            if let Some(p) = ui.input(|i| i.pointer.hover_pos())
                && rect.contains(p)
            {
                let pos = drop_position_for(rect, p, row.node.is_channel());
                self.paint_drop_indicator(ui, rect, pos, tokens.accent);
            }
        }

        rect
    }
}

/// The static `TreeImpl` every theme gets by default — preserves the
/// pre-refactor look: rotated-triangle caret, theme-`selection()` row
/// highlight, accent-coloured drop indicators.
pub struct DefaultTree;
pub(crate) static DEFAULT_TREE: DefaultTree = DefaultTree;

impl TreeImpl for DefaultTree {
    fn paint_caret(&self, ui: &mut Ui, rect: Rect, expanded: bool, color: Color32) {
        // Snap so `convex_polygon` vertices (not tessellator-snapped)
        // land on pixels.
        let rect = rect.round_to_pixels(ui.ctx().pixels_per_point());
        let c = rect.center();
        let s = 4.0;
        let pts = if expanded {
            // Down-pointing.
            vec![
                Pos2::new(c.x - s, c.y - s * 0.5),
                Pos2::new(c.x + s, c.y - s * 0.5),
                Pos2::new(c.x, c.y + s * 0.5),
            ]
        } else {
            // Right-pointing.
            vec![
                Pos2::new(c.x - s * 0.5, c.y - s),
                Pos2::new(c.x - s * 0.5, c.y + s),
                Pos2::new(c.x + s * 0.5, c.y),
            ]
        };
        ui.painter().add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
    }
}

/// Text-labelled builder — what callers actually use.
pub struct Tree<'a> {
    id_salt: Id,
    nodes: &'a [TreeNode],
    selected: Option<TreeNodeId>,
    drag_drop: bool,
    indent: f32,
    row_height: f32,
}

impl<'a> Tree<'a> {
    pub fn new(id_salt: impl Hash, nodes: &'a [TreeNode]) -> Self {
        Self {
            id_salt: Id::new(id_salt),
            nodes,
            selected: None,
            drag_drop: false,
            indent: 16.0,
            row_height: 28.0,
        }
    }

    pub fn selected(mut self, id: Option<TreeNodeId>) -> Self {
        self.selected = id;
        self
    }

    pub fn drag_drop(mut self, enabled: bool) -> Self {
        self.drag_drop = enabled;
        self
    }

    pub fn indent(mut self, px: f32) -> Self {
        self.indent = px;
        self
    }

    pub fn row_height(mut self, px: f32) -> Self {
        self.row_height = px;
        self
    }

    pub fn show(self, ui: &mut Ui) -> TreeResponse {
        let theme = ui.theme();
        let args = TreeArgs {
            id_salt: self.id_salt,
            nodes: self.nodes,
            selected: self.selected,
            drag_drop: self.drag_drop,
            indent: self.indent,
            row_height: self.row_height,
        };
        theme.tree().show(ui, args)
    }
}

// -----------------------------------------------------------------------
// Shared internals (used by the default render_row; themes that override
// render_row may reuse them too).

pub struct VisibleRow<'a> {
    pub node: &'a TreeNode,
    pub depth: usize,
    /// For each ancestor depth `i` (0..depth), `true` if that ancestor
    /// has more siblings after it. Used to decide whether to draw a
    /// vertical tree-line at column `i` (full-height vs top-half).
    pub ancestor_has_next: Vec<bool>,
    /// `true` if this row is the last sibling at its own depth (within
    /// its parent's children). Equivalent to
    /// `!ancestor_has_next.last().copied().unwrap_or(true)` when
    /// `depth > 0`.
    pub is_last_sibling: bool,
}

/// Stable ordering used when flattening: users before channels, original
/// insertion order within each kind. Callers keep their backing data in
/// whatever order they like; the display is normalized here.
fn sorted_child_indices(nodes: &[TreeNode]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..nodes.len()).collect();
    // `sort_by_key` is stable since Rust 1.20 — ties preserve original order.
    idx.sort_by_key(|&i| nodes[i].is_channel());
    idx
}

fn flatten<'a>(nodes: &'a [TreeNode], depth: usize, ancestor_has_next: Vec<bool>, out: &mut Vec<VisibleRow<'a>>) {
    let order = sorted_child_indices(nodes);
    let n = order.len();
    for (pos, &i) in order.iter().enumerate() {
        let node = &nodes[i];
        let is_last = pos + 1 == n;
        out.push(VisibleRow {
            node,
            depth,
            ancestor_has_next: ancestor_has_next.clone(),
            is_last_sibling: is_last,
        });
        if node.expanded && !node.children.is_empty() {
            let mut child_mask = ancestor_has_next.clone();
            child_mask.push(!is_last);
            flatten(&node.children, depth + 1, child_mask, out);
        }
    }
}

fn drop_position_for(row_rect: Rect, pointer: Pos2, target_is_channel: bool) -> DropPosition {
    let h = row_rect.height();
    let y = pointer.y - row_rect.top();
    if target_is_channel {
        if y < h * 0.25 {
            DropPosition::Above
        } else if y > h * 0.75 {
            DropPosition::Below
        } else {
            DropPosition::Into
        }
    } else {
        if y < h * 0.5 {
            DropPosition::Above
        } else {
            DropPosition::Below
        }
    }
}

fn take_drop(ui: &mut Ui, row_rects: &[(TreeNodeId, Rect, bool)]) -> Option<DropEvent> {
    let released = ui.input(|i| i.pointer.any_released());
    if !released {
        return None;
    }
    let pointer = ui.input(|i| i.pointer.interact_pos())?;
    let payload = egui::DragAndDrop::take_payload::<TreeNodeId>(ui.ctx())?;

    let (target_id, target_rect, is_channel) = row_rects.iter().find(|(_, r, _)| r.contains(pointer))?;
    let position = drop_position_for(*target_rect, pointer, *is_channel);
    let source = *payload;

    if source == *target_id {
        return None;
    }
    Some(DropEvent {
        source,
        target: *target_id,
        position,
    })
}

fn handle_keyboard(
    ui: &mut Ui,
    selected: Option<TreeNodeId>,
    rows: &[(TreeNodeId, Rect, bool)],
    out: &mut TreeResponse,
) {
    if rows.is_empty() {
        return;
    }
    let current_idx = selected
        .and_then(|id| rows.iter().position(|(rid, _, _)| *rid == id))
        .unwrap_or(0);

    let consume_key = |ui: &Ui, key: Key| ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key));

    if consume_key(ui, Key::ArrowDown) {
        let next = (current_idx + 1).min(rows.len() - 1);
        if Some(rows[next].0) != selected {
            out.selection_changed = Some(Some(rows[next].0));
        }
    } else if consume_key(ui, Key::ArrowUp) {
        let next = current_idx.saturating_sub(1);
        if Some(rows[next].0) != selected {
            out.selection_changed = Some(Some(rows[next].0));
        }
    } else if consume_key(ui, Key::Home) {
        if Some(rows[0].0) != selected {
            out.selection_changed = Some(Some(rows[0].0));
        }
    } else if consume_key(ui, Key::End) {
        let last = rows[rows.len() - 1].0;
        if Some(last) != selected {
            out.selection_changed = Some(Some(last));
        }
    } else if consume_key(ui, Key::Enter) {
        if let Some(id) = selected {
            out.activated = Some(id);
        }
    } else if consume_key(ui, Key::ArrowLeft) {
        if let Some(id) = selected
            && rows.iter().any(|(rid, _, ic)| *rid == id && *ic)
        {
            out.toggled = Some(id);
        }
    } else if consume_key(ui, Key::ArrowRight)
        && let Some(id) = selected
        && rows.iter().any(|(rid, _, ic)| *rid == id && *ic)
    {
        out.toggled = Some(id);
    }
}

#[cfg(test)]
mod inner_tests {
    use super::*;
    use eframe::egui::pos2;

    fn rect(top: f32, h: f32) -> Rect {
        Rect::from_min_max(pos2(0.0, top), pos2(100.0, top + h))
    }

    #[test]
    fn drop_position_for_channel_thirds() {
        let r = rect(0.0, 20.0);
        assert_eq!(drop_position_for(r, pos2(50.0, 2.0), true), DropPosition::Above);
        assert_eq!(drop_position_for(r, pos2(50.0, 10.0), true), DropPosition::Into);
        assert_eq!(drop_position_for(r, pos2(50.0, 18.0), true), DropPosition::Below);
    }

    #[test]
    fn drop_position_for_user_halves() {
        let r = rect(0.0, 20.0);
        assert_eq!(drop_position_for(r, pos2(50.0, 5.0), false), DropPosition::Above);
        assert_eq!(drop_position_for(r, pos2(50.0, 15.0), false), DropPosition::Below);
    }

    #[test]
    fn flatten_skips_collapsed_subtrees() {
        let mut root = TreeNode::channel(1, "root");
        root.children = vec![
            TreeNode::channel(2, "a").with_children(vec![TreeNode::user(3, "alice", UserState::default())]),
            TreeNode::user(4, "bob", UserState::default()),
        ];
        let mut v = Vec::new();
        flatten(std::slice::from_ref(&root), 0, Vec::new(), &mut v);
        assert_eq!(v.len(), 4);

        if let Some(inner) = root.children.get_mut(0) {
            inner.expanded = false;
        }
        let mut v = Vec::new();
        flatten(std::slice::from_ref(&root), 0, Vec::new(), &mut v);
        assert_eq!(v.len(), 3);
        assert!(v.iter().all(|r| r.node.id != 3));
    }

    #[test]
    fn flatten_respects_root_collapse() {
        let root = TreeNode {
            id: 1,
            kind: TreeNodeKind::Channel { name: "root".into() },
            children: vec![TreeNode::user(2, "a", UserState::default())],
            expanded: false,
        };
        let mut v = Vec::new();
        flatten(std::slice::from_ref(&root), 0, Vec::new(), &mut v);
        assert_eq!(v.len(), 1, "only the root is visible when it's collapsed");
    }
}
