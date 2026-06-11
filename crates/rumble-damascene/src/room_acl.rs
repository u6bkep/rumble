//! Per-room Permissions modal.
//!
//! Opened from the room context menu when the connected user has
//! `Permissions::WRITE` for the target room. Edits the room's
//! `inherit_acl` flag and `Vec<RoomAclEntry>` and saves through
//! `Command::SetRoomAcl`. Read-only sister surface to the Admin tab in
//! the Settings dialog — that tab edits per-group base permissions,
//! this modal edits how those base perms are sliced per room.
//!
//! Visual model (matches `reference/ACL wireframes/`):
//! - Title + breadcrumb of the room path (Root › Lobby › Gaming).
//! - `Inherit ACL` checkbox bound to `inherit_acl`.
//! - **Inherited from parent chain** — a collapsible block that lists
//!   read-only synthetic rules: first each group's base permissions
//!   (as `(base)` rows so the user can see *why* `default` has SPEAK
//!   without having to chase the Groups tab), then each ancestor
//!   room's ACL entries whose `apply_subs` flag is true.
//! - **Local rules** — one card per editable ACL entry on this room.
//!   The card header shows a scope pill (Here / Subtree / Here +
//!   Subtree) and a counts summary (`+3 −1`). Expanding the card
//!   reveals a row per room-scoped permission with a tristate
//!   `+` (allow) / `·` (inherit) / `−` (deny).
//! - "+ add group rule" appends a fresh, scope=Both, no-bits entry.
//! - Footer: explanatory tip + Cancel / Save buttons.

use damascene_core::prelude::*;

use rumble_client::{Command, State};
use rumble_protocol::{
    ROOT_ROOM_UUID,
    permissions::Permissions,
    proto::{GroupInfo, RoomAclEntry},
};
use uuid::Uuid;

// ============================================================
// Constants
// ============================================================

/// The 10 room-scoped permissions, in render order. Server-scoped
/// flags (KICK..SUDO) don't participate in per-room ACL editing — the
/// server evaluator strips them at non-root rooms anyway, and editing
/// them through this modal would only be sensible at root (where the
/// Admin tab's base-perms grid is the obvious entry point).
const ROOM_PERMS: &[(Permissions, &str, &str)] = &[
    (Permissions::TRAVERSE, "Traverse", "Walk through to children"),
    (Permissions::ENTER, "Enter", "Join this room"),
    (Permissions::SPEAK, "Speak", "Use voice here"),
    (Permissions::TEXT_MESSAGE, "Text", "Send chat messages"),
    (Permissions::SHARE_FILE, "Files", "Upload / share files"),
    (Permissions::MUTE_DEAFEN, "Mute/Deafen", "Mute or deafen others"),
    (Permissions::MOVE_USER, "Move users", "Move users between rooms"),
    (Permissions::MAKE_ROOM, "Make room", "Create sub-rooms"),
    (Permissions::MODIFY_ROOM, "Modify room", "Edit room settings"),
    (Permissions::WRITE, "Write ACL", "Edit this room's ACL"),
];

/// Bitmask covering every room-scoped permission. The `+ all` shortcut
/// flips this set in the grant column.
fn all_room_bits() -> u32 {
    ROOM_PERMS.iter().fold(0u32, |acc, (p, _, _)| acc | p.bits())
}

// ============================================================
// Scope tri-state
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclScope {
    /// Applies only to this room (`apply_here=true`, `apply_subs=false`).
    Here,
    /// Applies only to descendants (`apply_here=false`, `apply_subs=true`).
    Subtree,
    /// Applies here and to all descendants (`apply_here=true`, `apply_subs=true`).
    Both,
}

impl AclScope {
    /// Default for new entries — the most common case is "apply
    /// everywhere this entry would naturally fire", which matches the
    /// proto's `{apply_here=true, apply_subs=true}` default.
    const DEFAULT: AclScope = AclScope::Both;

    fn from_flags(apply_here: bool, apply_subs: bool) -> Self {
        match (apply_here, apply_subs) {
            (true, true) => AclScope::Both,
            (true, false) => AclScope::Here,
            (false, true) => AclScope::Subtree,
            // Illegal `{f, f}` falls back to Both so the entry stays
            // visible and the user can correct it.
            (false, false) => AclScope::Both,
        }
    }

    fn apply_here(self) -> bool {
        !matches!(self, AclScope::Subtree)
    }

    fn apply_subs(self) -> bool {
        !matches!(self, AclScope::Here)
    }

