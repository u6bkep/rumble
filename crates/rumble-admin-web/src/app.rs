//! The admin application: a `damascene_core::App` projecting the login,
//! first-run bootstrap, and dashboard screens, with every UI action routed
//! to the REST [`api`](crate::api) client.
//!
//! State lives in plain fields; [`AdminApp::build`] is a pure projection of
//! the current phase + fetched snapshot, and [`AdminApp::on_event`] maps
//! widget events to either local edits (text inputs, tab switches) or fetch
//! calls. Async results arrive through the shared [`Inbox`] and are folded
//! into state in [`AdminApp::before_build`].

use damascene_core::prelude::*;

use rumble_web_types::{
    AclEntryDto, BootstrapRequest, CreateGroupRequest, CreateRoomRequest, GroupDto, RoomDto, SetRoomAclRequest, UserDto,
};

use crate::{
    api::{self, Inb},
    inbox::Msg,
};

// ============================================================
// Widget keys
// ============================================================

const KEY_LOGIN_PW: &str = "login:pw";
const KEY_LOGIN_SUBMIT: &str = "login:submit";

const KEY_BOOT_TOKEN: &str = "boot:token";
const KEY_BOOT_PW: &str = "boot:pw";
const KEY_BOOT_KEY: &str = "boot:key";
const KEY_BOOT_SUBMIT: &str = "boot:submit";

const KEY_REFRESH: &str = "refresh";
const KEY_LOGOUT: &str = "logout";

const KEY_TABS: &str = "dash:tabs"; // tabs_list namespace; triggers route `dash:tabs:tab:<slug>`

const KEY_ROOM_NAME: &str = "room:name";
const KEY_ROOM_DESC: &str = "room:desc";
const KEY_ROOM_CREATE: &str = "room:create";
const KEY_ROOM_DEL: &str = "room:del:"; // + uuid

const KEY_GROUP_NAME: &str = "group:name";
const KEY_GROUP_CREATE: &str = "group:create";
const KEY_GROUP_DEL: &str = "group:del:"; // + name
const KEY_GROUP_EDIT: &str = "group:edit:"; // + name

// group permission editor
const KEY_GE_PERM: &str = "ge:perm:"; // + 8-hex bit
const KEY_GE_ALL: &str = "ge:all";
const KEY_GE_CLEAR: &str = "ge:clear";
const KEY_GE_SAVE: &str = "ge:save";
const KEY_GE_CANCEL: &str = "ge:cancel";

// room ACL editor
const KEY_ROOM_ACL: &str = "room:acl:"; // + uuid
const KEY_AE_INHERIT: &str = "ae:inherit";
const KEY_AE_ADD: &str = "ae:add:"; // + group
const KEY_AE_SAVE: &str = "ae:save";
const KEY_AE_CANCEL: &str = "ae:cancel";

const KEY_USER_KICK: &str = "user:kick:"; // + id
const KEY_USER_BAN: &str = "user:ban:"; // + id
const KEY_USER_REG: &str = "user:reg:"; // + id

// ============================================================
// Permission flag tables (mirror rumble_protocol::permissions)
// ============================================================

/// `(bit, label, description)` for the room-scoped permission flags, in wire
/// order. Kept local so the wasm bundle doesn't pull in the full protocol
/// crate; the bits are stable wire constants. These are the flags editable in
/// a per-room ACL.
const ROOM_PERMS: &[(u32, &str, &str)] = &[
    (0x001, "Traverse", "Walk through to children"),
    (0x002, "Enter", "Join this room"),
    (0x004, "Speak", "Use voice here"),
    (0x008, "Text", "Send chat messages"),
    (0x010, "Files", "Upload / share files"),
    (0x020, "Mute/Deafen", "Mute or deafen others"),
    (0x040, "Move users", "Move users between rooms"),
    (0x080, "Make room", "Create sub-rooms"),
    (0x100, "Modify room", "Edit room settings"),
    (0x200, "Write ACL", "Edit this room's ACL"),
];

/// `(bit, label, description)` for the server-scoped permission flags. These
/// only make sense in a group's base permission set, not a per-room ACL.
const SERVER_PERMS: &[(u32, &str, &str)] = &[
    (0x10000, "Kick", "Disconnect users"),
    (0x20000, "Ban", "Ban users"),
    (0x40000, "Register", "Register users"),
    (0x80000, "Self-register", "Register own identity"),
    (0x100000, "Manage ACL", "Create groups & edit ACLs"),
    (0x200000, "Sudo", "Superuser elevation"),
    (0x800000, "Manage participants", "Control bridge participants"),
];

/// Bitmask covering every room-scoped permission.
fn all_room_bits() -> u32 {
    ROOM_PERMS.iter().fold(0u32, |acc, (b, _, _)| acc | b)
}

/// Bitmask covering every permission we expose (room + server scoped).
fn all_perm_bits() -> u32 {
    ROOM_PERMS
        .iter()
        .chain(SERVER_PERMS)
        .fold(0u32, |acc, (b, _, _)| acc | b)
}

/// Comma-joined labels of the bits set in `mask`, or `—` when empty.
fn perm_summary(mask: u32) -> String {
    let set: Vec<&str> = ROOM_PERMS
        .iter()
        .chain(SERVER_PERMS)
        .filter(|(bit, _, _)| mask & bit != 0)
        .map(|(_, label, _)| *label)
        .collect();
    if set.is_empty() {
        "—".to_string()
    } else {
        set.join(", ")
    }
}

// ============================================================
// Room-ACL editing model (mirrors rumble-damascene's room_acl.rs)
// ============================================================

