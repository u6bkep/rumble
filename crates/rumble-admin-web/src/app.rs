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

use rumble_web_types::{BootstrapRequest, CreateGroupRequest, CreateRoomRequest, GroupDto, RoomDto, UserDto};

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

const KEY_TAB_OVERVIEW: &str = "tab:overview";
const KEY_TAB_USERS: &str = "tab:users";
const KEY_TAB_ROOMS: &str = "tab:rooms";
const KEY_TAB_GROUPS: &str = "tab:groups";

const KEY_ROOM_NAME: &str = "room:name";
const KEY_ROOM_DESC: &str = "room:desc";
const KEY_ROOM_CREATE: &str = "room:create";
const KEY_ROOM_DEL: &str = "room:del:"; // + uuid

const KEY_GROUP_NAME: &str = "group:name";
const KEY_GROUP_CREATE: &str = "group:create";
const KEY_GROUP_DEL: &str = "group:del:"; // + name

const KEY_USER_KICK: &str = "user:kick:"; // + id
const KEY_USER_BAN: &str = "user:ban:"; // + id
const KEY_USER_REG: &str = "user:reg:"; // + id

// ============================================================
// Permission flag table (mirrors rumble_protocol::permissions)
// ============================================================

/// `(bit, short label)` for every permission flag, in wire order. Kept local
/// so the wasm bundle doesn't pull in the full protocol crate; the bits are
/// stable wire constants.
const PERMS: &[(u32, &str)] = &[
    (0x001, "traverse"),
    (0x002, "enter"),
    (0x004, "speak"),
    (0x008, "text"),
    (0x010, "file"),
    (0x020, "mute/deafen"),
    (0x040, "move"),
    (0x080, "make-room"),
    (0x100, "modify-room"),
    (0x200, "write-acl"),
    (0x10000, "kick"),
    (0x20000, "ban"),
    (0x40000, "register"),
    (0x80000, "self-register"),
    (0x100000, "manage-acl"),
    (0x200000, "sudo"),
    (0x800000, "manage-participants"),
];

/// Comma-joined names of the bits set in `mask`, or `—` when empty.
fn perm_summary(mask: u32) -> String {
    let set: Vec<&str> = PERMS
        .iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if set.is_empty() {
        "—".to_string()
    } else {
        set.join(", ")
    }
}

// ============================================================
// Colors
// ============================================================

const BG: Color = Color::srgb_token("admin-bg", 26, 27, 30, 255);
const COLOR_ERR: Color = Color::srgb_token("admin-err", 224, 92, 92, 255);
const COLOR_OK: Color = Color::srgb_token("admin-ok", 96, 200, 124, 255);

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
            snapshot: None,
        }
    }

    fn enter_dashboard(&mut self) {
        self.phase = Phase::Dashboard;
        api::fetch_state(self.inbox.clone());
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
            .fill(BG)
    }

    fn selection(&self) -> Selection {
        self.selection.clone()
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

        // Everything else is a click / keyboard activation routed by key.
        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
            && let Some(route) = event.route()
        {
            self.handle_click(&route.to_string());
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
            KEY_TAB_OVERVIEW => self.tab = Tab::Overview,
            KEY_TAB_USERS => self.tab = Tab::Users,
            KEY_TAB_ROOMS => self.tab = Tab::Rooms,
            KEY_TAB_GROUPS => self.tab = Tab::Groups,
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
                if let Some(id) = route.strip_prefix(KEY_ROOM_DEL) {
                    api::delete_room(self.inbox.clone(), id.to_string());
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
                let color = if *is_err { COLOR_ERR } else { COLOR_OK };
                row([text(msg.clone()).color(color)]).padding(tokens::SPACE_1)
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
        let form = titled_card(
            "Admin login",
            [
                text("Sign in with the server's sudo password.").muted(),
                pw,
                button("Log in").key(KEY_LOGIN_SUBMIT).primary(),
            ],
        )
        .max_width(420.0)
        .gap(tokens::SPACE_3);
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
        let form = titled_card(
            "First-run setup",
            [
                text(
                    "No sudo password is configured yet. Enter the one-time setup token printed to the server log to \
                     complete bootstrap.",
                )
                .muted(),
                token,
                pw,
                key,
                button("Complete setup").key(KEY_BOOT_SUBMIT).primary(),
            ],
        )
        .max_width(520.0)
        .gap(tokens::SPACE_3);
        centered(form)
    }

    fn dashboard_view(&self) -> El {
        let tabs = row([
            tab_button("Overview", KEY_TAB_OVERVIEW, self.tab == Tab::Overview),
            tab_button("Users", KEY_TAB_USERS, self.tab == Tab::Users),
            tab_button("Rooms", KEY_TAB_ROOMS, self.tab == Tab::Rooms),
            tab_button("Groups", KEY_TAB_GROUPS, self.tab == Tab::Groups),
        ])
        .gap(tokens::SPACE_2);

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
                rows.push(table_row([
                    table_cell(text(r.name.clone())),
                    table_cell(text(r.description.clone().unwrap_or_default()).muted()),
                    table_cell(
                        button("Delete")
                            .key(format!("{KEY_ROOM_DEL}{}", r.id))
                            .destructive()
                            .small(),
                    ),
                ]));
            }
        }

        column([create, titled_card("Rooms", [table([table_body(rows)])])]).gap(tokens::SPACE_4)
    }

    fn groups_view(&self, groups: &[GroupDto]) -> El {
        let create = titled_card(
            "Create group",
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
                text("New groups start with no permissions; grant them via per-room ACLs.").muted(),
            ],
        )
        .gap(tokens::SPACE_3);

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
            let action = if g.is_builtin {
                spacer().width(Size::Fixed(0.0))
            } else {
                button("Delete")
                    .key(format!("{KEY_GROUP_DEL}{}", g.name))
                    .destructive()
                    .small()
            };
            rows.push(table_row([
                table_cell(name_cell),
                table_cell(text(perm_summary(g.permissions)).muted().small()),
                table_cell(action),
            ]));
        }

        column([create, titled_card("Groups", [table([table_body(rows)])])]).gap(tokens::SPACE_4)
    }
}

// ============================================================
// Free view helpers
// ============================================================

fn centered(child: El) -> El {
    row([child]).fill_width().justify(Justify::Center)
}

fn labeled_input(label: &str, input: El) -> El {
    column([text(label).small().muted(), input.fill_width()]).gap(tokens::SPACE_1)
}

fn tab_button(label: &str, key: &str, active: bool) -> El {
    let b = button(label).key(key);
    if active { b.primary() } else { b.ghost() }
}

fn overview_view(snap: &rumble_web_types::StateSnapshot) -> El {
    titled_card(
        "Overview",
        [
            stat_row("Connected clients", snap.client_count.to_string()),
            stat_row("Authenticated users", snap.users.len().to_string()),
            stat_row("Rooms", snap.rooms.len().to_string()),
            stat_row("Permission groups", snap.groups.len().to_string()),
        ],
    )
    .max_width(480.0)
    .gap(tokens::SPACE_2)
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
