//! GUI chat-image scaling microbenchmark: how the per-frame `App::build`
//! (El-tree construction) and build + `render_bundle` (layout + draw-op) passes
//! scale with the number of *image* attachments in the chat backlog.
//!
//! Run: `cargo bench -p rumble-damascene --bench chat_images`
//!
//! The sibling `projection` bench scales plain text messages. This one isolates
//! the image cost: `chat::render` eagerly pre-renders the attachment El for
//! every message carrying an attachment — O(images), even for rows the virtual
//! list never realizes (the off-screen-media culling gap, issue #16). Because
//! the client redraws ~30×/sec while idle (see docs/performance-testing.md §3),
//! that eager pass runs 30 times a second; this bench measures how steeply it
//! grows with backlog image count.
//!
//! Images are *pre-decoded* synthetic noise rasters seeded into the media cache
//! directly (the same `insert_image_preview_for_test` hook `dump_bundles` uses),
//! one per message with a unique `transfer_id`. Decode is a one-time setup cost,
//! not part of the measured per-frame loop — what's measured is the projection
//! and layout of N resident image previews. Each raster is distinct so its
//! content hash differs and no texture dedup masks the per-image cost.
//!
//! No GPU, no window — same path the goldens snapshot. The `connected` golden is
//! the behavior net; this is the cost net.
//!
//! Baseline-gated workflow:
//!   cargo bench -p rumble-damascene --bench chat_images -- --save-baseline before
//!   # ... optimize ...
//!   cargo bench -p rumble-damascene --bench chat_images -- --baseline before

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group};
use damascene_core::prelude::*;
use prost::Message;
use rumble_client::{
    AudioState, AudioStats, ChatMessage, ChatMessageKind, ChatMessageVisibility, Command, ConnectionState,
    MeterSnapshot, State, VoiceMode,
};
use rumble_damascene::{Identity, RumbleApp, backend::UiBackend};
use rumble_desktop_shell::SettingsStore;
use rumble_protocol::{
    ChatAttachment, Uuid,
    permissions::Permissions,
    proto::{RelayFileSharePayload, RoomInfo, User, UserId},
    room_id_from_uuid,
};

const ROOM_LOBBY: u128 = 0x1111_1111_1111_1111_1111_1111_1111_1111;
const ROOM_WORK: u128 = 0x2222_2222_2222_2222_2222_2222_2222_2222;

/// Pixel dimensions of each synthetic preview. Kept modest so a 100-image
/// backlog stays cheap to seed; the measured cost scales with image *count*,
/// not pixel area (layout/draw-op generation is per-element, not per-texel).
const NOISE_W: u32 = 128;
const NOISE_H: u32 = 128;

/// Returns a canned `State` and discards commands — the renderer only exercises
/// the read half of [`UiBackend`].
struct MockBackend {
    state: State,
}