    fn label(self) -> &'static str {
        match self {
            AclScope::Here => "Here",
            AclScope::Subtree => "Subtree",
            AclScope::Both => "Here + Subtree",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            AclScope::Here => "here",
            AclScope::Subtree => "subtree",
            AclScope::Both => "both",
        }
    }

    fn from_slug(s: &str) -> Option<Self> {
        Some(match s {
            "here" => AclScope::Here,
            "subtree" => AclScope::Subtree,
            "both" => AclScope::Both,
            _ => return None,
        })
    }
}

/// Tri-state cell for one permission inside one entry. Maps to the
/// pair of grant/deny bitmasks the proto carries on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellState {
    Allow,
    Inherit,
    Deny,
}

impl CellState {
    fn slug(self) -> &'static str {
        match self {
            CellState::Allow => "allow",
            CellState::Inherit => "inherit",
            CellState::Deny => "deny",
        }
    }

    fn from_slug(s: &str) -> Option<Self> {
        Some(match s {
            "allow" => CellState::Allow,
            "inherit" => CellState::Inherit,
            "deny" => CellState::Deny,
            _ => return None,
        })
    }
}

// ============================================================
// State
// ============================================================

/// Editable view of a `RoomAclEntry` while the modal is open.
#[derive(Debug, Clone)]
pub struct AclEditEntry {
    pub group: String,
    pub grant: u32,
    pub deny: u32,
    pub scope: AclScope,
}

impl AclEditEntry {
    fn from_proto(entry: &RoomAclEntry) -> Self {
        Self {
            group: entry.group.clone(),
            grant: entry.grant,
            deny: entry.deny,
            scope: AclScope::from_flags(entry.apply_here, entry.apply_subs),
        }
    }

    fn to_proto(&self) -> RoomAclEntry {
        RoomAclEntry {
            group: self.group.clone(),
            grant: self.grant,
            deny: self.deny,
            apply_here: self.scope.apply_here(),
            apply_subs: self.scope.apply_subs(),
        }
    }

    fn cell_state(&self, perm: Permissions) -> CellState {
        if self.deny & perm.bits() != 0 {
            CellState::Deny
        } else if self.grant & perm.bits() != 0 {
            CellState::Allow
        } else {
            CellState::Inherit
        }
    }

    fn set_cell(&mut self, perm: Permissions, state: CellState) {
        let bit = perm.bits();
        match state {
            CellState::Allow => {
                self.grant |= bit;
                self.deny &= !bit;
            }
            CellState::Inherit => {
                self.grant &= !bit;
                self.deny &= !bit;
            }
            CellState::Deny => {
                self.grant &= !bit;
                self.deny |= bit;
            }
        }
    }

    /// `(allow_count, deny_count)` for the header summary. `allow_count`
    /// peaks at `ROOM_PERMS.len()`; we render that as the "+ all"
    /// shorthand instead of the digit.
    fn counts(&self) -> (u32, u32) {
        let mask = all_room_bits();
        ((self.grant & mask).count_ones(), (self.deny & mask).count_ones())
    }
}

#[derive(Debug)]
pub struct RoomAclModalState {
    pub room_id: Uuid,
    pub room_name: String,
    /// Breadcrumb path from root down to this room (inclusive). Each
    /// element is `(uuid, name)` so we don't re-walk the room tree on
    /// every frame; computed at open time and held until close.
    pub path: Vec<(Uuid, String)>,
    pub inherit_acl: bool,
    pub entries: Vec<AclEditEntry>,
    /// Local-rules accordion: at most one card expanded at a time.
    /// Indices refer to `entries`.
    pub expanded_entry: Option<usize>,
    /// Inherited block disclosure — default open since "why does this
    /// user have SPEAK here" is the typical question.
    pub inherited_open: bool,
    /// `+ add group rule` popover state, holds the current filter
    /// string. `None` when the popover is closed.
    pub add_rule_query: Option<String>,
    /// Optimistic-concurrency token: [`rumble_protocol::room_acl_version`]
    /// of the `(inherit_acl, acls)` snapshot this modal copied at open.
    /// Echoed in `Command::SetRoomAcl` so the server rejects the save if
    /// another admin changed the room's ACL while the modal was open,
    /// instead of silently clobbering their edit (#38). The rejection
    /// surfaces as an error toast; reopening the modal loads fresh rules.
    pub base_version: u64,
}

