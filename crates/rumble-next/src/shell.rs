//! Shell = the pieces of the Rumble UI that are the same across every
//! paradigm (tree, chat, composer, self-state toggles). Paradigm-specific
//! chrome (top bar / toolbar / statusbar) is layered on top in
//! `paradigm::*`.
//!
//! `Shell` now reads from a live `State` snapshot and issues `Command`s
//! via a `UiBackend`. Per-frame flow: the app clones `State` once,
//! converts it via `crate::adapters`, and hands `(state, tree_nodes)` to
//! the shell's render functions.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::backend::UiBackend;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Pos2, RichText, ScrollArea, Sense, Stroke,
    StrokeKind, Ui, Vec2, epaint::RectShape,
};
use rumble_protocol::{Command, State, permissions::Permissions, proto::RoomAclEntry};
use rumble_widgets::{
    ButtonArgs, ComboBox, GroupBox, PressableRole, SurfaceFrame, SurfaceKind, TextInput, Tree, TreeNode, TreeNodeId,
    TreeNodeKind, UiExt,
};
use uuid::Uuid;

/// Ban-modal duration choices, matching `rumble-egui`'s set so the two
/// clients give bans the same shape. Index → `(label, seconds)`; 0
/// seconds means a permanent ban (the protocol's sentinel).
const BAN_DURATIONS: &[(&str, u64)] = &[
    ("Permanent", 0),
    ("1 hour", 3600),
    ("1 day", 86_400),
    ("1 week", 604_800),
    ("30 days", 2_592_000),
];

use crate::{
    adapters::{self, NodeRef, crumbs_for_room, is_room},
    data::{ChatEntry, ChatMsg, Media, SysMsg, SysTone},
};

/// What the right-click context menu is attached to.
#[derive(Clone, Debug)]
pub enum ContextTarget {
    User {
        id: u64,
        name: String,
        locally_muted: bool,
        server_muted: bool,
    },
    Room {
        id: Uuid,
        name: String,
    },
}

#[derive(Clone, Debug)]
pub struct TreeContext {
    target: ContextTarget,
    pos: Pos2,
}

/// A modal that needs a text input before dispatching. Only one can be
/// open at a time — opening a new one closes the previous.
#[derive(Clone, Debug)]
pub enum PendingModal {
    CreateRoom {
        parent: Option<Uuid>,
        parent_name: String,
        name: String,
    },
    RenameRoom {
        id: Uuid,
        original: String,
        name: String,
    },
    DeleteRoom {
        id: Uuid,
        name: String,
    },
    EditDescription {
        id: Uuid,
        name: String,
        description: String,
    },
    Kick {
        user_id: u64,
        username: String,
        reason: String,
    },
    Ban {
        user_id: u64,
        username: String,
        reason: String,
        /// Index into `BAN_DURATIONS`. Defaults to 0 (Permanent).
        duration_idx: usize,
    },
    DirectMessage {
        user_id: u64,
        username: String,
        text: String,
    },
    /// Sudo elevate to superuser. Password-only — server signals
    /// success by flipping `is_elevated` on the user's `User`.
    Elevate {
        password: String,
    },
    /// Per-room ACL editor. Edits a working copy of `inherit_acl` and
    /// the entry list; on submit dispatches `Command::SetRoomAcl`.
    EditAcls {
        room_id: Uuid,
        room_name: String,
        inherit_acl: bool,
        entries: Vec<RoomAclEntry>,
        /// Index → group name for the per-entry group dropdown. Source
        /// of truth is `state.group_definitions` snapshotted when the
        /// modal opens; we cache it so the dropdown labels don't shift
        /// while the user is editing.
        group_options: Vec<String>,
    },
    /// Prompt for an incoming `FileOffer` that didn't match any
    /// auto-download rule. Accept dispatches `Command::DownloadFile`;
    /// Deny just closes the prompt (and marks the offer as handled
    /// so a history replay doesn't re-prompt).
    IncomingFileOffer {
        transfer_id: String,
        name: String,
        size: u64,
        mime: String,
        share_data: String,
        from: String,
    },
}

/// Pending incoming-file prompt enqueued by `App::pump_auto_downloads`
/// when an offer didn't match auto-download rules. Drained one-at-a-time
/// into `Shell::modal` so the user sees them serially.
#[derive(Clone, Debug)]
pub struct OfferPrompt {
    pub transfer_id: String,
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub share_data: String,
    pub from: String,
}

/// UI-local state that isn't part of the backend's `State` — caret
/// positions, composer buffer, expanded/collapsed channel set, etc.
#[derive(Default)]
pub struct Shell {
    /// Persistent expansion state per channel `TreeNodeId`. Rebuilt
    /// tree copies start expanded; this map flips them. Present so the
    /// tree doesn't snap back when `build_tree` re-runs each frame.
    expanded: HashMap<TreeNodeId, bool>,
    /// Selected row — used for highlight. Falls back to `my_room_id`
    /// if the user hasn't clicked anything.
    pub selected: Option<TreeNodeId>,
    pub composer: String,
    /// Right-click menu anchored at this position. Cleared when the
    /// user picks something or clicks away.
    context_menu: Option<TreeContext>,
    /// Modal awaiting text input. Only one at a time.
    modal: Option<PendingModal>,
    /// Settings panel visibility (rendered as an overlay).
    pub settings_open: bool,
    /// Active chat timestamp format. `None` = hide timestamps. Set by
    /// `App::update` from `settings.chat` each frame, so toggling the
    /// preference reflects without restarting.
    pub chat_timestamp_format: Option<rumble_desktop_shell::TimestampFormat>,
    /// Modern paradigm tree filter. Empty means show the full tree.
    pub tree_filter: String,
    /// Set when the user clicks the composer's "share file" button.
    /// `App::update` consumes the flag at the end of the frame and
    /// spawns the OS file picker on its tokio runtime — the composer
    /// itself doesn't have access to a runtime handle.
    pub share_file_requested: bool,
    /// Transfer IDs the user has clicked "Download" on. Used to flip
    /// the file-offer card's button to a passive "Downloading…" label
    /// once accepted, even before the transfer plugin has produced a
    /// status entry for the new download.
    accepted_offers: HashSet<String>,
    /// Visibility of the active transfers window. Toggled from the
    /// composer's "transfers" button or auto-shown when the user
    /// shares a file (so they immediately see progress).
    pub transfers_open: bool,
    /// Set when the user clicks "Open" or "Show in folder" on a
    /// completed transfer. `App::update` drains the queue and spawns
    /// the OS handler — `open::that` is blocking, so it runs off the
    /// main thread.
    pub pending_open: Vec<PendingOpen>,
    /// Queue of incoming-file offers awaiting a manual accept/deny
    /// decision. The shell pops one at a time into `modal` so users
    /// see prompts serially rather than as a stack of dialogs.
    queued_offer_prompts: VecDeque<OfferPrompt>,
    /// Open image lightbox, rendered as a separate overlay (not via
    /// `PendingModal` since it has its own dismissal flow and doesn't
    /// dispatch a `Command` on close).
    image_lightbox: Option<ImageLightbox>,
    /// Rect (in screen coords) of the most recently drawn inline image
    /// preview. Read by integration tests in `tests/` to dispatch a
    /// synthetic click at the right coordinates. Always populated (no
    /// feature gate) so the field works the same in dev builds where
    /// you might want to manually inspect it via `RUST_LOG`.
    pub last_image_preview_rect: Option<egui::Rect>,
}

/// State for the click-to-enlarge image viewer. The image is loaded
/// from `path` via egui_extras' file:// loader; the URI is cached so
/// we evict the texture from egui's image cache on close.
#[derive(Clone, Debug)]
pub struct ImageLightbox {
    pub path: std::path::PathBuf,
    pub name: String,
    pub uri: String,
}

/// What the user wants to do with a completed transfer's local file.
#[derive(Clone, Debug)]
pub enum PendingOpen {
    /// Open the file with the platform's default handler.
    File(std::path::PathBuf),
    /// Reveal the file in the platform's file manager.
    InFolder(std::path::PathBuf),
    /// Prompt for a destination and copy the file there. The second
    /// field is the suggested filename for the save dialog.
    SaveAs {
        src: std::path::PathBuf,
        suggested_name: String,
    },
}

impl Shell {
    /// Open the "create room" modal for the given parent (None = root).
    /// Used by paradigm toolbars / sidebars whose "Add channel" button
    /// doesn't have a specific parent in mind. Replaces any modal that
    /// was already open.
    pub fn open_create_room(&mut self, parent: Option<Uuid>, parent_name: impl Into<String>) {
        self.modal = Some(PendingModal::CreateRoom {
            parent,
            parent_name: parent_name.into(),
            name: String::new(),
        });
    }

