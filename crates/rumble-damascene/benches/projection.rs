//! GUI projection microbenchmarks: the per-frame `App::build` (El-tree
//! construction) and the build + `render_bundle` (measure/layout) passes.
//!
//! Run: `cargo bench -p rumble-damascene --bench projection`
//!
//! The chat log is unbounded (issue #16 phase 2), so these benches answer:
//! how does a frame scale with backlog size? Groups (the `*_warm`, `scroll_*`,
//! and `resize` ones thread one persistent `UiState` across frames, which is
//! what lets the append-only virtual-list fast path reuse its height index):
//!
//! - `projection_build/<n>`: steady-state build over an n-message backlog.
//!   With the incremental `ChatHistory` row cache this should be ~flat in n
//!   (the per-frame `State` clone is an Arc bump; rows are cached).
//! - `projection_layout/<n>`: build + measure/layout with a fresh `UiState`
//!   each iteration — i.e. a COLD layout. The append-only height index can't
//!   help here (it has nothing to reuse), so this tracks the cold/epoch-change
//!   ceiling; with `.append_only()` it is slightly *slower* than the general
//!   path (the index build is extra work on a frame that's thrown away).
//! - `projection_layout_warm/<n>`: build + measure/layout threading one
//!   persistent `UiState` across frames (warmed by an untimed first frame) —
//!   the repainted-frame case the append-only fast path (#107) optimizes. With
//!   `.append_only()` this is ~flat O(visible) in n; without it the general
//!   path stays O(n) per frame even though `UiState` persists.
//! - `projection_scroll_end/<n>`: warm frames with the chat list pinned to the
//!   bottom — the stick-to-bottom case where the general path's visible-range
//!   scan walks from row 0 (O(n)) and append-only resolves it in O(√n).
//! - `projection_scroll_drag/<n>`: warm frames jumping the scroll offset across
//!   the whole range each frame (thumb-drag), stressing the √n block index at
//!   arbitrary positions and the offset-correction pass.
//! - `projection_resize/<n>`: warm frames alternating the viewport width, so
//!   every frame reflows + re-measures the realized rows.
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
use damascene_core::{UiState, layout::assign_ids, prelude::*};
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