impl RoomAclModalState {
    /// Build modal state from a fresh snapshot. Falls back gracefully
    /// if the room ID has gone stale between open and snapshot — the
    /// caller is expected to handle the empty-room degenerate case by
    /// not opening the modal in the first place, but defending here
    /// keeps the type total.
    pub fn open_for(app_state: &State, room_id: Uuid) -> Option<Self> {
        let room = app_state.get_room(room_id)?;
        let room_name = room.name.clone();

        // Walk parent-to-root and reverse to get root-down for the
        // breadcrumb. Then append self.
        let mut path: Vec<(Uuid, String)> = app_state
            .room_tree
            .ancestors(room_id)
            .filter_map(|uuid| app_state.get_room(uuid).map(|r| (uuid, r.name.clone())))
            .collect();
        path.reverse();
        path.push((room_id, room_name.clone()));

        let entries: Vec<AclEditEntry> = room.acls.iter().map(AclEditEntry::from_proto).collect();

        // Version of the exact configuration we're copying into the editor —
        // the server compares the save against this base.
        let base_version = rumble_protocol::room_acl_version(room.inherit_acl, &room.acls);

        Some(RoomAclModalState {
            room_id,
            room_name,
            path,
            inherit_acl: room.inherit_acl,
            entries,
            expanded_entry: None,
            inherited_open: true,
            add_rule_query: None,
            base_version,
        })
    }

    /// True for the root room. The "Inherit ACL from" checkbox is
    /// suppressed in that case because there's no parent to inherit
    /// from.
    pub fn is_root(&self) -> bool {
        self.room_id == ROOT_ROOM_UUID
    }

    /// Build a `Command::SetRoomAcl` from the current pending state.
    pub fn to_command(&self) -> Command {
        Command::SetRoomAcl {
            room_id: self.room_id,
            inherit_acl: self.inherit_acl,
            entries: self.entries.iter().map(AclEditEntry::to_proto).collect(),
            base_version: self.base_version,
        }
    }
}

// ============================================================
// Outcome
// ============================================================

pub enum RoomAclOutcome {
    Ignored,
    Handled,
    Close,
    /// Save + close. The carried command is `Command::SetRoomAcl`.
    Save(Command),
}

// ============================================================
// Routed-key constants
// ============================================================

const KEY_DISMISS: &str = "room_acl:dismiss";
const KEY_CLOSE: &str = "room_acl:close";
const KEY_CANCEL: &str = "room_acl:cancel";
const KEY_SAVE: &str = "room_acl:save";
const KEY_INHERIT_TOGGLE: &str = "room_acl:inherit";
const KEY_INHERITED_TOGGLE: &str = "room_acl:inherited:toggle";
const KEY_ADD_RULE_OPEN: &str = "room_acl:addrule:open";
const KEY_ADD_RULE_DISMISS: &str = "room_acl:addrule:dismiss";

fn key_entry_toggle(idx: usize) -> String {
    format!("room_acl:e:{idx}:toggle")
}
fn key_entry_remove(idx: usize) -> String {
    format!("room_acl:e:{idx}:remove")
}
fn key_entry_scope_tabs(idx: usize) -> String {
    format!("room_acl:e:{idx}:scope")
}
fn key_entry_cell(idx: usize, bit: u32, value: CellState) -> String {
    format!("room_acl:e:{idx}:p:{bit:08x}:{}", value.slug())
}
fn key_add_rule_pick(group: &str) -> String {
    format!("room_acl:addrule:pick:{group}")
}

/// Parse `room_acl:e:<idx>:<action>[:<payload>]`. Returns
/// `(idx, action, payload)`. `payload` is the unparsed remainder when
/// present.
fn parse_entry_route(route: &str) -> Option<(usize, &str, Option<&str>)> {
    let rest = route.strip_prefix("room_acl:e:")?;
    let (idx_str, tail) = rest.split_once(':')?;
    let idx: usize = idx_str.parse().ok()?;
    let (action, payload) = match tail.split_once(':') {
        Some((a, p)) => (a, Some(p)),
        None => (tail, None),
    };
    Some((idx, action, payload))
}

// ============================================================
// Public render API
// ============================================================

#[derive(Default)]
pub struct RoomAclLayers {
    pub modal: Option<El>,
    pub popover: Option<El>,
}

impl RoomAclLayers {
    pub fn any(&self) -> bool {
        self.modal.is_some() || self.popover.is_some()
    }

    /// Drop both layers into the caller's overlay stack.
    pub fn into_layers(self) -> [Option<El>; 2] {
        [self.modal, self.popover]
    }
}