/// How an ACL entry projects onto a room and its descendants — the
/// `(apply_here, apply_subs)` pair as a single tri-state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Here,
    Subtree,
    Both,
}

impl Scope {
    fn from_flags(apply_here: bool, apply_subs: bool) -> Self {
        match (apply_here, apply_subs) {
            (true, false) => Scope::Here,
            (false, true) => Scope::Subtree,
            // `{true,true}` and the illegal `{false,false}` both map to Both.
            _ => Scope::Both,
        }
    }
    fn apply_here(self) -> bool {
        !matches!(self, Scope::Subtree)
    }
    fn apply_subs(self) -> bool {
        !matches!(self, Scope::Here)
    }
    fn label(self) -> &'static str {
        match self {
            Scope::Here => "Here",
            Scope::Subtree => "Subtree",
            Scope::Both => "Here + Subtree",
        }
    }
    fn slug(self) -> &'static str {
        match self {
            Scope::Here => "here",
            Scope::Subtree => "subtree",
            Scope::Both => "both",
        }
    }
    fn from_slug(s: &str) -> Option<Self> {
        Some(match s {
            "here" => Scope::Here,
            "subtree" => Scope::Subtree,
            "both" => Scope::Both,
            _ => return None,
        })
    }
}

/// Tri-state of one permission within one ACL entry.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cell {
    Allow,
    Inherit,
    Deny,
}

impl Cell {
    fn slug(self) -> &'static str {
        match self {
            Cell::Allow => "allow",
            Cell::Inherit => "inherit",
            Cell::Deny => "deny",
        }
    }
    fn from_slug(s: &str) -> Option<Self> {
        Some(match s {
            "allow" => Cell::Allow,
            "inherit" => Cell::Inherit,
            "deny" => Cell::Deny,
            _ => return None,
        })
    }
    fn glyph(self) -> &'static str {
        match self {
            Cell::Allow => "+",
            Cell::Inherit => "·",
            Cell::Deny => "−",
        }
    }
}

/// One editable ACL entry — a `(group, grant, deny, scope)` tuple while the
/// editor is open. Mirrors `rumble_protocol::proto::RoomAclEntry`.
struct AclEntry {
    group: String,
    grant: u32,
    deny: u32,
    scope: Scope,
}

impl AclEntry {
    fn cell(&self, bit: u32) -> Cell {
        if self.deny & bit != 0 {
            Cell::Deny
        } else if self.grant & bit != 0 {
            Cell::Allow
        } else {
            Cell::Inherit
        }
    }
    fn set_cell(&mut self, bit: u32, state: Cell) {
        match state {
            Cell::Allow => {
                self.grant |= bit;
                self.deny &= !bit;
            }
            Cell::Inherit => {
                self.grant &= !bit;
                self.deny &= !bit;
            }
            Cell::Deny => {
                self.grant &= !bit;
                self.deny |= bit;
            }
        }
    }
}

/// In-progress group base-permission edit.
struct GroupEdit {
    name: String,
    is_builtin: bool,
    permissions: u32,
}

/// In-progress per-room ACL edit.
struct AclEdit {
    room_id: String,
    room_name: String,
    inherit_acl: bool,
    entries: Vec<AclEntry>,
    /// Index of the expanded entry card, at most one at a time.
    expanded: Option<usize>,
}

/// Parse a per-entry route `ae:e:<idx>:<action>[:<payload>]`.
fn parse_ae_route(route: &str) -> Option<(usize, &str, Option<&str>)> {
    let rest = route.strip_prefix("ae:e:")?;
    let (idx_str, tail) = rest.split_once(':')?;
    let idx: usize = idx_str.parse().ok()?;
    let (action, payload) = match tail.split_once(':') {
        Some((a, p)) => (a, Some(p)),
        None => (tail, None),
    };
    Some((idx, action, payload))
}

// ============================================================
// State
// ============================================================

#[derive(PartialEq, Eq, Clone, Copy)]
enum Phase {
    /// Initial session probe in flight.
    Loading,
    /// No sudo password set yet — show the first-run bootstrap form.
    Bootstrap,
    /// Awaiting the sudo password.
    Login,
    /// Authenticated.
    Dashboard,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Overview,
    Users,
    Rooms,
    Groups,
}