    /// Open the sudo elevate modal. Triggered from the Settings panel's
    /// Connection page when the connected user isn't already elevated.
    pub fn open_elevate_modal(&mut self) {
        self.modal = Some(PendingModal::Elevate {
            password: String::new(),
        });
    }

    /// Record that a file offer has been accepted (manually or via
    /// auto-download), so the file-offer card flips to "Downloading…"
    /// instead of showing a Download button.
    pub fn mark_offer_accepted(&mut self, transfer_id: impl Into<String>) {
        self.accepted_offers.insert(transfer_id.into());
    }

    /// Enqueue an incoming-file offer for a manual accept/deny prompt.
    /// Called by `App::pump_auto_downloads` when an offer doesn't
    /// match any auto-download rule.
    pub fn queue_offer_prompt(&mut self, prompt: OfferPrompt) {
        self.queued_offer_prompts.push_back(prompt);
    }

    /// Pop the next queued offer into `modal` if no other modal is
    /// already open. Called once per frame by `render_overlays`.
    fn drain_offer_queue(&mut self) {
        if self.modal.is_some() {
            return;
        }
        if let Some(p) = self.queued_offer_prompts.pop_front() {
            self.modal = Some(PendingModal::IncomingFileOffer {
                transfer_id: p.transfer_id,
                name: p.name,
                size: p.size,
                mime: p.mime,
                share_data: p.share_data,
                from: p.from,
            });
        }
    }

    /// Test-only hook to open the lightbox without going through the
    /// chat preview's click handler. Used by the kittest snapshot in
    /// `tests/chat_image_preview.rs`.
    #[cfg(feature = "test-harness")]
    pub fn open_image_lightbox_for_test(&mut self, path: std::path::PathBuf, name: String) {
        let uri = format!("file://{}", path.display());
        self.image_lightbox = Some(ImageLightbox { path, name, uri });
    }

    /// Test-only accessor: returns the active lightbox (if any) so tests
    /// can assert that a click on the inline preview opened it.
    #[cfg(feature = "test-harness")]
    pub fn image_lightbox_for_test(&self) -> Option<&ImageLightbox> {
        self.image_lightbox.as_ref()
    }
}

// ---------- Tree pane ----------

impl Shell {
    /// Render the nested channel/user tree. Emits `Command::JoinRoom`
    /// when a channel is double-clicked or activated via Enter.
    pub fn tree_pane<B: UiBackend>(&mut self, ui: &mut Ui, state: &State, backend: &B) {
        let (mut tree, id_map) = adapters::build_tree(state);
        if !self.tree_filter.trim().is_empty() {
            filter_tree(&mut tree, self.tree_filter.trim());
        }

        // Apply our local expansion overrides. (Live tree nodes start
        // expanded; the user's preference overrides that.)
        apply_expanded(&mut tree, &self.expanded);

        // Default selection = our current room, if user hasn't picked.
        let selected = self.selected.or_else(|| state.my_room_id.map(adapters::room_node_id));

        ScrollArea::vertical().id_salt("rumble_next_tree").show(ui, |ui| {
            let resp = Tree::new("rumble_next_tree", &tree)
                .selected(selected)
                .drag_drop(true)
                .show(ui);

            if let Some(id) = resp.toggled {
                let current = lookup_expanded(&tree, id).unwrap_or(true);
                self.expanded.insert(id, !current);
            }
            if let Some(id) = resp.clicked {
                self.selected = Some(id);
            }
            if let Some(new_sel) = resp.selection_changed {
                self.selected = new_sel;
            }
            if let Some(id) = resp.double_clicked.or(resp.activated)
                && is_room(id)
                && let Some(NodeRef::Room(uuid)) = id_map.get(&id)
            {
                backend.send(Command::JoinRoom { room_id: *uuid });
            }
            if let Some(drop) = resp.dropped {
                handle_room_drop(state, backend, &id_map, drop);
            }
            if let Some((id, pos)) = resp.context
                && let Some(node_ref) = id_map.get(&id)
            {
                let target = match node_ref {
                    NodeRef::User(uid) => {
                        let user = state.get_user(*uid);
                        let name = user
                            .map(|u| u.username.clone())
                            .unwrap_or_else(|| format!("user #{uid}"));
                        let server_muted = user.map(|u| u.server_muted).unwrap_or(false);
                        ContextTarget::User {
                            id: *uid,
                            name,
                            locally_muted: state.audio.is_user_muted(*uid),
                            server_muted,
                        }
                    }
                    NodeRef::Room(rid) => {
                        let name = state
                            .room_tree
                            .get(*rid)
                            .map(|n| n.name.clone())
                            .unwrap_or_else(|| "(room)".into());
                        ContextTarget::Room { id: *rid, name }
                    }
                };
                self.context_menu = Some(TreeContext { target, pos });
            }
        });
    }
}

fn filter_tree(nodes: &mut Vec<TreeNode>, needle: &str) {
    let needle = needle.to_ascii_lowercase();
    nodes.retain_mut(|node| node_matches_filter(node, &needle));
}

fn node_matches_filter(node: &mut TreeNode, needle: &str) -> bool {
    node.children.retain_mut(|child| node_matches_filter(child, needle));
    if !node.children.is_empty() {
        node.expanded = true;
        return true;
    }
    let name = match &node.kind {
        TreeNodeKind::Channel { name } | TreeNodeKind::User { name, .. } => name,
    };
    name.to_ascii_lowercase().contains(needle)
}

fn apply_expanded(nodes: &mut [TreeNode], overrides: &HashMap<TreeNodeId, bool>) {
    for n in nodes {
        if let Some(v) = overrides.get(&n.id) {
            n.expanded = *v;
        }
        apply_expanded(&mut n.children, overrides);
    }
}

fn lookup_expanded(nodes: &[TreeNode], id: TreeNodeId) -> Option<bool> {
    for n in nodes {
        if n.id == id {
            return Some(n.expanded);
        }
        if let Some(r) = lookup_expanded(&n.children, id) {
            return Some(r);
        }
    }
    None
}

// ---------- Room header (breadcrumbs) ----------

pub fn room_header(ui: &mut Ui, state: &State) {
    let theme = ui.theme();
    let tokens = theme.tokens().clone();

    let (crumbs, peers, description) = match state.my_room_id {
        Some(id) => (
            crumbs_for_room(state, id),
            adapters::peers_in_current_room(state),
            state.room_tree.get(id).and_then(|room| room.description.clone()),
        ),
        None => (vec!["— not in a room —".to_string()], 0, None),
    };

    SurfaceFrame::new(SurfaceKind::Toolbar)
        .inner_margin(Margin::symmetric(14, 8))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                let muted = tokens.text_muted;
                let faint = tokens.line_soft;
                let mono = tokens.font_mono.clone();

                ui.horizontal(|ui| {
                    let (last, head) = crumbs
                        .split_last()
                        .map(|(l, h)| (l.as_str(), h))
                        .unwrap_or(("(no room)", &[]));
                    for c in head {
                        ui.label(RichText::new(c).color(muted).font(mono.clone()));
                        ui.label(RichText::new("/").color(faint).font(mono.clone()));
                    }
                    ui.label(
                        RichText::new(last)
                            .color(tokens.text)
                            .strong()
                            .font(tokens.font_body.clone()),
                    );

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let summary = format!("● {peers} connected · text: ephemeral (last 50)");
                        ui.label(RichText::new(summary).color(muted).font(mono.clone()));
                    });
                });
                if let Some(description) = description.filter(|d| !d.trim().is_empty()) {
                    ui.label(RichText::new(description).color(muted).font(tokens.font_label.clone()));
                }
            });
        });
}

// ---------- Chat stream ----------

const AVATAR_SIZE: f32 = 24.0;

impl Shell {
    pub fn chat_stream<B: UiBackend>(&mut self, ui: &mut Ui, state: &State, backend: &B) {
        let theme = ui.theme();
        let tokens = theme.tokens().clone();

        let entries = adapters::chat_entries(state, self.chat_timestamp_format);

        // Snapshot once per frame: chat draw needs to look up live
        // progress/local-path info per `FileOffer`, but hammering the
        // plugin lock for every card is wasteful.
        let transfer_index: HashMap<String, rumble_client_traits::file_transfer::TransferStatus> =
            backend.transfers().into_iter().map(|s| (s.id.0.clone(), s)).collect();

        ScrollArea::vertical()
            .id_salt("rumble_next_chat")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.style_mut().spacing.item_spacing.y = 8.0;
                ui.add_space(8.0);
                if entries.is_empty() {
                    ui.label(
                        RichText::new("No chat messages yet. Say hello.")
                            .color(tokens.text_muted)
                            .italics(),
                    );
                }
                for entry in &entries {
                    match entry {
                        ChatEntry::Sys(m) => draw_sys(ui, &tokens, m),
                        ChatEntry::Msg(m) => draw_msg(
                            ui,
                            &tokens,
                            m,
                            backend,
                            &mut self.accepted_offers,
                            &transfer_index,
                            &mut self.pending_open,
                            &mut self.image_lightbox,
                            &mut self.last_image_preview_rect,
                        ),
                    }
                }
                ui.add_space(10.0);
            });
    }
}