pub fn render(state: &RoomAclModalState, app_state: &State, selection: &Selection) -> RoomAclLayers {
    RoomAclLayers {
        modal: Some(render_modal(state, app_state, selection)),
        popover: state
            .add_rule_query
            .as_ref()
            .map(|q| render_add_rule_popover(state, app_state, q, selection)),
    }
}

fn render_modal(state: &RoomAclModalState, app_state: &State, _selection: &Selection) -> El {
    let header = render_header(state);
    let sub = render_subheader(state);
    let inherited = render_inherited_block(state, app_state);
    let local = render_local_rules(state);
    let add_rule = button("+ add group rule").key(KEY_ADD_RULE_OPEN).secondary();
    let footer = render_footer();

    let body = column([inherited, local, add_rule])
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0));

    let panel = modal_panel(
        "Permissions",
        [
            header,
            sub,
            scroll([column([body])
                .padding(Sides::xy(tokens::SPACE_3, 0.0))
                .width(Size::Fill(1.0))])
            .padding(Sides::xy(0.0, tokens::SPACE_2))
            .gap(tokens::SPACE_3)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0)),
            divider(),
            footer,
        ],
    )
    .width(Size::Fixed(720.0))
    .height(Size::Fixed(640.0));

    overlay([scrim(KEY_DISMISS), panel.block_pointer()])
}

// ---- Header / breadcrumb ------------------------------------------

fn render_header(state: &RoomAclModalState) -> El {
    let mut crumbs: Vec<El> = Vec::new();
    let last = state.path.len().saturating_sub(1);
    for (i, (_, name)) in state.path.iter().enumerate() {
        if i > 0 {
            crumbs.push(breadcrumb_separator());
        }
        let display = if name.is_empty() && i == 0 {
            "Root".to_string()
        } else {
            name.clone()
        };
        let crumb_el: El = if i == last {
            breadcrumb_page(display)
        } else {
            breadcrumb_link(display)
        };
        crumbs.push(breadcrumb_item(crumb_el));
    }

    let scope_tag = if state.is_root() {
        badge("root")
    } else {
        badge("sub-room")
    };

    column([
        row([text("Permissions").semibold(), scope_tag])
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .width(Size::Fill(1.0)),
        breadcrumb([breadcrumb_list(crumbs)]),
    ])
    .gap(tokens::SPACE_1)
    .width(Size::Fill(1.0))
}

// ---- Sub-header — inherit checkbox ---------------------------------