impl Tab {
    fn slug(self) -> &'static str {
        match self {
            Tab::Overview => "overview",
            Tab::Users => "users",
            Tab::Rooms => "rooms",
            Tab::Groups => "groups",
        }
    }
    fn from_slug(s: &str) -> Option<Self> {
        Some(match s {
            "overview" => Tab::Overview,
            "users" => Tab::Users,
            "rooms" => Tab::Rooms,
            "groups" => Tab::Groups,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Tab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

#[derive(Default)]
struct BootstrapForm {
    setup_token: String,
    sudo_password: String,
    admin_key: String,
}

pub struct AdminApp {
    inbox: Inb,
    selection: Selection,
    phase: Phase,
    tab: Tab,

    /// Last status line: `(message, is_error)`.
    status: Option<(String, bool)>,

    // form state
    login_password: String,
    bootstrap: BootstrapForm,
    room_name: String,
    room_desc: String,
    group_name: String,

    // open editors (overlay the Groups / Rooms tab content when set)
    editing_group: Option<GroupEdit>,
    editing_acl: Option<AclEdit>,

    // fetched data
    snapshot: Option<rumble_web_types::StateSnapshot>,
}

impl AdminApp {
    pub fn new(inbox: Inb) -> Self {
        // Probe the session immediately; the result drives the initial phase.
        api::fetch_session(inbox.clone());
        Self {
            inbox,
            selection: Selection::default(),
            phase: Phase::Loading,
            tab: Tab::Overview,
            status: None,
            login_password: String::new(),
            bootstrap: BootstrapForm::default(),
            room_name: String::new(),
            room_desc: String::new(),
            group_name: String::new(),
            editing_group: None,
            editing_acl: None,
            snapshot: None,
        }
    }

    fn enter_dashboard(&mut self) {
        self.phase = Phase::Dashboard;
        api::fetch_state(self.inbox.clone());
    }
}

/// Offline scene builders for the `lint` binary: construct the app in a fixed
/// screen state with no network round-trip, so damascene's lint pass can run
/// against each screen on the host. Host-only — the wasm build always drives
/// state through real fetches.
#[cfg(not(target_arch = "wasm32"))]
impl AdminApp {
    fn offline() -> Self {
        Self::new(std::rc::Rc::new(std::cell::RefCell::new(crate::inbox::Inbox::new())))
    }

    pub fn scene_login() -> Self {
        let mut app = Self::offline();
        app.phase = Phase::Login;
        app
    }

    pub fn scene_bootstrap() -> Self {
        let mut app = Self::offline();
        app.phase = Phase::Bootstrap;
        app
    }

    pub fn scene_dashboard(snapshot: rumble_web_types::StateSnapshot, tab_slug: &str) -> Self {
        let mut app = Self::offline();
        app.phase = Phase::Dashboard;
        app.tab = Tab::from_slug(tab_slug).unwrap_or(Tab::Overview);
        app.snapshot = Some(snapshot);
        app
    }

    pub fn scene_group_editor(snapshot: rumble_web_types::StateSnapshot, group: &str) -> Self {
        let mut app = Self::scene_dashboard(snapshot, "groups");
        app.open_group_editor(group);
        app
    }

    pub fn scene_acl_editor(snapshot: rumble_web_types::StateSnapshot, room_id: &str) -> Self {
        let mut app = Self::scene_dashboard(snapshot, "rooms");
        app.open_acl_editor(room_id);
        // Expand the first rule so the tri-state permission grid is rendered
        // (and linted), not just the collapsed entry headers.
        if let Some(ae) = app.editing_acl.as_mut()
            && !ae.entries.is_empty()
        {
            ae.expanded = Some(0);
        }
        app
    }
}

// ============================================================
// App impl
// ============================================================

impl App for AdminApp {
    fn before_build(&mut self) {
        let msgs = self.inbox.borrow_mut().drain();
        for msg in msgs {
            match msg {
                Msg::Session(info) => {
                    if self.phase == Phase::Loading {
                        if info.authenticated {
                            self.enter_dashboard();
                        } else if info.needs_bootstrap {
                            self.phase = Phase::Bootstrap;
                        } else {
                            self.phase = Phase::Login;
                        }
                    }
                }
                Msg::LoginOk => {
                    self.login_password.clear();
                    self.status = Some(("Logged in.".to_string(), false));
                    self.enter_dashboard();
                }
                Msg::Bootstrapped => {
                    self.bootstrap = BootstrapForm::default();
                    self.phase = Phase::Login;
                    self.status = Some(("Bootstrap complete — log in with the sudo password.".to_string(), false));
                }
                Msg::LoggedOut => {
                    self.phase = Phase::Login;
                    self.snapshot = None;
                    self.editing_group = None;
                    self.editing_acl = None;
                    self.status = Some(("Logged out.".to_string(), false));
                }
                Msg::State(snapshot) => {
                    self.snapshot = Some(snapshot);
                }
                Msg::ActionOk(message) => {
                    self.status = Some((message, false));
                }
                Msg::Error(reason) => {
                    self.status = Some((reason, true));
                }
            }
        }
    }

    fn build(&self, _cx: &BuildCx) -> El {
        let body = match self.phase {
            Phase::Loading => column([text("Connecting…").muted()]).align(Align::Center),
            Phase::Bootstrap => self.bootstrap_view(),
            Phase::Login => self.login_view(),
            Phase::Dashboard => self.dashboard_view(),
        };

        column([self.header(), self.status_line(), body])
            .fill_size()
            .gap(tokens::SPACE_4)
            .padding(tokens::SPACE_5)
            .fill(tokens::BACKGROUND)
    }

    fn selection(&self) -> Selection {
        self.selection.clone()
    }

    /// Match the desktop client (`rumble-damascene`): Radix slate + blue dark.
    /// Every token reference in this app resolves through this palette.
    fn theme(&self) -> Theme {
        Theme::radix_slate_blue_dark()
    }

    fn on_event(&mut self, event: UiEvent) {
        // Fold cross-leaf selection changes (clicks off a text field) into
        // our single selection slot.
        if event.kind == UiEventKind::SelectionChanged {
            self.selection = event.selection.clone().unwrap_or_default();
            return;
        }

        // Text-input events arrive keyed on `target_key`; route them to the
        // matching field so we never write into an unfocused input.
        if let Some(target) = event.target_key() {
            let target = target.to_string();
            if self.route_text_input(&target, &event) {
                return;
            }
        }

        if !matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
            return;
        }

        // Controlled segmented tabs: fold the trigger straight into `self.tab`,
        // closing any open editor when the tab actually changes.
        let prev_tab = self.tab;
        if tabs::apply_event(&mut self.tab, &event, KEY_TABS, Tab::from_slug) {
            if self.tab != prev_tab {
                self.editing_group = None;
                self.editing_acl = None;
            }
            return;
        }

        // The "inherit ACL" checkbox is a plain bool — let the widget fold it.
        if let Some(ae) = self.editing_acl.as_mut()
            && checkbox::apply_event(&mut ae.inherit_acl, &event, KEY_AE_INHERIT)
        {
            return;
        }

        // Everything else is a click / keyboard activation routed by key.
        if let Some(route) = event.route() {
            self.handle_click(route);
        }
    }
}

// ============================================================
// Event routing
// ============================================================

impl AdminApp {
    /// Apply a text-input event to the field owning `target`. Returns true if
    /// `target` named one of our inputs.
    fn route_text_input(&mut self, target: &str, event: &UiEvent) -> bool {
        let field: Option<(&mut String, bool)> = match target {
            KEY_LOGIN_PW => Some((&mut self.login_password, true)),
            KEY_BOOT_TOKEN => Some((&mut self.bootstrap.setup_token, false)),
            KEY_BOOT_PW => Some((&mut self.bootstrap.sudo_password, true)),
            KEY_BOOT_KEY => Some((&mut self.bootstrap.admin_key, false)),
            KEY_ROOM_NAME => Some((&mut self.room_name, false)),
            KEY_ROOM_DESC => Some((&mut self.room_desc, false)),
            KEY_GROUP_NAME => Some((&mut self.group_name, false)),
            _ => None,
        };
        let Some((value, password)) = field else {
            return false;
        };
        let opts = if password {
            TextInputOpts::default().password()
        } else {
            TextInputOpts::default()
        };
        text_input::apply_event_with(value, &mut self.selection, target, event, &opts);
        true
    }

    fn handle_click(&mut self, route: &str) {
        // An open editor consumes its own routes first.
        if self.handle_group_edit_click(route) {
            return;
        }
        if self.handle_acl_edit_click(route) {
            return;
        }
        match route {
            KEY_LOGIN_SUBMIT => {
                if !self.login_password.is_empty() {
                    api::login(self.inbox.clone(), self.login_password.clone());
                }
            }
            KEY_BOOT_SUBMIT => {
                let key = self.bootstrap.admin_key.trim();
                let req = BootstrapRequest {
                    setup_token: self.bootstrap.setup_token.trim().to_string(),
                    sudo_password: self.bootstrap.sudo_password.clone(),
                    admin_public_key_b64: (!key.is_empty()).then(|| key.to_string()),
                };
                api::bootstrap(self.inbox.clone(), req);
            }
            KEY_REFRESH => api::fetch_state(self.inbox.clone()),
            KEY_LOGOUT => api::logout(self.inbox.clone()),
            KEY_ROOM_CREATE => {
                let name = self.room_name.trim();
                if !name.is_empty() {
                    let desc = self.room_desc.trim();
                    let req = CreateRoomRequest {
                        name: name.to_string(),
                        parent_id: None,
                        description: (!desc.is_empty()).then(|| desc.to_string()),
                    };
                    api::create_room(self.inbox.clone(), req);
                    self.room_name.clear();
                    self.room_desc.clear();
                }
            }
            KEY_GROUP_CREATE => {
                let name = self.group_name.trim();
                if !name.is_empty() {
                    let req = CreateGroupRequest {
                        name: name.to_string(),
                        permissions: 0,
                    };
                    api::create_group(self.inbox.clone(), req);
                    self.group_name.clear();
                }
            }
            _ => {
                if let Some(id) = route.strip_prefix(KEY_ROOM_ACL) {
                    self.open_acl_editor(id);
                } else if let Some(id) = route.strip_prefix(KEY_ROOM_DEL) {
                    api::delete_room(self.inbox.clone(), id.to_string());
                } else if let Some(name) = route.strip_prefix(KEY_GROUP_EDIT) {
                    self.open_group_editor(name);
                } else if let Some(name) = route.strip_prefix(KEY_GROUP_DEL) {
                    api::delete_group(self.inbox.clone(), name.to_string());
                } else if let Some(id) = route.strip_prefix(KEY_USER_KICK).and_then(|s| s.parse::<u64>().ok()) {
                    api::kick_user(self.inbox.clone(), id, String::new());
                } else if let Some(id) = route.strip_prefix(KEY_USER_BAN).and_then(|s| s.parse::<u64>().ok()) {
                    api::ban_user(self.inbox.clone(), id, 0, String::new());
                } else if let Some(id) = route.strip_prefix(KEY_USER_REG).and_then(|s| s.parse::<u64>().ok()) {
                    api::register_user(self.inbox.clone(), id);
                }
            }
        }
    }

    // --- group permission editor ---

    fn open_group_editor(&mut self, name: &str) {
        if let Some(snap) = &self.snapshot
            && let Some(g) = snap.groups.iter().find(|g| g.name == name)
        {
            self.editing_group = Some(GroupEdit {
                name: g.name.clone(),
                is_builtin: g.is_builtin,
                permissions: g.permissions,
            });
            self.editing_acl = None;
        }
    }

    /// Route a click while the group editor is open. Returns true if consumed.
    fn handle_group_edit_click(&mut self, route: &str) -> bool {
        if self.editing_group.is_none() {
            return false;
        }
        match route {
            KEY_GE_CANCEL => {
                self.editing_group = None;
                return true;
            }
            KEY_GE_SAVE => {
                if let Some(ge) = self.editing_group.take() {
                    api::modify_group(self.inbox.clone(), ge.name, ge.permissions);
                }
                return true;
            }
            KEY_GE_ALL => {
                if let Some(ge) = self.editing_group.as_mut() {
                    ge.permissions = all_perm_bits();
                }
                return true;
            }
            KEY_GE_CLEAR => {
                if let Some(ge) = self.editing_group.as_mut() {
                    ge.permissions = 0;
                }
                return true;
            }
            _ => {}
        }
        if let Some(hex) = route.strip_prefix(KEY_GE_PERM)
            && let Ok(bit) = u32::from_str_radix(hex, 16)
        {
            if let Some(ge) = self.editing_group.as_mut() {
                ge.permissions ^= bit;
            }
            return true;
        }
        false
    }

    // --- room ACL editor ---

    fn open_acl_editor(&mut self, id: &str) {
        if let Some(snap) = &self.snapshot
            && let Some(r) = snap.rooms.iter().find(|r| r.id == id)
        {
            let entries = r
                .acls
                .iter()
                .map(|e| AclEntry {
                    group: e.group.clone(),
                    grant: e.grant,
                    deny: e.deny,
                    scope: Scope::from_flags(e.apply_here, e.apply_subs),
                })
                .collect();
            self.editing_acl = Some(AclEdit {
                room_id: r.id.clone(),
                room_name: r.name.clone(),
                inherit_acl: r.inherit_acl,
                entries,
                expanded: None,
            });
            self.editing_group = None;
        }
    }

    /// Route a click while the ACL editor is open. Returns true if consumed.
    fn handle_acl_edit_click(&mut self, route: &str) -> bool {
        if self.editing_acl.is_none() {
            return false;
        }
        match route {
            KEY_AE_CANCEL => {
                self.editing_acl = None;
                return true;
            }
            KEY_AE_SAVE => {
                if let Some(ae) = self.editing_acl.take() {
                    let entries: Vec<AclEntryDto> = ae
                        .entries
                        .iter()
                        .map(|e| AclEntryDto {
                            group: e.group.clone(),
                            grant: e.grant,
                            deny: e.deny,
                            apply_here: e.scope.apply_here(),
                            apply_subs: e.scope.apply_subs(),
                        })
                        .collect();
                    let req = SetRoomAclRequest {
                        inherit_acl: ae.inherit_acl,
                        entries,
                    };
                    api::set_room_acl(self.inbox.clone(), ae.room_id, req);
                }
                return true;
            }
            _ => {}
        }
        if let Some(group) = route.strip_prefix(KEY_AE_ADD) {
            if let Some(ae) = self.editing_acl.as_mut() {
                ae.entries.push(AclEntry {
                    group: group.to_string(),
                    grant: 0,
                    deny: 0,
                    scope: Scope::Both,
                });
                ae.expanded = Some(ae.entries.len() - 1);
            }
            return true;
        }
        if let Some((idx, action, payload)) = parse_ae_route(route) {
            if let Some(ae) = self.editing_acl.as_mut() {
                apply_ae_entry_action(ae, idx, action, payload);
            }
            return true;
        }
        false
    }
}

/// Mutate one ACL entry in response to a parsed `ae:e:*` route.
fn apply_ae_entry_action(ae: &mut AclEdit, idx: usize, action: &str, payload: Option<&str>) {
    match action {
        "toggle" => {
            ae.expanded = match ae.expanded {
                Some(i) if i == idx => None,
                _ => Some(idx),
            };
        }
        "remove" if idx < ae.entries.len() => {
            ae.entries.remove(idx);
            ae.expanded = match ae.expanded {
                Some(i) if i == idx => None,
                Some(i) if i > idx => Some(i - 1),
                other => other,
            };
        }
        "scope" => {
            if let Some(slug) = payload
                && let (Some(entry), Some(scope)) = (ae.entries.get_mut(idx), Scope::from_slug(slug))
            {
                entry.scope = scope;
            }
        }
        "p" => {
            // payload is `<8-hex bit>:<state slug>`
            if let Some(rest) = payload
                && let Some((hex, slug)) = rest.split_once(':')
                && let (Ok(bit), Some(state), Some(entry)) = (
                    u32::from_str_radix(hex, 16),
                    Cell::from_slug(slug),
                    ae.entries.get_mut(idx),
                )
            {
                entry.set_cell(bit, state);
            }
        }
        _ => {}
    }
}

// ============================================================
// Views
// ============================================================

impl AdminApp {
    fn header(&self) -> El {
        let mut actions: Vec<El> = vec![spacer()];
        if self.phase == Phase::Dashboard {
            actions.push(button("Refresh").key(KEY_REFRESH).secondary());
            actions.push(button("Log out").key(KEY_LOGOUT).ghost());
        }
        let mut children = vec![text("Rumble Admin").title()];
        children.extend(actions);
        row(children).align(Align::Center).gap(tokens::SPACE_3)
    }

    fn status_line(&self) -> El {
        match &self.status {
            Some((msg, is_err)) => {
                let line = text(msg.clone());
                let line = if *is_err { line.destructive() } else { line.success() };
                row([line]).padding(tokens::SPACE_1)
            }
            None => spacer().height(Size::Fixed(0.0)),
        }
    }

    fn login_view(&self) -> El {
        let pw = text_input_with(
            &self.login_password,
            &self.selection,
            KEY_LOGIN_PW,
            TextInputOpts::default().placeholder("Sudo password").password(),
        )
        .fill_width();
        let form = card_section(
            "Admin login",
            tokens::SPACE_3,
            [
                text("Sign in with the server's sudo password.")
                    .muted()
                    .wrap_text()
                    .fill_width(),
                pw,
                button("Log in").key(KEY_LOGIN_SUBMIT).primary(),
            ],
        )
        .max_width(420.0);
        centered(form)
    }

    fn bootstrap_view(&self) -> El {
        let token = labeled_input(
            "Setup token",
            text_input_with(
                &self.bootstrap.setup_token,
                &self.selection,
                KEY_BOOT_TOKEN,
                TextInputOpts::default().placeholder("From the server log"),
            ),
        );
        let pw = labeled_input(
            "Sudo password",
            text_input_with(
                &self.bootstrap.sudo_password,
                &self.selection,
                KEY_BOOT_PW,
                TextInputOpts::default()
                    .placeholder("Choose a sudo password")
                    .password(),
            ),
        );
        let key = labeled_input(
            "Admin public key (base64, optional)",
            text_input_with(
                &self.bootstrap.admin_key,
                &self.selection,
                KEY_BOOT_KEY,
                TextInputOpts::default().placeholder("Ed25519 public key to seed the admin group"),
            ),
        );
        let form = card_section(
            "First-run setup",
            tokens::SPACE_3,
            [
                // Keep to one line: a hug-height card measures wrapping text at
                // its intrinsic single-line height, so a body string that
                // reflows to two lines under-sizes the card (see the `lint`
                // binary). The "First-run setup" title supplies the rest.
                text("Enter the one-time setup token printed to the server log.")
                    .muted()
                    .wrap_text()
                    .fill_width(),
                token,
                pw,
                key,
                button("Complete setup").key(KEY_BOOT_SUBMIT).primary(),
            ],
        )
        .max_width(520.0);
        centered(form)
    }

    fn dashboard_view(&self) -> El {
        let tabs = tabs_list(
            KEY_TABS,
            &self.tab,
            [
                (Tab::Overview, "Overview"),
                (Tab::Users, "Users"),
                (Tab::Rooms, "Rooms"),
                (Tab::Groups, "Groups"),
            ],
        );

        let content = match self.snapshot.as_ref() {
            None => column([text("Loading server state…").muted()]),
            Some(snap) => match self.tab {
                Tab::Overview => overview_view(snap),
                Tab::Users => users_view(&snap.users),
                Tab::Rooms => self.rooms_view(&snap.rooms),
                Tab::Groups => self.groups_view(&snap.groups),
            },
        };

        column([tabs, scroll([content]).key("dash:body")])
            .fill_size()
            .gap(tokens::SPACE_4)
    }

    fn rooms_view(&self, rooms: &[RoomDto]) -> El {
        if let Some(ae) = &self.editing_acl {
            return self.acl_editor(ae);
        }
        let create = titled_card(
            "Create room",
            [row([
                text_input_with(
                    &self.room_name,
                    &self.selection,
                    KEY_ROOM_NAME,
                    TextInputOpts::default().placeholder("Room name"),
                )
                .fill_width(),
                text_input_with(
                    &self.room_desc,
                    &self.selection,
                    KEY_ROOM_DESC,
                    TextInputOpts::default().placeholder("Description (optional)"),
                )
                .fill_width(),
                button("Create").key(KEY_ROOM_CREATE).primary(),
            ])
            .gap(tokens::SPACE_2)
            .align(Align::Center)],
        )
        .gap(tokens::SPACE_3);

        let mut rows: Vec<El> = vec![table_row([
            table_head("Name"),
            table_head("Description"),
            table_head(""),
        ])];
        if rooms.is_empty() {
            rows.push(table_row([table_cell(text("No rooms.").muted())]));
        } else {
            for r in rooms {
                let actions = row([
                    button("Permissions")
                        .key(format!("{KEY_ROOM_ACL}{}", r.id))
                        .secondary()
                        .small(),
                    button("Delete")
                        .key(format!("{KEY_ROOM_DEL}{}", r.id))
                        .destructive()
                        .small(),
                ])
                .gap(tokens::SPACE_2);
                rows.push(table_row([
                    table_cell(text(r.name.clone())),
                    table_cell(text(r.description.clone().unwrap_or_default()).muted()),
                    table_cell(actions),
                ]));
            }
        }

        column([create, titled_card("Rooms", [table([table_body(rows)])])]).gap(tokens::SPACE_4)
    }

    fn groups_view(&self, groups: &[GroupDto]) -> El {
        if let Some(ge) = &self.editing_group {
            return self.group_editor(ge);
        }
        let create = card_section(
            "Create group",
            tokens::SPACE_3,
            [
                row([
                    text_input_with(
                        &self.group_name,
                        &self.selection,
                        KEY_GROUP_NAME,
                        TextInputOpts::default().placeholder("Group name"),
                    )
                    .fill_width(),
                    button("Create").key(KEY_GROUP_CREATE).primary(),
                ])
                .gap(tokens::SPACE_2)
                .align(Align::Center),
                text("New groups start with no permissions; grant them via per-room ACLs.")
                    .muted()
                    .wrap_text()
                    .fill_width(),
            ],
        );

        let mut rows: Vec<El> = vec![table_row([
            table_head("Group"),
            table_head("Permissions"),
            table_head(""),
        ])];
        for g in groups {
            let name_cell = if g.is_builtin {
                row([text(g.name.clone()).semibold(), badge("built-in")])
                    .gap(tokens::SPACE_2)
                    .align(Align::Center)
            } else {
                row([text(g.name.clone())]).align(Align::Center)
            };
            let mut buttons: Vec<El> = vec![
                button("Edit")
                    .key(format!("{KEY_GROUP_EDIT}{}", g.name))
                    .secondary()
                    .small(),
            ];
            if !g.is_builtin {
                buttons.push(
                    button("Delete")
                        .key(format!("{KEY_GROUP_DEL}{}", g.name))
                        .destructive()
                        .small(),
                );
            }
            rows.push(table_row([
                table_cell(name_cell),
                table_cell(text(perm_summary(g.permissions)).muted().small()),
                table_cell(row(buttons).gap(tokens::SPACE_2)),
            ]));
        }

        column([create, titled_card("Groups", [table([table_body(rows)])])]).gap(tokens::SPACE_4)
    }

    // --- group permission editor view ---

    fn group_editor(&self, ge: &GroupEdit) -> El {
        let all = all_perm_bits();
        let everything_on = ge.permissions & all == all;
        let toggle_all = button(if everything_on { "Clear all" } else { "+ all" })
            .key(if everything_on { KEY_GE_CLEAR } else { KEY_GE_ALL })
            .secondary()
            .small();

        let header = row([
            text(format!("Edit group: {}", ge.name)).semibold(),
            spacer(),
            toggle_all,
        ])
        .align(Align::Center)
        .gap(tokens::SPACE_2)
        .fill_width();

        let room_rows: Vec<El> = ROOM_PERMS
            .iter()
            .map(|(bit, label, desc)| perm_switch_row(*bit, label, desc, ge.permissions & bit != 0))
            .collect();
        let server_rows: Vec<El> = SERVER_PERMS
            .iter()
            .map(|(bit, label, desc)| perm_switch_row(*bit, label, desc, ge.permissions & bit != 0))
            .collect();

        let mut body: Vec<El> = vec![header];
        if ge.is_builtin {
            body.push(
                text("Built-in group — change its base permissions with care.")
                    .warning()
                    .small(),
            );
        }
        body.push(text("Room-scoped").muted().small());
        body.push(column(room_rows).gap(tokens::SPACE_1).fill_width());
        body.push(text("Server-scoped").muted().small());
        body.push(column(server_rows).gap(tokens::SPACE_1).fill_width());
        body.push(divider());
        body.push(
            row([
                spacer(),
                button("Cancel").key(KEY_GE_CANCEL).ghost(),
                button("Save").key(KEY_GE_SAVE).primary(),
            ])
            .gap(tokens::SPACE_2)
            .fill_width(),
        );

        card_section("Group permissions", tokens::SPACE_3, body)
    }

    // --- room ACL editor view ---

    fn acl_editor(&self, ae: &AclEdit) -> El {
        let header = row([text(format!("Permissions — {}", ae.room_name)).semibold(), spacer()])
            .align(Align::Center)
            .fill_width();

        let inherit = row([
            checkbox(ae.inherit_acl).key(KEY_AE_INHERIT),
            text("Inherit ACL from parent room"),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .fill_width();

        let rules: El = if ae.entries.is_empty() {
            text("No local rules — inherited permissions apply unchanged. Add a group rule below.")
                .muted()
                .small()
        } else {
            column(
                ae.entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| acl_entry_card(i, e, ae.expanded == Some(i)))
                    .collect::<Vec<_>>(),
            )
            .gap(tokens::SPACE_2)
            .fill_width()
        };

        // One "+ group" button per defined group, to append a rule.
        let groups = self.snapshot.as_ref().map(|s| s.groups.as_slice()).unwrap_or(&[]);
        let mut add_children: Vec<El> = vec![text("Add rule:").muted().small()];
        for g in groups {
            add_children.push(
                button(format!("+ {}", g.name))
                    .key(format!("{KEY_AE_ADD}{}", g.name))
                    .secondary()
                    .small(),
            );
        }
        let add_row = row(add_children).gap(tokens::SPACE_2).align(Align::Center).fill_width();

        let footer = row([
            text("Denies always beat allows; rules inherit down the room tree.")
                .muted()
                .small(),
            spacer(),
            button("Cancel").key(KEY_AE_CANCEL).ghost(),
            button("Save changes").key(KEY_AE_SAVE).primary(),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .fill_width();

        card_section(
            "Room permissions",
            tokens::SPACE_3,
            vec![header, inherit, divider(), rules, add_row, divider(), footer],
        )
    }
}

// ============================================================
// Free view helpers
// ============================================================

/// One permission row in the group editor: label + description + a switch.
fn perm_switch_row(bit: u32, label: &str, desc: &str, on: bool) -> El {
    row([
        column([text(label.to_string()).small(), text(desc.to_string()).muted().small()])
            .gap(0.0)
            .fill_width(),
        switch(on).key(format!("{KEY_GE_PERM}{bit:08x}")),
    ])
    .gap(tokens::SPACE_3)
    .align(Align::Center)
    .fill_width()
}

/// One ACL entry card: a clickable header with a counts summary + scope badge,
/// expanding to a scope selector and a tri-state grid over the room perms.
fn acl_entry_card(idx: usize, entry: &AclEntry, open: bool) -> El {
    let mask = all_room_bits();
    let allow = (entry.grant & mask).count_ones();
    let deny = (entry.deny & mask).count_ones();
    let summary = if allow == 0 && deny == 0 {
        "— no overrides".to_string()
    } else {
        let mut parts: Vec<String> = Vec::new();
        if entry.grant & mask == mask {
            parts.push("+ all".into());
        } else if allow > 0 {
            parts.push(format!("+{allow}"));
        }
        if deny > 0 {
            parts.push(format!("−{deny}"));
        }
        parts.join(" ")
    };
    let chevron = if open { "▾" } else { "▸" };

    let head = row([
        text(format!("{chevron}  {}", entry.group))
            .semibold()
            .width(Size::Fixed(180.0)),
        text(summary).muted().small().fill_width(),
        badge(entry.scope.label()),
    ])
    .key(format!("ae:e:{idx}:toggle"))
    .focusable()
    .cursor(Cursor::Pointer)
    .gap(tokens::SPACE_3)
    .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_2))
    .align(Align::Center)
    .fill_width();

    let mut children: Vec<El> = vec![head];
    if open {
        let perm_rows: Vec<El> = ROOM_PERMS
            .iter()
            .map(|(bit, label, desc)| acl_perm_row(idx, *bit, label, desc, entry.cell(*bit)))
            .collect();
        children.push(
            column([
                acl_scope_row(idx, entry.scope),
                column(perm_rows).gap(tokens::SPACE_1).fill_width(),
                row([
                    spacer(),
                    button("Remove rule")
                        .key(format!("ae:e:{idx}:remove"))
                        .destructive()
                        .small(),
                ])
                .fill_width(),
            ])
            .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_2))
            .gap(tokens::SPACE_2)
            .fill_width(),
        );
    }
    column(children).gap(0.0).fill_width()
}

/// The Here / Subtree / Both selector for one ACL entry.
fn acl_scope_row(idx: usize, current: Scope) -> El {
    row([Scope::Here, Scope::Subtree, Scope::Both].map(|s| {
        let b = button(s.label()).key(format!("ae:e:{idx}:scope:{}", s.slug()));
        if s == current {
            b.primary().small()
        } else {
            b.secondary().small()
        }
    }))
    .gap(tokens::SPACE_1)
}

/// One permission row in an ACL entry: label + description + a +/·/− tri-state.
fn acl_perm_row(idx: usize, bit: u32, label: &str, desc: &str, current: Cell) -> El {
    let tri = row([Cell::Allow, Cell::Inherit, Cell::Deny].map(|v| {
        let b = button(v.glyph())
            .key(format!("ae:e:{idx}:p:{bit:08x}:{}", v.slug()))
            .width(Size::Fixed(34.0));
        if v == current { b.primary() } else { b.secondary() }
    }))
    .gap(tokens::SPACE_1);

    row([
        column([text(label.to_string()).small(), text(desc.to_string()).muted().small()])
            .gap(0.0)
            .fill_width(),
        tri,
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .fill_width()
}

fn centered(child: El) -> El {
    row([child]).fill_width().justify(Justify::Center)
}

fn labeled_input(label: &str, input: El) -> El {
    form_item([form_label(label), form_control(input.fill_width())])
}

/// A `titled_card` whose body items are spaced by `gap`.
///
/// `titled_card(...).gap(g)` spaces only the title from the body block —
/// `card_content` carries no default gap, so the body items otherwise render
/// flush (overlapping focus rings / hit targets on adjacent controls). This
/// lays the body out in a gapped column so `gap` lands where intended.
fn card_section(title: &str, gap: f32, body: impl IntoIterator<Item = El>) -> El {
    titled_card(
        title,
        [column(body.into_iter().collect::<Vec<_>>()).gap(gap).fill_width()],
    )
}

fn overview_view(snap: &rumble_web_types::StateSnapshot) -> El {
    card_section(
        "Overview",
        tokens::SPACE_2,
        [
            stat_row("Connected clients", snap.client_count.to_string()),
            stat_row("Authenticated users", snap.users.len().to_string()),
            stat_row("Rooms", snap.rooms.len().to_string()),
            stat_row("Permission groups", snap.groups.len().to_string()),
        ],
    )
    .max_width(480.0)
}

fn stat_row(label: &str, value: String) -> El {
    row([text(label).muted(), spacer(), text(value).semibold()])
        .fill_width()
        .align(Align::Center)
}

fn users_view(users: &[UserDto]) -> El {
    let mut rows: Vec<El> = vec![table_row([
        table_head("User"),
        table_head("Status"),
        table_head("Groups"),
        table_head("Actions"),
    ])];

    if users.is_empty() {
        rows.push(table_row([table_cell(text("No users connected.").muted())]));
    } else {
        for u in users {
            let mut status: Vec<&str> = Vec::new();
            if u.server_muted {
                status.push("server-muted");
            }
            if u.is_muted {
                status.push("muted");
            }
            if u.is_deafened {
                status.push("deafened");
            }
            if u.is_elevated {
                status.push("elevated");
            }
            let status_text = if status.is_empty() {
                "active".to_string()
            } else {
                status.join(", ")
            };
            let groups_text = if u.groups.is_empty() {
                "—".to_string()
            } else {
                u.groups.join(", ")
            };
            let actions = row([
                button("Kick")
                    .key(format!("{KEY_USER_KICK}{}", u.user_id))
                    .secondary()
                    .small(),
                button("Ban")
                    .key(format!("{KEY_USER_BAN}{}", u.user_id))
                    .destructive()
                    .small(),
                button("Register")
                    .key(format!("{KEY_USER_REG}{}", u.user_id))
                    .ghost()
                    .small(),
            ])
            .gap(tokens::SPACE_2);

            rows.push(table_row([
                table_cell(text(u.username.clone()).semibold()),
                table_cell(text(status_text).muted().small()),
                table_cell(text(groups_text).muted().small()),
                table_cell(actions),
            ]));
        }
    }

    titled_card("Connected users", [table([table_body(rows)])])
}