fn draw_sys(ui: &mut Ui, tokens: &rumble_widgets::Tokens, m: &SysMsg) {
    let dot_color = match m.tone {
        SysTone::Join => Color32::from_rgb(0x2f, 0x85, 0x5a),
        SysTone::Disc => tokens.danger,
        SysTone::Info => tokens.line_soft,
    };
    ui.horizontal(|ui| {
        ui.add_space(34.0);
        let mono = tokens.font_mono.clone();
        ui.label(RichText::new(&m.t).color(tokens.line_soft).font(mono.clone()));
        ui.label(RichText::new("●").color(dot_color).font(mono.clone()));
        ui.label(RichText::new(&m.text).color(tokens.text_muted).font(mono));
    });
}

#[allow(clippy::too_many_arguments)]
fn draw_msg<B: UiBackend>(
    ui: &mut Ui,
    tokens: &rumble_widgets::Tokens,
    m: &ChatMsg,
    backend: &B,
    accepted_offers: &mut HashSet<String>,
    transfer_index: &HashMap<String, rumble_client_traits::file_transfer::TransferStatus>,
    pending_open: &mut Vec<PendingOpen>,
    image_lightbox: &mut Option<ImageLightbox>,
    last_image_preview_rect: &mut Option<egui::Rect>,
) {
    ui.horizontal_top(|ui| {
        let (avatar_rect, _) = ui.allocate_exact_size(Vec2::splat(AVATAR_SIZE), Sense::hover());
        let initial = m
            .who
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase())
            .unwrap_or('?')
            .to_string();
        ui.painter().add(RectShape::new(
            avatar_rect,
            CornerRadius::same(3),
            tokens.line_soft,
            Stroke::NONE,
            StrokeKind::Inside,
        ));
        ui.painter().text(
            avatar_rect.center(),
            Align2::CENTER_CENTER,
            initial,
            FontId::new(10.0, tokens.font_body.family.clone()),
            Color32::WHITE,
        );

        ui.add_space(10.0);
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&m.who)
                        .color(tokens.text)
                        .strong()
                        .font(tokens.font_body.clone()),
                );
                ui.label(
                    RichText::new(&m.t)
                        .color(tokens.text_muted)
                        .font(tokens.font_mono.clone()),
                );
            });
            if let Some(body) = &m.body {
                ui.label(RichText::new(body).color(tokens.text).font(tokens.font_body.clone()));
            }
            if let Some(media) = &m.media {
                draw_media(
                    ui,
                    tokens,
                    media,
                    backend,
                    accepted_offers,
                    transfer_index,
                    pending_open,
                    image_lightbox,
                    last_image_preview_rect,
                );
            }
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn draw_media<B: UiBackend>(
    ui: &mut Ui,
    tokens: &rumble_widgets::Tokens,
    media: &Media,
    backend: &B,
    accepted_offers: &mut HashSet<String>,
    transfer_index: &HashMap<String, rumble_client_traits::file_transfer::TransferStatus>,
    pending_open: &mut Vec<PendingOpen>,
    image_lightbox: &mut Option<ImageLightbox>,
    last_image_preview_rect: &mut Option<egui::Rect>,
) {
    match media {
        Media::Image { .. } => {
            // Legacy variant — adapters never construct this anymore
            // (real attachments come through `FileOffer`). Render
            // nothing rather than a misleading placeholder.
        }
        Media::File { ext, name, size } => {
            let _ = draw_file_card(ui, tokens, ext, name, size, None);
        }
        Media::FileOffer {
            ext,
            name,
            size,
            transfer_id,
            share_data,
            is_own,
        } => {
            // Choose card action based on what we know about the
            // transfer. Priority order:
            //   1. We have a live `TransferStatus` for this id →
            //      surface progress / completion / error.
            //   2. The user has clicked Download already → "Downloading…"
            //      placeholder while the plugin spins up.
            //   3. Default → "Download" button.
            let status = transfer_index.get(transfer_id);
            let action = match (is_own, status) {
                (_, Some(s)) if s.is_finished && s.local_path.is_some() => {
                    let path = s.local_path.clone().expect("checked above");
                    Some(FileOfferAction::Completed { path })
                }
                (_, Some(s)) if s.error.is_some() => Some(FileOfferAction::Failed(s.error.clone().unwrap_or_default())),
                (_, Some(s)) => Some(FileOfferAction::InProgress {
                    progress: s.progress.clamp(0.0, 1.0),
                }),
                (true, None) => None,
                (false, None) if accepted_offers.contains(transfer_id) => Some(FileOfferAction::Accepted),
                (false, None) => Some(FileOfferAction::Available),
            };

            // For completed transfers of image files, show an inline
            // preview that opens the lightbox on click. The full file
            // card (with Open / Show in folder buttons) is still drawn
            // underneath for non-image actions and as a fallback if
            // the image fails to decode.
            let is_image = is_image_extension(ext);
            if let Some(FileOfferAction::Completed { path }) = &action
                && is_image
            {
                let preview = draw_image_preview(ui, tokens, path, name, size);
                let preview_resp = preview.response;
                *last_image_preview_rect = Some(preview.image_rect);
                if preview.clicked {
                    *image_lightbox = Some(ImageLightbox {
                        path: path.clone(),
                        name: name.clone(),
                        uri: file_uri(path),
                    });
                }
                attach_file_card_context_menu(&preview_resp, Some(path.clone()), name.clone(), pending_open, ui.ctx());
                return;
            }

            let result = draw_file_card(ui, tokens, ext, name, size, action.clone());
            match result.click {
                FileOfferClick::Download => {
                    backend.send(Command::DownloadFile {
                        share_data: share_data.clone(),
                    });
                    accepted_offers.insert(transfer_id.clone());
                }
                FileOfferClick::Open(path) => pending_open.push(PendingOpen::File(path)),
                FileOfferClick::ShowInFolder(path) => pending_open.push(PendingOpen::InFolder(path)),
                FileOfferClick::None => {}
            }
            let path = match action {
                Some(FileOfferAction::Completed { path }) => Some(path),
                _ => None,
            };
            attach_file_card_context_menu(&result.response, path, name.clone(), pending_open, ui.ctx());
        }
    }
}

/// True if `ext` (file extension, dot stripped, any case) names an
/// image format the egui_extras image loader can decode for us.
fn is_image_extension(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff"
    )
}

/// `file://` URI for `path`, used as the cache key for egui's image
/// loader. Same path always produces the same URI so the texture is
/// shared between an inline thumbnail and the lightbox.
fn file_uri(path: &std::path::Path) -> String {
    format!("file://{}", path.display())
}

struct ImagePreview {
    response: egui::Response,
    clicked: bool,
    /// Rect of the inner image widget (not the surrounding surface).
    /// Used by tests to send a synthetic click at the image center.
    image_rect: egui::Rect,
}

/// Inline image thumbnail shown for completed image transfers in the
/// chat log. Draws the image inside a bordered surface with a small
/// caption underneath ("name · size") so the user still sees what the
/// file is.
fn draw_image_preview(
    ui: &mut Ui,
    tokens: &rumble_widgets::Tokens,
    path: &std::path::Path,
    name: &str,
    size: &str,
) -> ImagePreview {
    ui.add_space(2.0);
    let max_w = ui.available_width().clamp(80.0, 360.0);
    let max_h = 220.0_f32;
    let uri = file_uri(path);

    // Lay out the image as a non-interactive widget, then layer a
    // separate `Sense::click` interaction on the same rect.
    let inner = SurfaceFrame::new(SurfaceKind::Group)
        .inner_margin(Margin::symmetric(6, 6))
        .show(ui, |ui| {
            let img = egui::Image::new(&uri)
                .max_width(max_w)
                .max_height(max_h)
                .corner_radius(4.0);
            let img_rect = ui.add(img).rect;
            let click_id = ui.id().with(("rumble_next_image_preview", uri.as_str()));
            let click_resp = ui
                .interact(img_rect, click_id, Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("{name} · {size}"))
                    .color(tokens.text_muted)
                    .font(tokens.font_mono.clone()),
            );
            (click_resp.clicked(), img_rect)
        });
    let (clicked, image_rect) = inner.inner;
    ImagePreview {
        response: inner.response,
        clicked,
        image_rect,
    }
}

