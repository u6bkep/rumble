//! GUI projection microbenchmarks: the per-frame `App::build` (El-tree
//! construction) and the build + `render_bundle` (measure/layout) passes.
//!
//! Run: `cargo bench -p rumble-damascene --bench projection`
//!
//! The chat log is unbounded (issue #16 phase 2), so these benches answer:
//! how does a frame scale with backlog size? Three groups:
//!
//! - `projection_build/<n>`: steady-state build over an n-message backlog.
//!   With the incremental `ChatHistory` row cache this should be ~flat in n
//!   (the per-frame `State` clone is an Arc bump; rows are cached).
//! - `projection_layout/<n>`: build + measure/layout. The damascene
//!   `virtual_list_dyn` walk is O(n) per frame upstream (row keys + height
//!   prefix sums), so this group tracks the library-side ceiling.
//! - `projection_append/<n>`: the frame that absorbs one newly-arrived
//!   message at backlog size n — the incremental extend path (epoch
//!   unchanged, length grown ⇒ only the tail row is built).
//!
//! This drives the REAL `RumbleApp::build` / `render_bundle` path (the same one
//! `dump_bundles` snapshots), against a `MockBackend` returning a canned
//! connected `State` with a parameterized number of chat messages. No GPU, no
//! window. The golden snapshots in `crates/rumble-damascene/goldens/` are the
//! behavior net for this path; this bench is the cost net.
//!
//! Baseline-gated workflow:
//!   cargo bench -p rumble-damascene --bench projection -- --save-baseline before
//!   # ... optimize ...
//!   cargo bench -p rumble-damascene --bench projection -- --baseline before

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group};
use damascene_core::prelude::*;
use rumble_client::{
    AudioState, AudioStats, ChatMessage, ChatMessageKind, ChatMessageVisibility, Command, ConnectionState,
    MeterSnapshot, State, VoiceMode,
};
use rumble_damascene::{Identity, RumbleApp, backend::UiBackend};
use rumble_desktop_shell::SettingsStore;
use rumble_protocol::{
    Uuid,
    permissions::Permissions,
    proto::{RoomInfo, User, UserId},
    room_id_from_uuid,
};

const ROOM_LOBBY: u128 = 0x1111_1111_1111_1111_1111_1111_1111_1111;
const ROOM_WORK: u128 = 0x2222_2222_2222_2222_2222_2222_2222_2222;

/// Returns a canned `State` and discards commands — the renderer only exercises
/// the read half of [`UiBackend`]. The state lives behind a shared lock so the
/// append bench can push messages between frames, mirroring how the projection
/// task mutates the real shared state.
struct MockBackend {
    state: std::sync::Arc<std::sync::RwLock<State>>,
}

impl UiBackend for MockBackend {
    fn state(&self) -> State {
        self.state.read().unwrap().clone()
    }
    fn send(&self, _command: Command) {}
    fn meter(&self) -> MeterSnapshot {
        MeterSnapshot::default()
    }
    fn stats(&self) -> AudioStats {
        AudioStats::default()
    }
    fn transfers(&self) -> Vec<rumble_client_traits::file_transfer::TransferStatus> {
        Vec::new()
    }
}

fn make_room(uuid: u128, name: &str) -> RoomInfo {
    RoomInfo {
        id: Some(room_id_from_uuid(Uuid::from_u128(uuid))),
        name: name.into(),
        parent_id: None,
        description: None,
        inherit_acl: false,
        acls: Vec::new(),
        effective_permissions: 0,
    }
}

fn make_user(id: u64, name: &str, room: u128) -> User {
    User {
        user_id: Some(UserId { value: id }),
        username: name.into(),
        current_room: Some(room_id_from_uuid(Uuid::from_u128(room))),
        is_muted: false,
        is_deafened: false,
        server_muted: false,
        is_elevated: false,
        groups: Vec::new(),
        label: None,
    }
}

/// One chat message with a distinct id derived from `seq` (wider than the
/// dump_bundles `u8` helper so a 1000-message backlog stays unique). The text
/// carries light markdown so the per-message parse cost is exercised.
fn make_chat(seq: u32, sender: &str) -> ChatMessage {
    let mut id = [0u8; 16];
    id[12..16].copy_from_slice(&seq.to_le_bytes());
    ChatMessage {
        id,
        sender: sender.into(),
        sender_id: None,
        text: format!("message **{seq}** from {sender} — see `docs/perf.md` for _details_"),
        timestamp: std::time::SystemTime::UNIX_EPOCH,
        kind: ChatMessageKind::Room,
        attachment: None,
        visibility: ChatMessageVisibility::Normal,
    }
}