impl UiBackend for MockBackend {
    fn state(&self) -> State {
        self.state.clone()
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

/// Stable per-image `transfer_id`. Used as both the chat attachment's id and the
/// media-cache key, so each message's preview resolves to its own raster.
fn transfer_id(seq: u32) -> String {
    format!("noise-{seq:05}")
}

/// A 128×128 RGBA8 noise raster, deterministic per `seq` so runs are repeatable
/// and distinct per image (distinct content hash → no texture dedup). Uses a
/// tiny inline xorshift rather than `rand` to keep the fixture self-contained
/// and avoid pulling RNG state into the hot loop.
fn noise_image(seq: u32) -> Image {
    // Seed away from 0 (xorshift fixed point) and decorrelate adjacent seqs.
    let mut s: u32 = seq.wrapping_mul(2_654_435_761).wrapping_add(0x9e37_79b9) | 1;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    };
    let mut pixels = Vec::with_capacity((NOISE_W * NOISE_H * 4) as usize);
    for _ in 0..(NOISE_W * NOISE_H) {
        let v = next();
        pixels.extend_from_slice(&[v as u8, (v >> 8) as u8, (v >> 16) as u8, 0xff]);
    }
    Image::from_rgba8(NOISE_W, NOISE_H, pixels)
}

/// A relay-plugin `ChatAttachment` pointing at the cache entry for `seq`.
/// Mirrors `dump_bundles::relay_attachment`.
fn noise_attachment(seq: u32) -> ChatAttachment {
    let payload = RelayFileSharePayload {
        transfer_id: transfer_id(seq),
        name: format!("noise_{seq}.png"),
        size: (NOISE_W * NOISE_H * 4) as u64,
        mime: "image/png".into(),
        share_data: "demo".into(),
    };
    ChatAttachment {
        namespace: rumble_desktop::FILE_TRANSFER_RELAY_NAMESPACE.to_string(),
        schema_version: rumble_desktop::FILE_TRANSFER_RELAY_PAYLOAD_SCHEMA_VERSION,
        payload: payload.encode_to_vec(),
        fallback_text: format!("shared image: noise_{seq}.png"),
    }
}

/// One chat message carrying the noise attachment for `seq`, with a unique id.
fn make_image_chat(seq: u32, sender: &str) -> ChatMessage {
    let mut id = [0u8; 16];
    id[12..16].copy_from_slice(&seq.to_le_bytes());
    ChatMessage {
        id,
        sender: sender.into(),
        sender_id: None,
        text: format!("shared image: noise_{seq}.png"),
        timestamp: std::time::SystemTime::UNIX_EPOCH,
        kind: ChatMessageKind::Room,
        attachment: Some(noise_attachment(seq)),
        visibility: ChatMessageVisibility::Normal,
    }
}

/// Connected state mirroring the `connected` golden scene, but with `n_images`
/// chat messages, each carrying an image attachment.
fn connected_state_with_images(n_images: usize) -> State {
    let mut audio = AudioState {
        voice_mode: VoiceMode::Continuous,
        ..AudioState::default()
    };
    audio.talking_users.insert(2);

    let senders = ["alice", "bob", "charlie", "diana"];
    let chat_messages = (0..n_images as u32)
        .map(|seq| make_image_chat(seq, senders[seq as usize % senders.len()]))
        .collect();

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

/// Build a connected `RumbleApp` over `n_images` image attachments, with one
/// pre-decoded noise raster seeded into the media cache per message so the
/// attachment renders the image-preview path (not the file-card fallback).
/// First-run wizard suppressed so we render the main shell. The temp config dir
/// is kept alive by the returned guard.
fn make_app(n_images: usize) -> (RumbleApp<MockBackend>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = MockBackend {
        state: connected_state_with_images(n_images),
    };
    let identity = Identity::load(dir.path().to_path_buf()).expect("identity");
    let settings = SettingsStore::load_from_path(Some(dir.path().join("settings.json")));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let mut app = RumbleApp::new(backend, identity, settings, runtime);
    app.suppress_first_run_for_test();
    for seq in 0..n_images as u32 {
        app.insert_image_preview_for_test(transfer_id(seq), noise_image(seq));
    }
    (app, dir)
}

fn bench_chat_images(c: &mut Criterion) {
    let viewport = Rect::new(0.0, 0.0, 1280.0, 800.0);
    let counts = [1usize, 10, 50, 100];

    let mut build = c.benchmark_group("chat_images_build");
    for &n in &counts {
        let (app, _dir) = make_app(n);
        let theme = app.theme();
        let cx = BuildCx::new(&theme);
        build.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| black_box(app.build(&cx)));
        });
    }
    build.finish();

    let mut layout = c.benchmark_group("chat_images_layout");
    for &n in &counts {
        let (app, _dir) = make_app(n);
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
}

criterion_group!(benches, bench_chat_images);

fn main() {
    // Each RumbleApp opens an XDG GlobalShortcuts portal session on Wayland;
    // built back-to-back (one per bench param) they back up xdg-desktop-portal
    // and hang teardown. This bench only needs the El-tree path, not global
    // hotkeys, so opt out — same escape hatch `dump_bundles` and the
    // `projection` bench use. Safety: set before any tokio runtime/thread is
    // built (all RumbleApp construction happens later, inside `benches()`).
    unsafe {
        std::env::set_var("RUMBLE_DISABLE_PORTAL", "1");
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
}
