//! UI lint gate for the web admin app.
//!
//! Builds each admin screen against canned [`AdminApp`] state on the host and
//! runs damascene's lint pass (raw colors, overflow, weak focus, scrollbar
//! overlap, duplicate ids, …) over the rendered tree. Prints every finding and
//! exits non-zero if any scene has one, so it can run as a CI gate:
//!
//! ```bash
//! cargo run -p rumble-admin-web --bin lint
//! ```
//!
//! This is the lightweight half of damascene's bundle pipeline — it gates on
//! lint findings only and pins no golden draw-ops, so it never needs blessing
//! after an intended visual change. The whole projection renders natively (the
//! `api` transport is stubbed off-wasm), so no browser or GPU is involved.

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    std::process::exit(native::run());
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use damascene_core::prelude::*;
    use rumble_admin_web::AdminApp;
    use rumble_web_types::{AclEntryDto, GroupDto, RegisteredUserDto, RoomDto, StateSnapshot, UserDto};

    // Matches the real browser viewport (`damascene_web::VIEWPORT`) so layout —
    // and therefore overflow/scrollbar findings — reflects what users see.
    const VIEWPORT: Rect = Rect::new(0.0, 0.0, 1280.0, 800.0);

    /// A representative populated server state: built-in + custom groups, a
    /// nested room carrying a couple of ACL rules, and a few connected users in
    /// assorted states — enough to exercise every table, badge, and editor.
    fn sample_snapshot() -> StateSnapshot {
        StateSnapshot {
            client_count: 5,
            users: vec![
                UserDto {
                    user_id: 1,
                    username: "alice".into(),
                    room_id: Some("root".into()),
                    is_muted: false,
                    is_deafened: false,
                    server_muted: false,
                    is_elevated: true,
                    groups: vec!["admin".into()],
                },
                UserDto {
                    user_id: 2,
                    username: "bob".into(),
                    room_id: Some("lobby".into()),
                    is_muted: true,
                    is_deafened: false,
                    server_muted: true,
                    is_elevated: false,
                    groups: vec!["default".into(), "moderators".into()],
                },
                UserDto {
                    user_id: 3,
                    username: "carol".into(),
                    room_id: None,
                    is_muted: false,
                    is_deafened: true,
                    server_muted: false,
                    is_elevated: false,
                    groups: vec![],
                },
            ],
            rooms: vec![
                RoomDto {
                    id: "root".into(),
                    name: "Root".into(),
                    parent_id: None,
                    description: Some("Top-level lobby".into()),
                    inherit_acl: false,
                    acls: vec![AclEntryDto {
                        group: "default".into(),
                        grant: 0x001 | 0x002 | 0x004,
                        deny: 0x008,
                        apply_here: true,
                        apply_subs: true,
                    }],
                },
                RoomDto {
                    id: "lobby".into(),
                    name: "Lobby".into(),
                    parent_id: Some("root".into()),
                    description: None,
                    inherit_acl: true,
                    acls: vec![AclEntryDto {
                        group: "moderators".into(),
                        grant: 0x020 | 0x040,
                        deny: 0,
                        apply_here: true,
                        apply_subs: false,
                    }],
                },
            ],
            groups: vec![
                GroupDto {
                    name: "default".into(),
                    permissions: 0x001 | 0x002 | 0x004 | 0x008,
                    is_builtin: true,
                },
                GroupDto {
                    name: "admin".into(),
                    permissions: 0x00FF_FFFF,
                    is_builtin: true,
                },
                GroupDto {
                    name: "moderators".into(),
                    permissions: 0x020 | 0x040 | 0x10000,
                    is_builtin: false,
                },
            ],
            registered_users: vec![
                RegisteredUserDto {
                    public_key: "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIz".into(),
                    username: "alice".into(),
                    groups: vec!["admin".into()],
                    online: true,
                },
                RegisteredUserDto {
                    public_key: "MDEyMzQ1Njc4OUFCQ0RFRkdISUpLTE1OT1BRUlNU".into(),
                    username: "dave".into(),
                    groups: vec![],
                    online: false,
                },
            ],
        }
    }

    /// Every screen the lint gate covers, paired with the app state that drives
    /// it. Constructing fresh per scene keeps them independent.
    fn scenes() -> Vec<(&'static str, AdminApp)> {
        vec![
            ("login", AdminApp::scene_login()),
            ("bootstrap", AdminApp::scene_bootstrap()),
            (
                "dashboard_overview",
                AdminApp::scene_dashboard(sample_snapshot(), "overview"),
            ),
            ("dashboard_users", AdminApp::scene_dashboard(sample_snapshot(), "users")),
            ("dashboard_rooms", AdminApp::scene_dashboard(sample_snapshot(), "rooms")),
            (
                "dashboard_groups",
                AdminApp::scene_dashboard(sample_snapshot(), "groups"),
            ),
            ("group_editor", AdminApp::scene_group_editor(sample_snapshot(), "admin")),
            ("acl_editor", AdminApp::scene_acl_editor(sample_snapshot(), "root")),
            (
                "user_groups_editor",
                AdminApp::scene_user_groups_editor(sample_snapshot(), 0),
            ),
        ]
    }

    /// Render one scene and return its lint findings as formatted lines.
    fn lint_scene(app: &AdminApp) -> Vec<String> {
        let theme = app.theme();
        let cx = BuildCx::new(&theme);
        let mut tree = app.build(&cx);
        let bundle = render_bundle(&mut tree, VIEWPORT);
        bundle
            .lint
            .findings
            .iter()
            .map(|f| {
                let source = if f.source.line == 0 {
                    "<no-source>".to_string()
                } else {
                    format!("{}:{}", f.source.file, f.source.line)
                };
                format!("{:?} node={} {} :: {}", f.kind, f.node_id, source, f.message)
            })
            .collect()
    }

    pub fn run() -> i32 {
        // `LINT_DUMP=<scene>` prints that scene's layout tree and bails — a
        // diagnostic escape hatch when a finding needs the resolved geometry.
        if let Ok(want) = std::env::var("LINT_DUMP") {
            for (name, app) in scenes() {
                if name == want {
                    let theme = app.theme();
                    let cx = BuildCx::new(&theme);
                    let mut tree = app.build(&cx);
                    let bundle = render_bundle(&mut tree, VIEWPORT);
                    println!("{}", bundle.tree_dump);
                    return 0;
                }
            }
            eprintln!("unknown scene: {want}");
            return 2;
        }

        let mut total = 0usize;
        for (name, app) in scenes() {
            let findings = lint_scene(&app);
            if findings.is_empty() {
                println!("✓ {name}: no findings");
            } else {
                total += findings.len();
                println!("✗ {name}: {} finding(s)", findings.len());
                for line in findings {
                    println!("    {line}");
                }
            }
        }
        if total == 0 {
            println!("\nUI lint passed — no findings.");
            0
        } else {
            eprintln!("\nUI lint failed — {total} finding(s) across scenes.");
            1
        }
    }
}