/// Attach the standard right-click context menu (Open / Show in folder
/// / Save as / Copy path) to `response`. When `path` is `None` the
/// transfer hasn't completed yet and we suppress the menu — all the
/// actions need a local file to act on.
fn attach_file_card_context_menu(
    response: &egui::Response,
    path: Option<std::path::PathBuf>,
    name: String,
    pending_open: &mut Vec<PendingOpen>,
    ctx: &egui::Context,
) {
    let Some(path) = path else { return };
    response.clone().context_menu(|ui| {
        ui.set_min_width(160.0);
        if ui.button("Open").clicked() {
            pending_open.push(PendingOpen::File(path.clone()));
            ui.close();
        }
        if ui.button("Show in folder").clicked() {
            pending_open.push(PendingOpen::InFolder(path.clone()));
            ui.close();
        }
        if ui.button("Save as…").clicked() {
            pending_open.push(PendingOpen::SaveAs {
                src: path.clone(),
                suggested_name: name.clone(),
            });
            ui.close();
        }
        if ui.button("Copy path").clicked() {
            ctx.copy_text(path.display().to_string());
            ui.close();
        }
    });
}

#[derive(Clone)]
enum FileOfferAction {
    Available,
    Accepted,
    InProgress { progress: f32 },
    Completed { path: std::path::PathBuf },
    Failed(String),
}

enum FileOfferClick {
    None,
    Download,
    Open(std::path::PathBuf),
    ShowInFolder(std::path::PathBuf),
}

struct FileCardResult {
    click: FileOfferClick,
    response: egui::Response,
}

fn draw_file_card(
    ui: &mut Ui,
    tokens: &rumble_widgets::Tokens,
    ext: &str,
    name: &str,
    size: &str,
    action: Option<FileOfferAction>,
) -> FileCardResult {
    let mut click = FileOfferClick::None;
    ui.add_space(2.0);
    let response = SurfaceFrame::new(SurfaceKind::Group)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (ico, _) = ui.allocate_exact_size(Vec2::new(36.0, 40.0), Sense::hover());
                ui.painter().add(RectShape::new(
                    ico,
                    CornerRadius::same(2),
                    tokens.surface,
                    Stroke::new(1.0, tokens.line_soft),
                    StrokeKind::Inside,
                ));
                ui.painter().text(
                    ico.center(),
                    Align2::CENTER_CENTER,
                    ext,
                    tokens.font_mono.clone(),
                    tokens.text_muted,
                );
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    let title = ui.label(
                        RichText::new(name)
                            .color(tokens.text)
                            .strong()
                            .font(tokens.font_body.clone()),
                    );
                    let title_resp = title.interact(Sense::click());
                    ui.label(
                        RichText::new(size)
                            .color(tokens.text_muted)
                            .font(tokens.font_mono.clone()),
                    );
                    // If we have a path, surface it under the size and let
                    // the user click the filename to open the file.
                    if let Some(FileOfferAction::Completed { path }) = &action {
                        ui.label(
                            RichText::new(format!("Saved to {}", path.display()))
                                .color(tokens.text_muted)
                                .font(tokens.font_mono.clone()),
                        );
                        if title_resp.clicked() {
                            click = FileOfferClick::Open(path.clone());
                        }
                    }
                });
                if let Some(action) = action {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| match action {
                        FileOfferAction::Available => {
                            if ButtonArgs::new("Download")
                                .role(PressableRole::Default)
                                .show(ui)
                                .clicked()
                            {
                                click = FileOfferClick::Download;
                            }
                        }
                        FileOfferAction::Accepted | FileOfferAction::InProgress { .. } => {
                            let label = match action {
                                FileOfferAction::InProgress { progress } => {
                                    format!("Downloading… {}%", (progress * 100.0).round() as u32)
                                }
                                _ => "Downloading…".to_string(),
                            };
                            ui.label(
                                RichText::new(label)
                                    .color(tokens.text_muted)
                                    .font(tokens.font_mono.clone()),
                            );
                        }
                        FileOfferAction::Completed { path } => {
                            if ButtonArgs::new("Show in folder")
                                .role(PressableRole::Ghost)
                                .show(ui)
                                .clicked()
                            {
                                click = FileOfferClick::ShowInFolder(path.clone());
                            }
                            if ButtonArgs::new("Open").role(PressableRole::Default).show(ui).clicked() {
                                click = FileOfferClick::Open(path);
                            }
                        }
                        FileOfferAction::Failed(err) => {
                            ui.label(
                                RichText::new(format!("Failed: {err}"))
                                    .color(tokens.danger)
                                    .font(tokens.font_mono.clone()),
                            );
                        }
                    });
                }
            });
        })
        .response;
    FileCardResult { click, response }
}

// ---------- Composer ----------

impl Shell {
    pub fn composer<B: UiBackend>(&mut self, ui: &mut Ui, state: &State, backend: &B) {
        let no_room = state.my_room_id.is_none();
        let no_text_permission = state
            .my_room_id
            .map(|room| {
                state
                    .per_room_permissions
                    .get(&room)
                    .copied()
                    .unwrap_or(state.effective_permissions)
            })
            .map(|bits| !Permissions::from_bits_truncate(bits).contains(Permissions::TEXT_MESSAGE))
            .unwrap_or(false);
        let disabled = no_room || no_text_permission;
        let placeholder = if disabled {
            if no_text_permission {
                "(no permission to send messages here)"
            } else {
                "connect and join a room to chat"
            }
        } else {
            "type a message — try /msg <user> hi or /tree announcement"
        };

        let can_share_file = state
            .my_room_id
            .map(|room| {
                state
                    .per_room_permissions
                    .get(&room)
                    .copied()
                    .unwrap_or(state.effective_permissions)
            })
            .map(|bits| Permissions::from_bits_truncate(bits).contains(Permissions::SHARE_FILE))
            .unwrap_or(false);

        SurfaceFrame::new(SurfaceKind::Panel)
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ButtonArgs::new("⟳ sync")
                        .role(PressableRole::Ghost)
                        .disabled(no_room)
                        .show(ui)
                        .clicked()
                    {
                        backend.send(Command::RequestChatHistory);
                    }
                    if ButtonArgs::new("📎 file")
                        .role(PressableRole::Ghost)
                        .disabled(no_room || !can_share_file)
                        .show(ui)
                        .on_hover_text(if can_share_file {
                            "Share a file with this room"
                        } else {
                            "You don't have permission to share files here"
                        })
                        .clicked()
                    {
                        self.share_file_requested = true;
                    }
                    if ButtonArgs::new("📥 transfers")
                        .role(PressableRole::Ghost)
                        .active(self.transfers_open)
                        .show(ui)
                        .on_hover_text("Show active uploads and downloads")
                        .clicked()
                    {
                        self.transfers_open = !self.transfers_open;
                    }

                    let avail = ui.available_width() - 96.0;
                    let mut submitted: Option<String> = None;

                    ui.add_enabled_ui(!disabled, |ui| {
                        let resp = TextInput::new(&mut self.composer)
                            .placeholder(placeholder)
                            .submit_on_enter(true)
                            .desired_width(avail.max(80.0))
                            .show(ui);
                        if let Some(text) = resp.submitted
                            && !text.trim().is_empty()
                        {
                            submitted = Some(text);
                        }
                    });

                    if ButtonArgs::new("send ↵")
                        .role(PressableRole::Primary)
                        .disabled(disabled || self.composer.trim().is_empty())
                        .show(ui)
                        .clicked()
                    {
                        submitted = Some(std::mem::take(&mut self.composer));
                    }

                    if let Some(text) = submitted {
                        dispatch_composer(state, backend, text);
                    }
                });
                if no_text_permission {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("(no permission to send messages here)")
                            .color(ui.theme().tokens().text_muted)
                            .font(ui.theme().font(rumble_widgets::TextRole::Label)),
                    );
                }
            });
    }
}

/// Translate a composer line into the right `Command`. Slash commands
/// route to specialised flows; everything else is room chat.
///
/// - `/msg <user> <text>` → `SendDirectMessage` to the matching user.
///   Validation errors land in chat as a local system message, not
///   as a toast — a toast for a typo is heavier than the offence
///   warrants.
/// - `/tree <text>` → `SendTreeChat` (broadcast to current room and
///   all descendant rooms).
/// - `<text>` → `SendChat` to the current room.
fn dispatch_composer<B: UiBackend>(state: &State, backend: &B, text: String) {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("/msg ") {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let username = parts.next().unwrap_or("").trim();
        let body = parts.next().map(str::trim).unwrap_or("");
        if username.is_empty() || body.is_empty() {
            backend.send(Command::SendChat {
                text: "(usage: /msg <username> <text>)".to_string(),
            });
            return;
        }
        match find_user_by_name(state, username) {
            Some((target_user_id, target_username)) => backend.send(Command::SendDirectMessage {
                target_user_id,
                target_username,
                text: body.to_string(),
            }),
            None => backend.send(Command::SendChat {
                text: format!("(no user named '{username}' is connected)"),
            }),
        }
        return;
    }
    if let Some(rest) = trimmed.strip_prefix("/tree ") {
        let body = rest.trim();
        if !body.is_empty() {
            backend.send(Command::SendTreeChat { text: body.to_string() });
        }
        return;
    }
    backend.send(Command::SendChat { text });
}