/// Connected state mirroring the `connected` golden scene, but with `n_messages`
/// chat messages so the bench can scale the dominant O(messages) term.
fn connected_state_with_chat(n_messages: usize) -> State {
    let mut audio = AudioState {
        voice_mode: VoiceMode::Continuous,
        ..AudioState::default()
    };
    audio.talking_users.insert(2);

    let senders = ["alice", "bob", "charlie", "diana"];
    let chat_messages = std::sync::Arc::new(
        (0..n_messages as u32)
            .map(|seq| make_chat(seq, senders[seq as usize % senders.len()]))
            .collect::<Vec<_>>(),
    );

    let mut state = State {
        connection: ConnectionState::Connected {
            server_name: "rumble.example".into(),
            user_id: 1,
        },
        rooms: vec![make_room(ROOM_LOBBY, "Lobby"), make_room(ROOM_WORK, "Work")],
        users: vec![
            make_user(1, "alice", ROOM_LOBBY),
            make_user(2, "bob", ROOM_WORK),
            make_user(3, "charlie", ROOM_LOBBY),
            make_user(4, "diana", ROOM_WORK),
        ],
        my_user_id: Some(1),
        my_room_id: Some(Uuid::from_u128(ROOM_LOBBY)),
        audio,
        chat_messages,
        effective_permissions: Permissions::TEXT_MESSAGE.bits(),
        ..State::default()
    };
    state.rebuild_room_tree();
    state
}

/// Build a connected `RumbleApp` over `n_messages` of chat backlog, with the
/// first-run wizard suppressed so we render the main shell. The temp config dir
/// is kept alive by the returned guard (identity/settings persist to disk).
#[allow(clippy::type_complexity)]
fn make_app(
    n_messages: usize,
) -> (
    RumbleApp<MockBackend>,
    std::sync::Arc<std::sync::RwLock<State>>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let shared = std::sync::Arc::new(std::sync::RwLock::new(connected_state_with_chat(n_messages)));
    let backend = MockBackend { state: shared.clone() };
    let identity = Identity::load(dir.path().to_path_buf()).expect("identity");
    let settings = SettingsStore::load_from_path(Some(dir.path().join("settings.json")));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut app = RumbleApp::new(backend, identity, settings, runtime);
    app.suppress_first_run_for_test();
    (app, shared, dir)
}

fn bench_projection(c: &mut Criterion) {
    let viewport = Rect::new(0.0, 0.0, 1280.0, 800.0);

    let mut build = c.benchmark_group("projection_build");
    for &n in &[10usize, 100, 1000, 10_000, 100_000] {
        let (app, _shared, _dir) = make_app(n);
        let theme = app.theme();
        let cx = BuildCx::new(&theme);
        build.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| black_box(app.build(&cx)));
        });
    }
    build.finish();

    let mut layout = c.benchmark_group("projection_layout");
    for &n in &[10usize, 100, 1000, 10_000, 100_000] {
        let (app, _shared, _dir) = make_app(n);
        let theme = app.theme();
        let cx = BuildCx::new(&theme);
        layout.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let mut tree = app.build(&cx);
                black_box(render_bundle(&mut tree, viewport));
            });
        });
    }
    layout.finish();

    // The frame that absorbs one new message at backlog size n: push one
    // message, build. Exercises the incremental `ChatHistory` extend (same
    // epoch, length+1 ⇒ only the new row is built). To keep the workload
    // stationary while criterion iterates, the log is reset to its
    // n-message template (and the row cache re-warmed) every 1000 appends,
    // outside the timed region.
    let mut append = c.benchmark_group("projection_append");
    for &n in &[1000usize, 10_000, 100_000] {
        let (app, shared, _dir) = make_app(n);
        let theme = app.theme();
        let cx = BuildCx::new(&theme);
        let template = shared.read().unwrap().chat_messages.clone();
        // Warm the row cache so the first timed iteration is an append,
        // not the initial O(n) build.
        let _ = app.build(&cx);
        append.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            let mut next_seq = n as u32;
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    if next_seq >= n as u32 + 1000 {
                        // Untimed reset: fresh deep copy of the template
                        // (so subsequent pushes mutate in place without
                        // copy-on-write spikes), epoch bump to force the
                        // cache rebuild, then one untimed build to
                        // re-warm.
                        {
                            let mut s = shared.write().unwrap();
                            s.chat_messages = std::sync::Arc::new(template.as_ref().clone());
                            s.chat_epoch += 1;
                        }
                        let _ = app.build(&cx);
                        next_seq = n as u32;
                    }
                    {
                        let mut s = shared.write().unwrap();
                        let senders = ["alice", "bob", "charlie", "diana"];
                        let msg = make_chat(next_seq, senders[next_seq as usize % senders.len()]);
                        std::sync::Arc::make_mut(&mut s.chat_messages).push(msg);
                    }
                    next_seq += 1;
                    let start = std::time::Instant::now();
                    black_box(app.build(&cx));
                    total += start.elapsed();
                }
                total
            });
        });
    }
    append.finish();
}

criterion_group!(benches, bench_projection);

fn main() {
    // Each RumbleApp opens an XDG GlobalShortcuts portal session on Wayland;
    // built back-to-back (one per bench param) they back up xdg-desktop-portal
    // and hang teardown after the run completes. The projection bench only
    // needs the El-tree path, not global hotkeys, so opt out — same escape
    // hatch `dump_bundles` uses. Safety: set before any tokio runtime/thread
    // is built (all RumbleApp construction happens later, inside `benches()`).
    unsafe {
        std::env::set_var("RUMBLE_DISABLE_PORTAL", "1");
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
}