fn render_subheader(state: &RoomAclModalState) -> El {
    if state.is_root() {
        return paragraph("This is the root room — there's no parent to inherit from.")
            .muted()
            .font_size(tokens::TEXT_XS.size);
    }

    let parent_path: String = state
        .path
        .iter()
        .take(state.path.len().saturating_sub(1))
        .map(|(_, n)| if n.is_empty() { "Root".to_string() } else { n.clone() })
        .collect::<Vec<_>>()
        .join(" / ");

    row([
        checkbox(state.inherit_acl).key(KEY_INHERIT_TOGGLE),
        text("Inherit ACL from "),
        mono(format!("/{parent_path}")).font_size(tokens::TEXT_XS.size),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

// ---- Inherited rules block -----------------------------------------

fn render_inherited_block(state: &RoomAclModalState, app_state: &State) -> El {
    let synth = collect_inherited_rules(state, app_state);

    let header = row([
        icon(if state.inherited_open {
            "chevron-down"
        } else {
            "chevron-right"
        })
        .icon_size(tokens::ICON_XS)
        .color(tokens::MUTED_FOREGROUND),
        mono("inherited from parent chain").font_size(tokens::TEXT_XS.size),
        spacer(),
        text(format!(
            "{} rule{}",
            synth.len(),
            if synth.len() == 1 { "" } else { "s" }
        ))
        .muted()
        .font_size(tokens::TEXT_XS.size),
    ])
    .key(KEY_INHERITED_TOGGLE)
    .focusable()
    .cursor(Cursor::Pointer)
    .gap(tokens::SPACE_2)
    .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_1))
    .fill(tokens::MUTED)
    .radius(tokens::RADIUS_SM)
    .align(Align::Center)
    .width(Size::Fill(1.0));

    let mut children: Vec<El> = vec![header];
    if state.inherited_open {
        let rows: Vec<El> = synth.iter().map(render_inherited_row).collect();
        let body: El = if rows.is_empty() {
            paragraph("Nothing inherited yet — group base permissions only fire at the root.")
                .muted()
                .font_size(tokens::TEXT_XS.size)
        } else {
            column(rows).gap(tokens::SPACE_1).width(Size::Fill(1.0))
        };
        children.push(body);
    }

    column(children).gap(tokens::SPACE_2).width(Size::Fill(1.0))
}

/// Synthetic row describing one inherited rule. Either from a group's
/// base bitmask (`from == "(base)"`) or from an ancestor room's ACL
/// entry (`from == "/Lobby/Gaming"`).
struct InheritedRule {
    from: String,
    group: String,
    grant: u32,
    deny: u32,
    scope: AclScope,
}

fn collect_inherited_rules(state: &RoomAclModalState, app_state: &State) -> Vec<InheritedRule> {
    let mut out: Vec<InheritedRule> = Vec::new();

    // 1. Synthetic (base) rows — one per group definition. We surface
    //    these even at the root so the user always sees where base
    //    perms live.
    for g in &app_state.group_definitions {
        // Hide groups whose base set is empty — they'd be noise.
        if g.permissions == 0 {
            continue;
        }
        out.push(InheritedRule {
            from: "(base)".into(),
            group: g.name.clone(),
            grant: g.permissions,
            deny: 0,
            scope: AclScope::Both,
        });
    }

    // 2. Ancestor-room ACL entries with apply_subs=true. We skip
    //    self — the entries on the current room aren't "inherited",
    //    they're rendered as the editable local cards below.
    let last = state.path.len().saturating_sub(1);
    for (i, (_, name)) in state.path.iter().enumerate() {
        if i == last {
            break;
        }
        let path_up_to: String = state
            .path
            .iter()
            .take(i + 1)
            .map(|(_, n)| if n.is_empty() { "Root".to_string() } else { n.clone() })
            .collect::<Vec<_>>()
            .join("/");
        if let Some(room) = app_state.get_room(state.path[i].0) {
            for entry in &room.acls {
                if !entry.apply_subs {
                    continue;
                }
                out.push(InheritedRule {
                    from: format!("/{path_up_to}"),
                    group: entry.group.clone(),
                    grant: entry.grant,
                    deny: entry.deny,
                    scope: AclScope::from_flags(entry.apply_here, entry.apply_subs),
                });
            }
        }
        let _ = name;
    }

    out
}

fn render_inherited_row(rule: &InheritedRule) -> El {
    let summary = perm_summary(rule.grant, rule.deny);
    row([
        mono(rule.from.clone())
            .font_size(tokens::TEXT_XS.size)
            .width(Size::Fixed(130.0)),
        text(rule.group.clone()).semibold().width(Size::Fixed(140.0)),
        // Ellipsis on the perm summary — `default` (8 base bits)
        // alone exceeds the Fill column at typical modal widths.
        mono(summary)
            .font_size(tokens::TEXT_XS.size)
            .muted()
            .ellipsis()
            .width(Size::Fill(1.0)),
        scope_pill(rule.scope),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_1))
    .width(Size::Fill(1.0))
}

fn perm_summary(grant: u32, deny: u32) -> String {
    let mask = all_room_bits();
    let g = grant & mask;
    let d = deny & mask;
    if g == mask && d == 0 {
        return "+ all".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    for (perm, _label, _) in ROOM_PERMS {
        if g & perm.bits() != 0 {
            parts.push(format!("+{}", short_name(*perm)));
        }
    }
    for (perm, _label, _) in ROOM_PERMS {
        if d & perm.bits() != 0 {
            parts.push(format!("−{}", short_name(*perm)));
        }
    }
    if parts.is_empty() {
        "—".into()
    } else {
        parts.join(" ")
    }
}

fn short_name(perm: Permissions) -> &'static str {
    match perm {
        Permissions::TRAVERSE => "TRAV",
        Permissions::ENTER => "ENT",
        Permissions::SPEAK => "SPK",
        Permissions::TEXT_MESSAGE => "TXT",
        Permissions::SHARE_FILE => "FILE",
        Permissions::MUTE_DEAFEN => "MUTE",
        Permissions::MOVE_USER => "MOVE",
        Permissions::MAKE_ROOM => "MAKE",
        Permissions::MODIFY_ROOM => "MOD",
        Permissions::WRITE => "WRITE",
        _ => "?",
    }
}

fn scope_pill(scope: AclScope) -> El {
    badge(scope.label())
}

// ---- Local rules ---------------------------------------------------

fn render_local_rules(state: &RoomAclModalState) -> El {
    if state.entries.is_empty() {
        return paragraph("No local overrides. Inherited rules above apply unchanged.")
            .muted()
            .font_size(tokens::TEXT_XS.size);
    }

    let cards: Vec<El> = state
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| render_entry_card(i, e, state.expanded_entry == Some(i)))
        .collect();

    column(cards).gap(tokens::SPACE_2).width(Size::Fill(1.0))
}