/// Recursively find the `computed_id` of the first node carrying `key`
/// (populated by [`assign_ids`]). Used to seed a virtual list's scroll
/// offset before layout, since offsets are keyed by `computed_id`.
fn find_computed_id(el: &El, key: &str) -> Option<String> {
    if el.key.as_deref() == Some(key) {
        return Some(el.computed_id.to_string());
    }
    el.children.iter().find_map(|c| find_computed_id(c, key))
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

    // Warm steady-state layout: the same persistent `UiState` is threaded
    // through `render_bundle_with` across frames, so the chat list's
    // `virtual_list_dyn` keeps its per-list height index between frames. This
    // is the case the append-only fast path (#107) optimizes — a *repainted*
    // frame whose row sequence is unchanged. (`projection_layout` above builds
    // a fresh `UiState` per iteration, so it measures the COLD rebuild, where
    // append-only's persistent index can't help and the general path is the
    // ceiling.) `UiState` is warmed with one untimed frame before measuring so
    // the first cold rebuild is excluded; scroll sits at the default (top)
    // position — the from-end O(n)→O(√n) range-scan win is additional on top of
    // this and not isolated here.
    let mut layout_warm = c.benchmark_group("projection_layout_warm");
    for &n in &[10usize, 100, 1000, 10_000, 100_000] {
        let (app, _shared, _dir) = make_app(n);
        let theme = app.theme();
        let cx = BuildCx::new(&theme);
        let mut ui_state = UiState::new();
        // Untimed warm-up: build the persistent per-list height index once.
        {
            let mut tree = app.build(&cx);
            let _ = render_bundle_with(&mut tree, &mut ui_state, viewport);
        }
        layout_warm.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let mut tree = app.build(&cx);
                black_box(render_bundle_with(&mut tree, &mut ui_state, viewport));
            });
        });
    }
    layout_warm.finish();

    // Warm interaction frames: persistent `UiState`, with the scroll position
    // or viewport varied per frame to exercise the paths the append-only fast
    // path (#107) is meant to keep cheap. Each warms one untimed frame, then
    // every timed frame is a painted interaction frame. Scroll offsets are
    // keyed by the chat list's `computed_id` (resolved once via `assign_ids` +
    // `find_computed_id`); an over-large offset is clamped to the end on layout.
    // Sizes start at 1k — below the viewport's worth of rows there is nothing to
    // scroll and `max_offset` is 0.
    const CHAT_LIST_KEY: &str = rumble_damascene::chat::CHAT_HISTORY_KEY;

    // Stick-to-bottom: scroll pinned to the end every frame. This is the
    // worst case from the report — the general path's visible-range scan walks
    // from row 0 to the end (O(n)); append-only resolves it in O(√n).
    let mut scroll_end = c.benchmark_group("projection_scroll_end");
    for &n in &[1000usize, 10_000, 100_000] {
        let (app, _shared, _dir) = make_app(n);
        let theme = app.theme();
        let cx = BuildCx::new(&theme);
        let mut ui_state = UiState::new();
        let mut tree = app.build(&cx);
        assign_ids(&mut tree);
        let list_id = find_computed_id(&tree, CHAT_LIST_KEY).expect("chat history scroll node");
        ui_state.set_scroll_offset(list_id.as_str(), f32::MAX); // clamped to end on layout
        let _ = render_bundle_with(&mut tree, &mut ui_state, viewport);
        scroll_end.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let mut tree = app.build(&cx);
                black_box(render_bundle_with(&mut tree, &mut ui_state, viewport));
            });
        });
    }
    scroll_end.finish();

    // Thumb-drag: jump the offset to a different position across the full range
    // each frame — stresses the √n block index at arbitrary positions plus the
    // offset-correction pass, the case the maintainer flagged as the one that
    // might want a Fenwick tree if √n is short.
    let mut scroll_drag = c.benchmark_group("projection_scroll_drag");
    for &n in &[1000usize, 10_000, 100_000] {
        let (app, _shared, _dir) = make_app(n);
        let theme = app.theme();
        let cx = BuildCx::new(&theme);
        let mut ui_state = UiState::new();
        let mut tree = app.build(&cx);
        assign_ids(&mut tree);
        let list_id = find_computed_id(&tree, CHAT_LIST_KEY).expect("chat history scroll node");
        // One layout at MAX clamps and writes back the true max offset.
        ui_state.set_scroll_offset(list_id.as_str(), f32::MAX);
        let _ = render_bundle_with(&mut tree, &mut ui_state, viewport);
        let max_offset = ui_state.scroll_offset(&list_id);
        // Spread successive frames far apart across the range.
        let fracs = [0.07f32, 0.61, 0.23, 0.83, 0.41, 0.97, 0.13, 0.53];
        let mut frame = 0usize;
        scroll_drag.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                ui_state.set_scroll_offset(list_id.as_str(), fracs[frame % fracs.len()] * max_offset);
                frame += 1;
                let mut tree = app.build(&cx);
                black_box(render_bundle_with(&mut tree, &mut ui_state, viewport));
            });
        });
    }
    scroll_drag.finish();

    // Resize: alternate the viewport width every frame, so each timed frame is
    // a width change ⇒ every realized row reflows and re-measures, folding fresh
    // heights back into the persistent index.
    let mut resize = c.benchmark_group("projection_resize");
    let narrow = Rect::new(0.0, 0.0, 960.0, 800.0);
    for &n in &[1000usize, 10_000, 100_000] {
        let (app, _shared, _dir) = make_app(n);
        let theme = app.theme();
        let cx = BuildCx::new(&theme);
        let mut ui_state = UiState::new();
        {
            let mut tree = app.build(&cx);
            let _ = render_bundle_with(&mut tree, &mut ui_state, viewport);
        }
        let mut frame = 0usize;
        resize.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let vp = if frame.is_multiple_of(2) { narrow } else { viewport };
                frame += 1;
                let mut tree = app.build(&cx);
                black_box(render_bundle_with(&mut tree, &mut ui_state, vp));
            });
        });
    }
    resize.finish();

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