/// Case-insensitive username → `(user_id, canonical username)` lookup
/// against the current roster. Returns the first match. The canonical
/// username preserves the casing the server reported; the DM command
/// wants both fields.
fn find_user_by_name(state: &State, name: &str) -> Option<(u64, String)> {
    let needle = name.to_ascii_lowercase();
    state.users.iter().find_map(|u| {
        if u.username.to_ascii_lowercase() == needle {
            u.user_id.as_ref().map(|id| (id.value, u.username.clone()))
        } else {
            None
        }
    })
}

// ---------- Voice-state toggles ----------

impl Shell {
    pub fn voice_row<B: UiBackend>(&mut self, ui: &mut Ui, state: &State, backend: &B) {
        let muted = state.audio.self_muted;
        let deafened = state.audio.self_deafened;
        let server_muted = adapters::am_i_server_muted(state);
        // While server-muted the audio task suppresses capture, so
        // `is_transmitting` correctly reads false — but we still gate
        // the PTT button so clicks don't even attempt to start.
        let ptt_active = state.audio.is_transmitting;

        if ButtonArgs::new(if server_muted { "🔒 Server muted" } else { "🎤 Mute" })
            .role(PressableRole::Default)
            .active(muted || server_muted)
            .disabled(server_muted)
            .show(ui)
            .on_hover_text(if server_muted {
                "Server muted — you cannot speak in this room"
            } else if muted {
                "Click to unmute"
            } else {
                "Click to mute"
            })
            .clicked()
        {
            backend.send(Command::SetMuted { muted: !muted });
        }
        if ButtonArgs::new("🔇 Deafen")
            .role(PressableRole::Danger)
            .active(deafened)
            .show(ui)
            .clicked()
        {
            backend.send(Command::SetDeafened { deafened: !deafened });
        }

        // Latched click toggle. Hold-to-talk lives on the global
        // hotkey (default Space), wired in `App::pump_hotkeys`; this
        // button is a mouse-friendly fallback.
        let ptt_resp = ButtonArgs::new("● PTT")
            .role(PressableRole::Accent)
            .active(ptt_active && !server_muted)
            .disabled(server_muted)
            .show(ui);
        if ptt_resp.clicked() && !server_muted {
            if ptt_active {
                backend.send(Command::StopTransmit);
            } else {
                backend.send(Command::StartTransmit);
            }
        }
    }
}

// ---------- Self / avatar pill ----------

pub fn avatar_pill(ui: &mut Ui, name: &str, talking: bool) {
    let theme = ui.theme();
    let tokens = theme.tokens().clone();
    let _ = rumble_widgets::Pressable::new(("avatar-pill", name))
        .role(PressableRole::Ghost)
        .min_size(Vec2::new(120.0, 30.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
                ui.painter().add(RectShape::new(
                    rect,
                    CornerRadius::same(11),
                    tokens.text,
                    Stroke::NONE,
                    StrokeKind::Inside,
                ));
                let initial = name
                    .chars()
                    .next()
                    .map(|c| c.to_ascii_uppercase())
                    .unwrap_or('?')
                    .to_string();
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    initial,
                    FontId::new(11.0, tokens.font_body.family.clone()),
                    tokens.surface,
                );
                ui.add_space(6.0);
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                // Bright accent when transmitting, calm green otherwise —
                // matches the PTT button so all "I'm live" indicators
                // light up together.
                let dot_color = if talking {
                    tokens.accent
                } else {
                    Color32::from_rgb(0x2f, 0x85, 0x5a)
                };
                ui.painter().circle_filled(dot.center(), 4.0, dot_color);
                ui.label(RichText::new(name).color(tokens.text).font(tokens.font_body.clone()));
            });
        });
}

// ---------- Transfers window ----------

impl Shell {
    /// Render the active transfers window if it's open. Lists
    /// in-flight uploads and downloads with progress, byte counts,
    /// and per-row actions. Called by `render_overlays` so it floats
    /// above the paradigm body like other overlays.
    pub fn render_transfers_window<B: UiBackend>(&mut self, ctx: &egui::Context, backend: &B) {
        if !self.transfers_open {
            return;
        }
        let mut open = true;
        let transfers = backend.transfers();
        egui::Window::new("Transfers")
            .open(&mut open)
            .resizable(true)
            .default_width(420.0)
            .default_height(280.0)
            .show(ctx, |ui| {
                let tokens = ui.theme().tokens().clone();
                if transfers.is_empty() {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("No active uploads or downloads.")
                            .color(tokens.text_muted)
                            .italics(),
                    );
                    return;
                }
                ScrollArea::vertical()
                    .id_salt("rumble_next_transfers_list")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for status in &transfers {
                            transfer_row(ui, &tokens, status, &mut self.pending_open, backend);
                            ui.add_space(6.0);
                        }
                    });
            });
        // Toggle off if the user clicks the window's close ✕.
        self.transfers_open = open;
    }
}

fn transfer_row<B: UiBackend>(
    ui: &mut Ui,
    tokens: &rumble_widgets::Tokens,
    status: &rumble_client_traits::file_transfer::TransferStatus,
    pending_open: &mut Vec<PendingOpen>,
    backend: &B,
) {
    use rumble_client_traits::file_transfer::PluginTransferState;
    SurfaceFrame::new(SurfaceKind::Group)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&status.name)
                            .color(tokens.text)
                            .strong()
                            .font(tokens.font_body.clone()),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let label = transfer_state_label(status);
                        ui.label(
                            RichText::new(label)
                                .color(tokens.text_muted)
                                .font(tokens.font_mono.clone()),
                        );
                    });
                });
                let progress = status.progress.clamp(0.0, 1.0);
                ui.add(egui::ProgressBar::new(progress).desired_width(f32::INFINITY));
                ui.label(
                    RichText::new(transfer_byte_summary(status))
                        .color(tokens.text_muted)
                        .font(tokens.font_mono.clone()),
                );
                if let Some(err) = &status.error {
                    ui.label(RichText::new(err).color(tokens.danger).font(tokens.font_mono.clone()));
                }
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    let id = &status.id;
                    let is_active = !status.is_finished
                        && !matches!(status.state, PluginTransferState::Error | PluginTransferState::Seeding);
                    if ButtonArgs::new("Cancel")
                        .role(PressableRole::Danger)
                        .disabled(!is_active)
                        .show(ui)
                        .clicked()
                        && let Err(e) = backend.cancel_transfer(id, false)
                    {
                        tracing::warn!("cancel transfer {} failed: {e}", id.0);
                    }
                    let path = status.local_path.clone();
                    let can_open = status.is_finished && path.is_some();
                    if ButtonArgs::new("Open")
                        .role(PressableRole::Default)
                        .disabled(!can_open)
                        .show(ui)
                        .on_hover_text("Open with the system default")
                        .clicked()
                        && let Some(p) = path.clone()
                    {
                        pending_open.push(PendingOpen::File(p));
                    }
                    if ButtonArgs::new("Show in folder")
                        .role(PressableRole::Ghost)
                        .disabled(!can_open)
                        .show(ui)
                        .clicked()
                        && let Some(p) = path
                    {
                        pending_open.push(PendingOpen::InFolder(p));
                    }
                });
            });
        });
}

fn transfer_state_label(status: &rumble_client_traits::file_transfer::TransferStatus) -> String {
    use rumble_client_traits::file_transfer::PluginTransferState;
    let pct = (status.progress.clamp(0.0, 1.0) * 100.0).round() as u32;
    match status.state {
        _ if status.is_finished => "Done".into(),
        PluginTransferState::Initializing => format!("Uploading · {pct}%"),
        PluginTransferState::Downloading => format!("Downloading · {pct}%"),
        PluginTransferState::Seeding => "Seeding".into(),
        PluginTransferState::Paused => "Paused".into(),
        PluginTransferState::Error => "Error".into(),
    }
}

fn transfer_byte_summary(status: &rumble_client_traits::file_transfer::TransferStatus) -> String {
    let done = (status.progress.clamp(0.0, 1.0) as f64 * status.size as f64) as u64;
    format!("{} / {}", format_bytes(done), format_bytes(status.size))
}

fn format_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let n = n as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.1} KB", n / KB)
    } else {
        format!("{n:.0} B")
    }
}

// ---------- Context menu + modals (overlays) ----------