fn render_entry_card(idx: usize, entry: &AclEditEntry, open: bool) -> El {
    let header = render_entry_header(idx, entry, open);
    let mut children: Vec<El> = vec![header];
    if open {
        let perm_rows: Vec<El> = ROOM_PERMS
            .iter()
            .map(|(p, label, desc)| render_perm_row(idx, *p, label, desc, entry.cell_state(*p)))
            .collect();
        children.push(
            column([
                column(perm_rows).gap(tokens::SPACE_1).width(Size::Fill(1.0)),
                row([spacer(), button("Remove rule").key(key_entry_remove(idx)).destructive()]).width(Size::Fill(1.0)),
            ])
            .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_2))
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0)),
        );
    }
    column(children).gap(0.0).width(Size::Fill(1.0))
}

fn render_entry_header(idx: usize, entry: &AclEditEntry, open: bool) -> El {
    let chevron = if open { "chevron-down" } else { "chevron-right" };
    let (allow_count, deny_count) = entry.counts();
    let summary = if allow_count == 0 && deny_count == 0 {
        "— no overrides".to_string()
    } else {
        let mask = all_room_bits();
        let g = entry.grant & mask;
        let mut parts: Vec<String> = Vec::new();
        if g == mask {
            parts.push("+ all".into());
        } else if allow_count > 0 {
            parts.push(format!("+{allow_count}"));
        }
        if deny_count > 0 {
            parts.push(format!("−{deny_count}"));
        }
        parts.join(" ")
    };

    let scope_tabs = tabs_list(
        key_entry_scope_tabs(idx),
        &entry.scope.slug(),
        [
            (AclScope::Here.slug(), AclScope::Here.label()),
            (AclScope::Subtree.slug(), AclScope::Subtree.label()),
            (AclScope::Both.slug(), AclScope::Both.label()),
        ],
    )
    .width(Size::Fixed(400.0));

    row([
        icon(chevron).icon_size(tokens::ICON_XS).color(tokens::MUTED_FOREGROUND),
        text(entry.group.clone()).semibold().width(Size::Fixed(140.0)),
        text(summary)
            .muted()
            .font_size(tokens::TEXT_XS.size)
            .width(Size::Fill(1.0)),
        scope_tabs,
    ])
    .key(key_entry_toggle(idx))
    .focusable()
    .cursor(Cursor::Pointer)
    .gap(tokens::SPACE_3)
    .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_1))
    .fill(tokens::CARD)
    .radius(tokens::RADIUS_SM)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