impl Shell {
    /// Render any open overlay: the right-click context menu on the
    /// tree, or a text-input modal for rename/create/ban/DM. Called by
    /// each paradigm after its main body so overlays float above it.
    pub fn render_overlays<B: UiBackend>(&mut self, ctx: &egui::Context, state: &State, backend: &B) {
        // Promote queued incoming-file prompts into the modal slot
        // before rendering, so the next pending prompt opens as soon
        // as the previous one closes.
        self.drain_offer_queue();
        // The context menu reads `state` to surface the live per-user
        // volume override; the modal needs `state` for username lookups.
        self.render_context_menu(ctx, state, backend);
        self.render_pending_modal(ctx, backend);
        self.render_transfers_window(ctx, backend);
        self.render_image_lightbox(ctx);
    }

    /// Click-to-enlarge image viewer. Shown when a chat image preview
    /// is clicked. ESC, the close button, and clicking outside the
    /// modal frame all dismiss it; on close we evict the texture to
    /// free GPU memory.
    fn render_image_lightbox(&mut self, ctx: &egui::Context) {
        let Some(lightbox) = self.image_lightbox.clone() else {
            return;
        };

        // Use 90% of the modal-content rect as a soft max so really
        // tall or wide images still leave room for the title bar /
        // close button.
        let content = ctx.content_rect();
        let max_w = (content.width() * 0.9).max(120.0);
        let max_h = (content.height() * 0.9 - 60.0).max(120.0);

        let modal = egui::Modal::new(egui::Id::new("rumble_next_image_lightbox")).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(&lightbox.name);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ButtonArgs::new("Close").role(PressableRole::Default).show(ui).clicked() {
                        ui.close();
                    }
                });
            });
            ui.add_space(6.0);
            ui.add(
                egui::Image::new(&lightbox.uri)
                    .max_width(max_w)
                    .max_height(max_h)
                    .corner_radius(6.0),
            );
        });

        if modal.should_close() || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            ctx.forget_image(&lightbox.uri);
            self.image_lightbox = None;
        }
    }

    fn render_context_menu<B: UiBackend>(&mut self, ctx: &egui::Context, state: &State, backend: &B) {
        let Some(menu) = self.context_menu.clone() else {
            return;
        };

        let mut close = false;
        let mut next_modal: Option<PendingModal> = None;

        egui::Area::new(egui::Id::new("rumble_next_tree_ctx_menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(menu.pos)
            .show(ctx, |ui| {
                SurfaceFrame::new(SurfaceKind::Popup)
                    .inner_margin(Margin::same(6))
                    .show(ui, |ui| {
                        ui.set_min_width(180.0);
                        let perms = effective_perms_in_current_room(state);
                        match &menu.target {
                            ContextTarget::User {
                                id,
                                name,
                                locally_muted,
                                server_muted,
                            } => {
                                header(ui, name);
                                if ctx_btn(
                                    ui,
                                    if *locally_muted {
                                        "Unmute locally"
                                    } else {
                                        "Mute locally"
                                    },
                                ) {
                                    backend.send(if *locally_muted {
                                        Command::UnmuteUser { user_id: *id }
                                    } else {
                                        Command::MuteUser { user_id: *id }
                                    });
                                    close = true;
                                }
                                if perms.contains(Permissions::MUTE_DEAFEN) {
                                    let label = if *server_muted {
                                        "Remove server mute"
                                    } else {
                                        "Server mute"
                                    };
                                    if ctx_btn(ui, label) {
                                        backend.send(Command::SetServerMute {
                                            target_user_id: *id,
                                            muted: !*server_muted,
                                        });
                                        close = true;
                                    }
                                }
                                // Per-user volume slider. The current
                                // override (if any) lives in
                                // `state.per_user_rx`; absent users
                                // default to 0 dB.
                                let mut volume_db =
                                    state.audio.per_user_rx.get(id).map(|cfg| cfg.volume_db).unwrap_or(0.0);
                                let before = volume_db;
                                ui.label(
                                    egui::RichText::new("Local volume")
                                        .color(ui.theme().tokens().text_muted)
                                        .font(ui.theme().font(rumble_widgets::TextRole::Label)),
                                );
                                rumble_widgets::Slider::new(&mut volume_db, -40.0..=40.0)
                                    .step(1.0)
                                    .suffix(" dB")
                                    .show(ui);
                                if (volume_db - before).abs() > 0.01 {
                                    backend.send(Command::SetUserVolume {
                                        user_id: *id,
                                        volume_db,
                                    });
                                }
                                ui.separator();
                                if ctx_btn(ui, "Direct message…") {
                                    next_modal = Some(PendingModal::DirectMessage {
                                        user_id: *id,
                                        username: name.clone(),
                                        text: String::new(),
                                    });
                                    close = true;
                                }
                                let can_kick = perms.contains(Permissions::KICK);
                                let can_ban = perms.contains(Permissions::BAN);
                                if can_kick || can_ban {
                                    ui.separator();
                                }
                                if can_kick && ctx_danger(ui, "Kick…") {
                                    next_modal = Some(PendingModal::Kick {
                                        user_id: *id,
                                        username: name.clone(),
                                        reason: String::new(),
                                    });
                                    close = true;
                                }
                                if can_ban && ctx_danger(ui, "Ban…") {
                                    next_modal = Some(PendingModal::Ban {
                                        user_id: *id,
                                        username: name.clone(),
                                        reason: String::new(),
                                        duration_idx: 0,
                                    });
                                    close = true;
                                }
                            }
                            ContextTarget::Room { id, name } => {
                                header(ui, name);
                                if ctx_btn(ui, "Join") {
                                    backend.send(Command::JoinRoom { room_id: *id });
                                    close = true;
                                }
                                if ctx_btn(ui, "New sub-room…") {
                                    next_modal = Some(PendingModal::CreateRoom {
                                        parent: Some(*id),
                                        parent_name: name.clone(),
                                        name: String::new(),
                                    });
                                    close = true;
                                }
                                if ctx_btn(ui, "Rename…") {
                                    next_modal = Some(PendingModal::RenameRoom {
                                        id: *id,
                                        original: name.clone(),
                                        name: name.clone(),
                                    });
                                    close = true;
                                }
                                if ctx_btn(ui, "Edit description…") {
                                    let current = state
                                        .room_tree
                                        .get(*id)
                                        .and_then(|n| n.description.clone())
                                        .unwrap_or_default();
                                    next_modal = Some(PendingModal::EditDescription {
                                        id: *id,
                                        name: name.clone(),
                                        description: current,
                                    });
                                    close = true;
                                }
                                if perms.contains(Permissions::WRITE)
                                    && let Some(room) = state.get_room(*id)
                                    && ctx_btn(ui, "Edit ACLs…")
                                {
                                    let group_options: Vec<String> =
                                        state.group_definitions.iter().map(|g| g.name.clone()).collect();
                                    next_modal = Some(PendingModal::EditAcls {
                                        room_id: *id,
                                        room_name: name.clone(),
                                        inherit_acl: room.inherit_acl,
                                        entries: room.acls.clone(),
                                        group_options,
                                    });
                                    close = true;
                                }
                                ui.separator();
                                if ctx_danger(ui, "Delete…") {
                                    next_modal = Some(PendingModal::DeleteRoom {
                                        id: *id,
                                        name: name.clone(),
                                    });
                                    close = true;
                                }
                            }
                        }
                    });
            });

        // Click outside the menu closes it. `any_click` fires on
        // release (frame after the press), so the right-click that
        // opened the menu won't immediately close it the same frame.
        let clicked_outside = ctx.input(|i| i.pointer.any_click()) && !ctx.is_pointer_over_area();
        if close || clicked_outside {
            self.context_menu = None;
        }
        if let Some(m) = next_modal {
            self.modal = Some(m);
        }
    }

    fn render_pending_modal<B: UiBackend>(&mut self, ctx: &egui::Context, backend: &B) {
        let Some(modal) = self.modal.as_mut() else {
            return;
        };

        let mut close = false;
        let mut submit: Option<Command> = None;
        let title: &str;
        let primary_label: &str;

        // Snapshot the simple type-based primary-button label here; the
        // body of the modal edits the buffer in place below.
        match modal {
            PendingModal::CreateRoom { .. } => {
                title = "Create room";
                primary_label = "Create";
            }
            PendingModal::RenameRoom { .. } => {
                title = "Rename room";
                primary_label = "Rename";
            }
            PendingModal::DeleteRoom { .. } => {
                title = "Delete room";
                primary_label = "Delete";
            }
            PendingModal::EditDescription { .. } => {
                title = "Room description";
                primary_label = "Save";
            }
            PendingModal::Kick { .. } => {
                title = "Kick user";
                primary_label = "Kick";
            }
            PendingModal::Ban { .. } => {
                title = "Ban user";
                primary_label = "Ban";
            }
            PendingModal::DirectMessage { .. } => {
                title = "Direct message";
                primary_label = "Send";
            }
            PendingModal::Elevate { .. } => {
                title = "Elevate to superuser";
                primary_label = "Elevate";
            }
            PendingModal::EditAcls { .. } => {
                title = "Edit ACLs";
                primary_label = "Save";
            }
            PendingModal::IncomingFileOffer { .. } => {
                title = "Incoming file";
                primary_label = "Accept";
            }
        }

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| match modal {
                PendingModal::CreateRoom {
                    parent,
                    parent_name,
                    name,
                } => {
                    ui.label(RichText::new(format!("Parent: {parent_name}")).color(ui.theme().tokens().text_muted));
                    ui.add_space(6.0);
                    GroupBox::new("Name")
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            TextInput::new(name)
                                .placeholder("Room name")
                                .desired_width(280.0)
                                .show(ui);
                        });
                    ui.add_space(8.0);
                    let can_submit = !name.trim().is_empty();
                    modal_buttons(ui, primary_label, can_submit, &mut close, |go| {
                        if go {
                            submit = Some(Command::CreateRoom {
                                name: name.trim().to_string(),
                                parent_id: *parent,
                            });
                        }
                    });
                }
                PendingModal::RenameRoom { id, original, name } => {
                    ui.label(RichText::new(format!("Was: {original}")).color(ui.theme().tokens().text_muted));
                    ui.add_space(6.0);
                    GroupBox::new("New name")
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            TextInput::new(name)
                                .placeholder("Room name")
                                .desired_width(280.0)
                                .show(ui);
                        });
                    ui.add_space(8.0);
                    let trimmed = name.trim().to_string();
                    let can_submit = !trimmed.is_empty() && trimmed != *original;
                    modal_buttons(ui, primary_label, can_submit, &mut close, |go| {
                        if go {
                            submit = Some(Command::RenameRoom {
                                room_id: *id,
                                new_name: trimmed.clone(),
                            });
                        }
                    });
                }
                PendingModal::DeleteRoom { id, name } => {
                    ui.label(
                        RichText::new(format!("Permanently delete room \"{name}\"?")).color(ui.theme().tokens().text),
                    );
                    ui.label(RichText::new("This cannot be undone.").color(ui.theme().tokens().text_muted));
                    ui.add_space(8.0);
                    modal_buttons(ui, primary_label, true, &mut close, |go| {
                        if go {
                            submit = Some(Command::DeleteRoom { room_id: *id });
                        }
                    });
                }
                PendingModal::EditDescription { id, name, description } => {
                    ui.label(RichText::new(format!("Description for {name}")).color(ui.theme().tokens().text_muted));
                    ui.add_space(6.0);
                    GroupBox::new("Description")
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            TextInput::new(description)
                                .placeholder("describe the room")
                                .desired_width(320.0)
                                .show(ui);
                        });
                    ui.add_space(8.0);
                    let trimmed = description.trim().to_string();
                    modal_buttons(ui, primary_label, true, &mut close, |go| {
                        if go {
                            submit = Some(Command::SetRoomDescription {
                                room_id: *id,
                                description: trimmed.clone(),
                            });
                        }
                    });
                }
                PendingModal::Kick {
                    user_id,
                    username,
                    reason,
                } => {
                    ui.label(
                        RichText::new(format!("Kick {username}"))
                            .strong()
                            .color(ui.theme().tokens().text),
                    );
                    ui.add_space(6.0);
                    GroupBox::new("Reason")
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            TextInput::new(reason)
                                .placeholder("optional reason shown to the user")
                                .desired_width(320.0)
                                .show(ui);
                        });
                    ui.add_space(8.0);
                    modal_buttons(ui, primary_label, true, &mut close, |go| {
                        if go {
                            submit = Some(Command::KickUser {
                                target_user_id: *user_id,
                                reason: reason.trim().to_string(),
                            });
                        }
                    });
                }
                PendingModal::Ban {
                    user_id,
                    username,
                    reason,
                    duration_idx,
                } => {
                    ui.label(
                        RichText::new(format!("Ban {username}"))
                            .strong()
                            .color(ui.theme().tokens().text),
                    );
                    ui.add_space(6.0);
                    GroupBox::new("Reason")
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            TextInput::new(reason)
                                .placeholder("optional reason shown to the user")
                                .desired_width(320.0)
                                .show(ui);
                        });
                    ui.add_space(6.0);
                    GroupBox::new("Duration")
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            let labels: Vec<String> = BAN_DURATIONS.iter().map(|(l, _)| l.to_string()).collect();
                            ComboBox::new("ban_duration", duration_idx, labels)
                                .width(200.0)
                                .show(ui);
                        });
                    ui.add_space(8.0);
                    let idx = (*duration_idx).min(BAN_DURATIONS.len().saturating_sub(1));
                    let secs = BAN_DURATIONS[idx].1;
                    let duration_seconds = if secs == 0 { None } else { Some(secs) };
                    modal_buttons(ui, primary_label, true, &mut close, |go| {
                        if go {
                            submit = Some(Command::BanUser {
                                target_user_id: *user_id,
                                reason: reason.trim().to_string(),
                                duration_seconds,
                            });
                        }
                    });
                }
                PendingModal::Elevate { password } => {
                    ui.label(
                        RichText::new("Enter the server's sudo password to gain superuser rights.")
                            .color(ui.theme().tokens().text_muted),
                    );
                    ui.add_space(6.0);
                    GroupBox::new("Password")
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            // Egui's `TextEdit::password(true)` handles
                            // the masking; the rumble-widgets TextInput
                            // doesn't expose that flag yet, so use the
                            // raw widget here.
                            ui.add(egui::TextEdit::singleline(password).password(true).desired_width(280.0));
                        });
                    ui.add_space(8.0);
                    let can_submit = !password.is_empty();
                    let pass = password.clone();
                    modal_buttons(ui, primary_label, can_submit, &mut close, |go| {
                        if go {
                            submit = Some(Command::Elevate { password: pass.clone() });
                        }
                    });
                }
                PendingModal::EditAcls {
                    room_id,
                    room_name,
                    inherit_acl,
                    entries,
                    group_options,
                } => {
                    ui.label(
                        RichText::new(format!("ACLs for \"{room_name}\""))
                            .strong()
                            .color(ui.theme().tokens().text),
                    );
                    ui.add_space(6.0);
                    ui.checkbox(inherit_acl, "Inherit from parent");
                    ui.add_space(6.0);

                    let mut to_remove: Option<usize> = None;
                    ScrollArea::vertical()
                        .id_salt("rumble_next_acl_entries")
                        .max_height(320.0)
                        .show(ui, |ui| {
                            for (i, entry) in entries.iter_mut().enumerate() {
                                acl_entry_row(ui, i, entry, group_options, &mut to_remove);
                            }
                        });
                    if let Some(idx) = to_remove {
                        entries.remove(idx);
                    }

                    ui.add_space(4.0);
                    if ButtonArgs::new("+ Add entry")
                        .role(PressableRole::Default)
                        .show(ui)
                        .clicked()
                    {
                        let default_group = group_options.first().cloned().unwrap_or_else(|| "default".to_string());
                        entries.push(RoomAclEntry {
                            group: default_group,
                            grant: 0,
                            deny: 0,
                            apply_here: true,
                            apply_subs: true,
                        });
                    }

                    ui.add_space(8.0);
                    let room_id_for_submit = *room_id;
                    let inherit = *inherit_acl;
                    let entries_for_submit = entries.clone();
                    modal_buttons(ui, primary_label, true, &mut close, |go| {
                        if go {
                            submit = Some(Command::SetRoomAcl {
                                room_id: room_id_for_submit,
                                inherit_acl: inherit,
                                entries: entries_for_submit.clone(),
                            });
                        }
                    });
                }
                PendingModal::DirectMessage {
                    user_id,
                    username,
                    text,
                } => {
                    ui.label(
                        RichText::new(format!("Message {username}"))
                            .strong()
                            .color(ui.theme().tokens().text),
                    );
                    ui.add_space(6.0);
                    // `submit_on_enter` clears the buffer and surfaces the
                    // typed text via `resp.submitted`; if we don't read it
                    // here, Enter just eats the message.
                    let mut enter_body: Option<String> = None;
                    GroupBox::new("Text")
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            let resp = TextInput::new(text)
                                .placeholder("write a direct message…")
                                .desired_width(320.0)
                                .submit_on_enter(true)
                                .show(ui);
                            enter_body = resp.submitted.filter(|s| !s.trim().is_empty());
                        });
                    ui.add_space(8.0);
                    let target_id = *user_id;
                    let target_name = username.clone();
                    if let Some(body) = enter_body {
                        submit = Some(Command::SendDirectMessage {
                            target_user_id: target_id,
                            target_username: target_name.clone(),
                            text: body.trim().to_string(),
                        });
                    }
                    let trimmed = text.trim().to_string();
                    let can_submit = !trimmed.is_empty();
                    modal_buttons(ui, primary_label, can_submit, &mut close, |go| {
                        if go {
                            submit = Some(Command::SendDirectMessage {
                                target_user_id: target_id,
                                target_username: target_name.clone(),
                                text: trimmed.clone(),
                            });
                        }
                    });
                }
                PendingModal::IncomingFileOffer {
                    transfer_id,
                    name,
                    size,
                    mime,
                    share_data,
                    from,
                } => {
                    let tokens = ui.theme().tokens().clone();
                    ui.label(
                        RichText::new(format!("{from} wants to send you a file"))
                            .strong()
                            .color(tokens.text),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(name.as_str())
                            .color(tokens.text)
                            .font(tokens.font_body.clone()),
                    );
                    ui.label(
                        RichText::new(format!("{} · {}", format_bytes(*size), mime))
                            .color(tokens.text_muted)
                            .font(tokens.font_mono.clone()),
                    );
                    ui.add_space(8.0);
                    let share_for_submit = share_data.clone();
                    let id_for_accept = transfer_id.clone();
                    modal_buttons(ui, primary_label, true, &mut close, |go| {
                        if go {
                            submit = Some(Command::DownloadFile {
                                share_data: share_for_submit.clone(),
                            });
                        }
                    });
                    // Mark the offer as accepted on Accept so the chat
                    // card flips to "Downloading…" immediately. Deny
                    // (Cancel) is a no-op beyond closing — `App` already
                    // tracked this id in `auto_handled_offers` so a
                    // history replay won't re-prompt.
                    if submit.is_some() {
                        self.accepted_offers.insert(id_for_accept);
                    }
                }
            });

        if let Some(cmd) = submit {
            backend.send(cmd);
            close = true;
        }
        if close {
            self.modal = None;
        }
    }
}

fn header(ui: &mut Ui, name: &str) {
    let tokens = ui.theme().tokens().clone();
    ui.label(
        RichText::new(name)
            .color(tokens.text_muted)
            .font(tokens.font_label.clone()),
    );
    ui.add_space(2.0);
}

fn ctx_btn(ui: &mut Ui, label: &str) -> bool {
    ButtonArgs::new(label)
        .role(PressableRole::Ghost)
        .min_width(160.0)
        .show(ui)
        .clicked()
}

fn ctx_danger(ui: &mut Ui, label: &str) -> bool {
    ButtonArgs::new(label)
        .role(PressableRole::Danger)
        .min_width(160.0)
        .show(ui)
        .clicked()
}

/// Effective permissions for the current user in their current room.
/// Falls back to the connection-wide `effective_permissions` when the
/// user isn't in a room. Used to gate context-menu items so we don't
/// offer actions the server will refuse.
fn effective_perms_in_current_room(state: &State) -> Permissions {
    let bits = state
        .my_room_id
        .and_then(|room| state.per_room_permissions.get(&room).copied())
        .unwrap_or(state.effective_permissions);
    Permissions::from_bits_truncate(bits)
}

/// Handle a drag-drop on the room tree. Only room→room drops do
/// anything: dropping a user is a no-op, dropping a room onto another
/// room reparents (Into → child of target; Above/Below → sibling of
/// target). Server validates that the source is movable; we just emit
/// the command.
fn handle_room_drop<B: UiBackend>(
    state: &State,
    backend: &B,
    id_map: &HashMap<TreeNodeId, NodeRef>,
    drop: rumble_widgets::DropEvent,
) {
    let Some(NodeRef::Room(source)) = id_map.get(&drop.source).copied() else {
        return;
    };
    let Some(NodeRef::Room(target)) = id_map.get(&drop.target).copied() else {
        return;
    };
    if source == target {
        return;
    }
    // The `MoveRoom` command takes a concrete `new_parent_id: Uuid` —
    // there's no representation for "no parent" / root. So Into → child
    // of target; Above/Below → sibling of target only when target has a
    // parent. Dropping next to a root room is a no-op for now.
    let new_parent = match drop.position {
        rumble_widgets::DropPosition::Into => Some(target),
        rumble_widgets::DropPosition::Above | rumble_widgets::DropPosition::Below => {
            state.room_tree.get(target).and_then(|n| n.parent_id)
        }
    };
    let Some(new_parent_id) = new_parent else {
        return;
    };
    if new_parent_id == source {
        return;
    }
    backend.send(Command::MoveRoom {
        room_id: source,
        new_parent_id,
    });
}