fn render_perm_row(idx: usize, perm: Permissions, label: &str, description: &str, current: CellState) -> El {
    let tri = row([CellState::Allow, CellState::Inherit, CellState::Deny].map(|v| {
        let glyph = match v {
            CellState::Allow => "+",
            CellState::Inherit => "·",
            CellState::Deny => "−",
        };
        let mut b = button(glyph).key(key_entry_cell(idx, perm.bits(), v));
        if v == current {
            b = b.primary();
        } else {
            b = b.secondary();
        }
        b.width(Size::Fixed(32.0))
    }))
    .gap(tokens::SPACE_1);

    row([
        column([
            text(label.to_string()).font_size(tokens::TEXT_SM.size),
            text(description.to_string()).muted().font_size(tokens::TEXT_XS.size),
        ])
        .gap(0.0)
        .width(Size::Fill(1.0)),
        tri,
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

// ---- Footer --------------------------------------------------------

fn render_footer() -> El {
    let tip = row([
        mono("tip").font_size(tokens::TEXT_XS.size),
        // Ellipsis so the tip yields to the buttons when the dialog
        // is unusually narrow; "denies beat allows" is reinforced in
        // the inline cell coloring anyway.
        text("· Denies always beat allows in the same room; rules walk up the tree.")
            .muted()
            .font_size(tokens::TEXT_XS.size)
            .ellipsis()
            .width(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_1)
    .align(Align::Center)
    .width(Size::Fill(1.0));

    row([
        tip,
        button("Cancel").key(KEY_CANCEL),
        button("Save changes").key(KEY_SAVE).primary(),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

// ---- Add-rule popover ----------------------------------------------

fn render_add_rule_popover(state: &RoomAclModalState, app_state: &State, query: &str, _selection: &Selection) -> El {
    let q = query.trim().to_lowercase();
    // Show every group; users can re-add rules for groups they already
    // have an entry for if they want multiple-scope rules. That's
    // unusual but legal — `apply_here=t, apply_subs=f` plus
    // `apply_here=f, apply_subs=t` is a valid combo of two entries.
    let mut candidates: Vec<&GroupInfo> = app_state
        .group_definitions
        .iter()
        .filter(|g| q.is_empty() || g.name.to_lowercase().contains(&q))
        .collect();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));

    let items: Vec<El> = if candidates.is_empty() {
        vec![
            text(if app_state.group_definitions.is_empty() {
                "No groups defined yet"
            } else {
                "No match"
            })
            .muted()
            .font_size(tokens::TEXT_XS.size)
            .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_2)),
        ]
    } else {
        candidates
            .iter()
            .map(|g| menu_item(g.name.clone()).key(key_add_rule_pick(&g.name)))
            .collect()
    };

    let _ = state;
    let panel = popover_panel(items);
    popover("room_acl:addrule", Anchor::below_key(KEY_ADD_RULE_OPEN), panel)
}

// ============================================================
// Event handling
// ============================================================

pub fn handle_event(
    state: &mut RoomAclModalState,
    event: &UiEvent,
    app_state: &State,
    _selection: &mut Selection,
) -> RoomAclOutcome {
    // Dismiss (scrim click / Cancel / Close / Esc).
    if event.kind == UiEventKind::Escape
        || event.is_click_or_activate(KEY_CANCEL)
        || event.is_click_or_activate(KEY_CLOSE)
        || (event.is_route(KEY_DISMISS) && event.kind == UiEventKind::Click)
    {
        return RoomAclOutcome::Close;
    }

    // Save.
    if event.is_click_or_activate(KEY_SAVE) {
        return RoomAclOutcome::Save(state.to_command());
    }

    if !matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
        return RoomAclOutcome::Ignored;
    }
    let Some(route) = event.route() else {
        return RoomAclOutcome::Ignored;
    };

    match route {
        KEY_INHERIT_TOGGLE => {
            state.inherit_acl = !state.inherit_acl;
            return RoomAclOutcome::Handled;
        }
        KEY_INHERITED_TOGGLE => {
            state.inherited_open = !state.inherited_open;
            return RoomAclOutcome::Handled;
        }
        KEY_ADD_RULE_OPEN => {
            state.add_rule_query = Some(String::new());
            return RoomAclOutcome::Handled;
        }
        KEY_ADD_RULE_DISMISS => {
            state.add_rule_query = None;
            return RoomAclOutcome::Handled;
        }
        _ => {}
    }

    // Add-rule pick.
    if let Some(group) = route.strip_prefix("room_acl:addrule:pick:") {
        state.add_rule_query = None;
        state.entries.push(AclEditEntry {
            group: group.to_string(),
            grant: 0,
            deny: 0,
            scope: AclScope::DEFAULT,
        });
        state.expanded_entry = Some(state.entries.len() - 1);
        return RoomAclOutcome::Handled;
    }

    // Scope-tab events (room_acl:e:{i}:scope:tab:{slug}) must be
    // dispatched before the generic entry-route parser, which would
    // otherwise absorb them as action="scope" and discard without
    // mutating anything. apply_event only matches its own key prefix,
    // so no other entry route can be shadowed here.
    for (i, entry) in state.entries.iter_mut().enumerate() {
        if damascene_core::widgets::tabs::apply_event(
            &mut entry.scope,
            event,
            &key_entry_scope_tabs(i),
            AclScope::from_slug,
        ) {
            return RoomAclOutcome::Handled;
        }
    }

    // Per-entry events.
    if let Some((idx, action, payload)) = parse_entry_route(route) {
        return handle_entry_event(state, idx, action, payload);
    }

    let _ = app_state;
    RoomAclOutcome::Ignored
}

fn handle_entry_event(
    state: &mut RoomAclModalState,
    idx: usize,
    action: &str,
    payload: Option<&str>,
) -> RoomAclOutcome {
    match action {
        "toggle" => {
            state.expanded_entry = match state.expanded_entry {
                Some(i) if i == idx => None,
                _ => Some(idx),
            };
            RoomAclOutcome::Handled
        }
        "remove" => {
            if idx < state.entries.len() {
                state.entries.remove(idx);
                state.expanded_entry = match state.expanded_entry {
                    Some(i) if i == idx => None,
                    Some(i) if i > idx => Some(i - 1),
                    other => other,
                };
            }
            RoomAclOutcome::Handled
        }
        "p" => {
            // `p:<bit_hex>:<state_slug>`
            let Some(rest) = payload else {
                return RoomAclOutcome::Handled;
            };
            let Some((bit_str, state_slug)) = rest.split_once(':') else {
                return RoomAclOutcome::Handled;
            };
            let Ok(bit) = u32::from_str_radix(bit_str, 16) else {
                return RoomAclOutcome::Handled;
            };
            let Some(new) = CellState::from_slug(state_slug) else {
                return RoomAclOutcome::Handled;
            };
            let Some(entry) = state.entries.get_mut(idx) else {
                return RoomAclOutcome::Handled;
            };
            if let Some(perm) = Permissions::from_bits(bit) {
                entry.set_cell(perm, new);
            }
            RoomAclOutcome::Handled
        }
        "scope" => {
            // A bare :scope route (no :tab: suffix) was not claimed by
            // the tabs::apply_event loop; nothing to do.
            RoomAclOutcome::Handled
        }
        _ => RoomAclOutcome::Ignored,
    }
}

// ============================================================
// Permission helper — used by the room_tree context menu so it can
// gate the menu item on `WRITE` for the target room.
// ============================================================

/// True when the local user holds `WRITE` for `room_id` (per-room
/// permissions trump effective when present, matching the rest of the
/// UI).
pub fn can_edit_acl(app_state: &State, room_id: Uuid) -> bool {
    let bits = app_state
        .per_room_permissions
        .get(&room_id)
        .copied()
        .unwrap_or(app_state.effective_permissions);
    Permissions::from_bits_truncate(bits).contains(Permissions::WRITE)
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use damascene_core::{event::UiEvent, selection::Selection};
    use rumble_client::State;
    use uuid::Uuid;

    use super::*;

    fn minimal_modal(scope: AclScope) -> RoomAclModalState {
        RoomAclModalState {
            room_id: Uuid::nil(),
            room_name: String::new(),
            path: vec![(Uuid::nil(), String::new())],
            inherit_acl: true,
            entries: vec![AclEditEntry {
                group: "default".to_string(),
                grant: 0,
                deny: 0,
                scope,
            }],
            expanded_entry: Some(0),
            inherited_open: false,
            add_rule_query: None,
            base_version: 0,
        }
    }

    /// Scope-tab clicks must mutate the entry's scope. Previously the
    /// generic entry-route parser absorbed them as action="scope" before
    /// tabs::apply_event could run, leaving apply_here/apply_subs frozen.
    #[test]
    fn scope_tab_both_to_here() {
        let mut modal = minimal_modal(AclScope::Both);
        let event = UiEvent::synthetic_click("room_acl:e:0:scope:tab:here");
        let outcome = handle_event(&mut modal, &event, &State::default(), &mut Selection::default());
        assert!(matches!(outcome, RoomAclOutcome::Handled), "event must be consumed");
        assert_eq!(
            modal.entries[0].scope,
            AclScope::Here,
            "scope was not updated — route collision regression"
        );
    }

    #[test]
    fn scope_tab_both_to_subtree() {
        let mut modal = minimal_modal(AclScope::Both);
        let event = UiEvent::synthetic_click("room_acl:e:0:scope:tab:subtree");
        handle_event(&mut modal, &event, &State::default(), &mut Selection::default());
        assert_eq!(modal.entries[0].scope, AclScope::Subtree);
    }

    #[test]
    fn scope_tab_here_to_both() {
        let mut modal = minimal_modal(AclScope::Here);
        let event = UiEvent::synthetic_click("room_acl:e:0:scope:tab:both");
        handle_event(&mut modal, &event, &State::default(), &mut Selection::default());
        assert_eq!(modal.entries[0].scope, AclScope::Both);
    }

    /// A non-scope entry-route event (e.g. perm cell toggle) must still
    /// work after the reorder.
    #[test]
    fn perm_cell_toggle_still_dispatches() {
        let mut modal = minimal_modal(AclScope::Both);
        // Simulate clicking the "allow" cell for TRAVERSE (0x00000001).
        let event = UiEvent::synthetic_click("room_acl:e:0:p:00000001:allow");
        let outcome = handle_event(&mut modal, &event, &State::default(), &mut Selection::default());
        assert!(matches!(outcome, RoomAclOutcome::Handled));
        assert_ne!(modal.entries[0].grant & 0x1, 0, "TRAVERSE grant bit not set");
    }
}