/// One row in the per-room ACL editor: group dropdown, here/subs
/// toggles, remove button, then a grant + deny matrix below.
fn acl_entry_row(
    ui: &mut Ui,
    idx: usize,
    entry: &mut RoomAclEntry,
    group_options: &[String],
    to_remove: &mut Option<usize>,
) {
    GroupBox::new(format!("Entry {}", idx + 1))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Group")
                        .color(ui.theme().tokens().text_muted)
                        .font(ui.theme().font(rumble_widgets::TextRole::Label)),
                );
                if !group_options.is_empty() {
                    let mut sel = group_options.iter().position(|g| g == &entry.group).unwrap_or(0);
                    let labels: Vec<String> = group_options.to_vec();
                    let before = sel;
                    ComboBox::new(format!("acl_entry_group_{idx}"), &mut sel, labels)
                        .width(160.0)
                        .show(ui);
                    if sel != before
                        && let Some(name) = group_options.get(sel)
                    {
                        entry.group = name.clone();
                    }
                } else {
                    ui.label(entry.group.clone());
                }
                ui.checkbox(&mut entry.apply_here, "Here");
                ui.checkbox(&mut entry.apply_subs, "Subs");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ButtonArgs::new("✖").role(PressableRole::Ghost).show(ui).clicked() {
                        *to_remove = Some(idx);
                    }
                });
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new("Grant")
                    .color(ui.theme().tokens().text_muted)
                    .font(ui.theme().font(rumble_widgets::TextRole::Label)),
            );
            permission_checkboxes_compact(ui, &mut entry.grant, idx, "grant");
            ui.add_space(4.0);
            ui.label(
                RichText::new("Deny")
                    .color(ui.theme().tokens().text_muted)
                    .font(ui.theme().font(rumble_widgets::TextRole::Label)),
            );
            permission_checkboxes_compact(ui, &mut entry.deny, idx, "deny");
        });
}

/// Inline grid of room-scoped permission checkboxes for an ACL entry's
/// grant or deny mask. Mirrors `rumble-egui::render_permission_checkboxes_compact`
/// — same flag set, same labels, same wrap behaviour.
pub(crate) fn permission_checkboxes_compact(ui: &mut Ui, bits: &mut u32, idx: usize, role: &str) {
    let perms: &[(Permissions, &str, &str)] = &[
        (Permissions::TRAVERSE, "Traverse", "See this room in the tree"),
        (Permissions::ENTER, "Enter", "Join this room"),
        (Permissions::SPEAK, "Speak", "Transmit voice"),
        (Permissions::TEXT_MESSAGE, "Text", "Send chat"),
        (Permissions::SHARE_FILE, "Files", "Share files"),
        (Permissions::MUTE_DEAFEN, "Mute", "Server-mute others"),
        (Permissions::MOVE_USER, "Move", "Move users"),
        (Permissions::MAKE_ROOM, "Mk Rm", "Create sub-rooms"),
        (Permissions::MODIFY_ROOM, "Mod Rm", "Modify rooms"),
        (Permissions::WRITE, "Edit ACL", "Edit ACL entries"),
    ];
    ui.horizontal_wrapped(|ui| {
        for (perm, label, hover) in perms {
            let mut on = Permissions::from_bits_truncate(*bits).contains(*perm);
            let _ = ui.push_id((idx, role, label), |ui| {
                if ui.checkbox(&mut on, *label).on_hover_text(*hover).changed() {
                    if on {
                        *bits |= perm.bits();
                    } else {
                        *bits &= !perm.bits();
                    }
                }
            });
        }
    });
}

fn modal_buttons(ui: &mut Ui, primary: &str, can_submit: bool, close: &mut bool, mut on_action: impl FnMut(bool)) {
    ui.horizontal(|ui| {
        if ButtonArgs::new("Cancel")
            .role(PressableRole::Default)
            .show(ui)
            .clicked()
        {
            on_action(false);
            *close = true;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ButtonArgs::new(primary)
                .role(PressableRole::Primary)
                .disabled(!can_submit)
                .show(ui)
                .clicked()
            {
                on_action(true);
            }
        });
    });
}
