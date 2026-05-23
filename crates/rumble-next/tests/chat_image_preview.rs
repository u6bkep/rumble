//! Smoke test for the chat image-preview path.
//!
//! Builds an `App` against a `MockBackend`, drops a real PNG on disk,
//! arranges state so the chat shows a completed `FileOffer` for that
//! file, and steps the harness a handful of frames. The test passes if
//! nothing panics — image decoding goes through egui_extras' file://
//! loader, so this also catches missing image-format features.

#![cfg(feature = "test-harness")]

use eframe::egui::{self, Event, Modifiers, PointerButton, Pos2};
use image::{ImageBuffer, Rgba};
use rumble_client_traits::file_transfer::{PluginTransferState, TransferId, TransferStatus};
use rumble_next::TestHarness;
use rumble_protocol::{
    ChatAttachment, ChatMessage, ChatMessageKind, ConnectionState, FileOfferInfo, State, proto, room_id_from_uuid,
};
use std::path::PathBuf;

fn write_test_png_sized(w: u32, h: u32, name: &str) -> PathBuf {
    // Magenta PNG of the requested resolution. Used for both the
    // small smoke test and a larger-image stress test that mirrors the
    // 8K UHD image the user reported the panic with — we want to
    // reproduce the panic at a size egui actually has to scale down.
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(w, h, |_, _| Rgba([0xff, 0x00, 0xff, 0xff]));
    let dir = std::env::temp_dir().join(format!(
        "rumble-next-image-preview-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    let path = dir.join(name);
    img.save(&path).expect("save png");
    path
}

fn write_test_png() -> PathBuf {
    write_test_png_sized(32, 16, "hello.png")
}

fn sandbox_config_dir() {
    if std::env::var_os("RUMBLE_NEXT_CONFIG_DIR").is_some() {
        return;
    }
    let sandbox = std::env::temp_dir().join(format!(
        "rumble-next-tests-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&sandbox).expect("sandbox");
    // SAFETY: tests are single-threaded entry points; nothing else reads
    // this env var before we set it.
    unsafe {
        std::env::set_var("RUMBLE_NEXT_CONFIG_DIR", &sandbox);
    }
}

#[test]
fn renders_image_preview_for_completed_file_offer() {
    sandbox_config_dir();

    let png = write_test_png();
    let room_id = uuid::Uuid::new_v4();

    let mut state = State {
        connection: ConnectionState::Connected {
            server_name: "test".to_string(),
            user_id: 1,
        },
        my_user_id: Some(1),
        my_room_id: Some(room_id),
        ..Default::default()
    };
    state.rooms.push(proto::RoomInfo {
        id: Some(room_id_from_uuid(room_id)),
        name: "lobby".to_string(),
        parent_id: None,
        description: None,
        inherit_acl: true,
        acls: vec![],
        effective_permissions: 0,
    });
    state.users.push(proto::User {
        user_id: Some(proto::UserId { value: 1 }),
        username: "me".to_string(),
        current_room: Some(room_id_from_uuid(room_id)),
        ..Default::default()
    });
    state.users.push(proto::User {
        user_id: Some(proto::UserId { value: 2 }),
        username: "alice".to_string(),
        current_room: Some(room_id_from_uuid(room_id)),
        ..Default::default()
    });
    state.rebuild_room_tree();

    state.chat_messages.push(ChatMessage {
        id: [0u8; 16],
        sender: "alice".to_string(),
        text: "shared hello.png".to_string(),
        timestamp: std::time::SystemTime::now(),
        is_local: false,
        kind: ChatMessageKind::Room,
        attachment: Some(ChatAttachment::FileOffer(FileOfferInfo {
            schema_version: 1,
            transfer_id: "tx-1".to_string(),
            name: "hello.png".to_string(),
            size: 256,
            mime: "image/png".to_string(),
            share_data: "{}".to_string(),
        })),
        phase: Default::default(),
    });

    let mut harness = TestHarness::with_state(state);
    harness.backend().set_transfers(vec![TransferStatus {
        id: TransferId("tx-1".to_string()),
        name: "hello.png".to_string(),
        size: 256,
        progress: 1.0,
        download_speed: 0,
        upload_speed: 0,
        peers: 0,
        state: PluginTransferState::Seeding,
        is_finished: true,
        error: None,
        local_path: Some(png.clone()),
        peer_details: vec![],
    }]);

    // Several frames: the egui_extras image loader resolves async, and
    // the chat ScrollArea also needs a couple of frames to settle. The
    // first render kicks off the image-loader request; subsequent
    // frames consume the resolved bytes and upload the texture.
    for _ in 0..30 {
        harness.step();
        std::thread::sleep(std::time::Duration::from_millis(8));
    }

    // Render the framebuffer to exercise the image-decode path. When
    // RUMBLE_NEXT_TEST_DUMP is set, write a PNG of the rendered UI so
    // the developer can eyeball the inline preview.
    let img = harness.kittest_mut().render().expect("render harness");
    if let Ok(out) = std::env::var("RUMBLE_NEXT_TEST_DUMP") {
        img.save(&out).expect("save test dump");
        eprintln!("wrote chat preview screenshot to {out}");
    }

    // Open the lightbox programmatically via the public Shell API to
    // verify that path renders too.
    {
        let app = harness.app_mut();
        app.shell
            .open_image_lightbox_for_test(png.clone(), "hello.png".to_string());
    }
    for _ in 0..30 {
        harness.step();
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
    let img2 = harness.kittest_mut().render().expect("render harness with lightbox");
    if let Ok(out) = std::env::var("RUMBLE_NEXT_TEST_LIGHTBOX_DUMP") {
        img2.save(&out).expect("save lightbox dump");
        eprintln!("wrote lightbox screenshot to {out}");
    }

    // Sanity: file still on disk, harness still alive.
    assert!(png.exists());

    // Cleanup.
    let _ = std::fs::remove_file(&png);
    if let Some(parent) = png.parent() {
        let _ = std::fs::remove_dir(parent);
    }

    // Suppress unused import warning when egui isn't strictly needed.
    let _ = egui::Vec2::ZERO;
}

/// Build the same chat-state-with-completed-image that the smoke test
/// uses, plus the on-disk PNG. Returns the harness, the image path, and
/// (after the harness has been stepped a few frames) the rect of the
/// inline preview in screen coordinates.
fn setup_harness_with_image() -> (TestHarness, PathBuf) {
    setup_harness_with_image_sized(32, 16, "hello.png")
}

fn setup_harness_with_image_sized(w: u32, h: u32, name: &str) -> (TestHarness, PathBuf) {
    sandbox_config_dir();

    let png = write_test_png_sized(w, h, name);
    let room_id = uuid::Uuid::new_v4();

    let mut state = State {
        connection: ConnectionState::Connected {
            server_name: "test".to_string(),
            user_id: 1,
        },
        my_user_id: Some(1),
        my_room_id: Some(room_id),
        ..Default::default()
    };
    state.rooms.push(proto::RoomInfo {
        id: Some(room_id_from_uuid(room_id)),
        name: "lobby".to_string(),
        parent_id: None,
        description: None,
        inherit_acl: true,
        acls: vec![],
        effective_permissions: 0,
    });
    state.users.push(proto::User {
        user_id: Some(proto::UserId { value: 1 }),
        username: "me".to_string(),
        current_room: Some(room_id_from_uuid(room_id)),
        ..Default::default()
    });
    state.users.push(proto::User {
        user_id: Some(proto::UserId { value: 2 }),
        username: "alice".to_string(),
        current_room: Some(room_id_from_uuid(room_id)),
        ..Default::default()
    });
    state.rebuild_room_tree();
    state.chat_messages.push(ChatMessage {
        id: [0u8; 16],
        sender: "alice".to_string(),
        text: "shared hello.png".to_string(),
        timestamp: std::time::SystemTime::now(),
        is_local: false,
        kind: ChatMessageKind::Room,
        attachment: Some(ChatAttachment::FileOffer(FileOfferInfo {
            schema_version: 1,
            transfer_id: "tx-1".to_string(),
            name: "hello.png".to_string(),
            size: 256,
            mime: "image/png".to_string(),
            share_data: "{}".to_string(),
        })),
        phase: Default::default(),
    });

    let harness = TestHarness::with_state(state);
    harness.backend().set_transfers(vec![TransferStatus {
        id: TransferId("tx-1".to_string()),
        name: "hello.png".to_string(),
        size: 256,
        progress: 1.0,
        download_speed: 0,
        upload_speed: 0,
        peers: 0,
        state: PluginTransferState::Seeding,
        is_finished: true,
        error: None,
        local_path: Some(png.clone()),
        peer_details: vec![],
    }]);

    (harness, png)
}

/// Drive a synthetic primary-button click at `pos` through the kittest
/// harness. Sends `PointerMoved` → `PointerButton(pressed)` → `step()`
/// → `PointerButton(released)` → `step()` so the button-down and
/// button-up events arrive on different frames. egui's hit-test runs
/// during `begin_pass()` of every frame, so this exercises the same
/// code path that panicked in the live app.
fn synthetic_click(harness: &mut TestHarness, pos: Pos2) {
    let kittest = harness.kittest();
    kittest.event(Event::PointerMoved(pos));
    kittest.event(Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::NONE,
    });
    harness.step();
    let kittest = harness.kittest();
    kittest.event(Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::NONE,
    });
    harness.step();
}

fn settle_until_preview(harness: &mut TestHarness, max_frames: usize) -> egui::Rect {
    let mut rect = None;
    for _ in 0..max_frames {
        harness.step();
        std::thread::sleep(std::time::Duration::from_millis(8));
        if let Some(r) = harness.app().shell.last_image_preview_rect {
            rect = Some(r);
        }
    }
    rect.expect("preview rect was never recorded — image preview did not render")
}

fn cleanup(png: &PathBuf) {
    let _ = std::fs::remove_file(png);
    if let Some(parent) = png.parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

/// Variant A — small image, click at the image's center after letting
/// the texture finish loading. This is the most "obvious" repro and is
/// what we'd want green if/when the underlying egui bug is fixed.
#[test]
fn click_small_image_preview_center() {
    let (mut harness, png) = setup_harness_with_image();
    let preview_rect = settle_until_preview(&mut harness, 40);
    eprintln!("variant A: click at {:?}", preview_rect.center());
    synthetic_click(&mut harness, preview_rect.center());
    let opened = harness.app().shell.image_lightbox_for_test().is_some();
    cleanup(&png);
    assert!(opened, "lightbox didn't open after click");
}

/// Variant B — much bigger image (matches the 7680x4320 the user
/// reported the panic with). Larger images take more frames to decode
/// and may hit different code paths (texture upload, scaling).
#[test]
fn click_large_image_preview_center() {
    let (mut harness, png) = setup_harness_with_image_sized(7680, 4320, "huge.png");
    let preview_rect = settle_until_preview(&mut harness, 80);
    eprintln!("variant B: click at {:?}", preview_rect.center());
    synthetic_click(&mut harness, preview_rect.center());
    let opened = harness.app().shell.image_lightbox_for_test().is_some();
    cleanup(&png);
    assert!(opened, "lightbox didn't open after click on large image");
}

/// Variant C — hover the preview on multiple frames before clicking.
/// In the live app the user has likely already moved the cursor over
/// the image (triggering hover bookkeeping and any tooltip popups)
/// before the click event arrives. If the hover sets up some state
/// that the click then trips over, this should catch it.
#[test]
fn hover_then_click_image_preview() {
    let (mut harness, png) = setup_harness_with_image();
    let preview_rect = settle_until_preview(&mut harness, 40);
    let pos = preview_rect.center();
    eprintln!("variant C: hover then click at {pos:?}");
    for _ in 0..6 {
        harness.kittest().event(egui::Event::PointerMoved(pos));
        harness.step();
    }
    synthetic_click(&mut harness, pos);
    let opened = harness.app().shell.image_lightbox_for_test().is_some();
    cleanup(&png);
    assert!(opened, "lightbox didn't open after hover+click");
}

/// Variant G — overflow the chat scroll area with many messages, then
/// click the image preview. egui's `ScrollArea` only registers a
/// background drag widget when its content is bigger than the
/// viewport; small chats don't trigger that. The user's live app
/// almost certainly had that drag widget present, and combining it
/// with our click-on-image widget might be what trips the hit-test.
#[test]
fn click_image_preview_in_overflowing_chat() {
    sandbox_config_dir();
    let png = write_test_png();
    let room_id = uuid::Uuid::new_v4();

    let mut state = State {
        connection: ConnectionState::Connected {
            server_name: "test".to_string(),
            user_id: 1,
        },
        my_user_id: Some(1),
        my_room_id: Some(room_id),
        ..Default::default()
    };
    state.rooms.push(proto::RoomInfo {
        id: Some(room_id_from_uuid(room_id)),
        name: "lobby".to_string(),
        parent_id: None,
        description: None,
        inherit_acl: true,
        acls: vec![],
        effective_permissions: 0,
    });
    state.users.push(proto::User {
        user_id: Some(proto::UserId { value: 1 }),
        username: "me".to_string(),
        current_room: Some(room_id_from_uuid(room_id)),
        ..Default::default()
    });
    state.users.push(proto::User {
        user_id: Some(proto::UserId { value: 2 }),
        username: "alice".to_string(),
        current_room: Some(room_id_from_uuid(room_id)),
        ..Default::default()
    });
    state.rebuild_room_tree();
    // Pile up text-only messages first so the scroll area overflows
    // and registers its `Sense::drag()` background widget.
    for i in 0..40 {
        state.chat_messages.push(ChatMessage {
            id: [(i & 0xff) as u8; 16],
            sender: "alice".to_string(),
            text: format!("filler message #{i} so the scroll area has to scroll"),
            timestamp: std::time::SystemTime::now(),
            is_local: false,
            kind: ChatMessageKind::Room,
            attachment: None,
            phase: Default::default(),
        });
    }
    // Image FileOffer at the end — that's where stick-to-bottom keeps
    // the viewport, so the synthetic click lands on it.
    state.chat_messages.push(ChatMessage {
        id: [0xfeu8; 16],
        sender: "alice".to_string(),
        text: "shared hello.png".to_string(),
        timestamp: std::time::SystemTime::now(),
        is_local: false,
        kind: ChatMessageKind::Room,
        attachment: Some(ChatAttachment::FileOffer(FileOfferInfo {
            schema_version: 1,
            transfer_id: "tx-1".to_string(),
            name: "hello.png".to_string(),
            size: 256,
            mime: "image/png".to_string(),
            share_data: "{}".to_string(),
        })),
        phase: Default::default(),
    });

    let mut harness = TestHarness::with_state(state);
    harness.backend().set_transfers(vec![TransferStatus {
        id: TransferId("tx-1".to_string()),
        name: "hello.png".to_string(),
        size: 256,
        progress: 1.0,
        download_speed: 0,
        upload_speed: 0,
        peers: 0,
        state: PluginTransferState::Seeding,
        is_finished: true,
        error: None,
        local_path: Some(png.clone()),
        peer_details: vec![],
    }]);

    let preview_rect = settle_until_preview(&mut harness, 60);
    let pos = preview_rect.center();
    eprintln!("variant G: click in overflowing chat at {pos:?}");
    synthetic_click(&mut harness, pos);
    let opened = harness.app().shell.image_lightbox_for_test().is_some();
    cleanup(&png);
    assert!(opened, "lightbox didn't open after click in overflowing chat");
}

/// Variant D — click multiple times rapidly. If the panic only fires
/// on the second-or-later click after the lightbox modal has already
/// captured input, this should expose it.
#[test]
fn double_click_image_preview() {
    let (mut harness, png) = setup_harness_with_image();
    let preview_rect = settle_until_preview(&mut harness, 40);
    let pos = preview_rect.center();
    eprintln!("variant D: double-click at {pos:?}");
    synthetic_click(&mut harness, pos);
    synthetic_click(&mut harness, pos);
    cleanup(&png);
}

/// Variant F — point the FileOffer's local_path at a file that doesn't
/// exist. The egui_extras file:// loader will fail and the Image
/// widget renders the "⚠ failed" path, which adds a `Failed loading…`
/// hover tooltip to the response. Different widget topology than the
/// happy path; might catch a hit-test bug specific to the error case.
#[test]
fn click_image_preview_with_failed_load() {
    sandbox_config_dir();

    let bogus_path = std::env::temp_dir().join(format!(
        "rumble-next-nonexistent-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let room_id = uuid::Uuid::new_v4();

    let mut state = State {
        connection: ConnectionState::Connected {
            server_name: "test".to_string(),
            user_id: 1,
        },
        my_user_id: Some(1),
        my_room_id: Some(room_id),
        ..Default::default()
    };
    state.rooms.push(proto::RoomInfo {
        id: Some(room_id_from_uuid(room_id)),
        name: "lobby".to_string(),
        parent_id: None,
        description: None,
        inherit_acl: true,
        acls: vec![],
        effective_permissions: 0,
    });
    state.users.push(proto::User {
        user_id: Some(proto::UserId { value: 1 }),
        username: "me".to_string(),
        current_room: Some(room_id_from_uuid(room_id)),
        ..Default::default()
    });
    state.rebuild_room_tree();
    state.chat_messages.push(ChatMessage {
        id: [0u8; 16],
        sender: "alice".to_string(),
        text: "shared bogus.png".to_string(),
        timestamp: std::time::SystemTime::now(),
        is_local: false,
        kind: ChatMessageKind::Room,
        attachment: Some(ChatAttachment::FileOffer(FileOfferInfo {
            schema_version: 1,
            transfer_id: "tx-1".to_string(),
            name: "bogus.png".to_string(),
            size: 256,
            mime: "image/png".to_string(),
            share_data: "{}".to_string(),
        })),
        phase: Default::default(),
    });

    let mut harness = TestHarness::with_state(state);
    harness.backend().set_transfers(vec![TransferStatus {
        id: TransferId("tx-1".to_string()),
        name: "bogus.png".to_string(),
        size: 256,
        progress: 1.0,
        download_speed: 0,
        upload_speed: 0,
        peers: 0,
        state: PluginTransferState::Seeding,
        is_finished: true,
        error: None,
        local_path: Some(bogus_path),
        peer_details: vec![],
    }]);

    let preview_rect = settle_until_preview(&mut harness, 80);
    eprintln!("variant F: click failed-load at {:?}", preview_rect.center());
    synthetic_click(&mut harness, preview_rect.center());
}

/// Variant E — click before the texture has finished loading. egui's
/// image loader is async; if the click event arrives during the
/// "Pending" texture state, the Image widget renders a spinner and
/// also adds a `Loading…` hover tooltip. That extra widget might be
/// what's tripping up the hit-test.
#[test]
fn click_image_preview_before_texture_loads() {
    let (mut harness, png) = setup_harness_with_image();
    // Step JUST enough for the preview rect to be recorded but not
    // necessarily for the texture to finish uploading. With sleep=0
    // the egui_extras file loader probably hasn't read the PNG yet.
    let mut rect = None;
    for _ in 0..3 {
        harness.step();
        if let Some(r) = harness.app().shell.last_image_preview_rect {
            rect = Some(r);
            break;
        }
    }
    let Some(rect) = rect else {
        cleanup(&png);
        // Preview hadn't rendered yet → can't repro this variant.
        return;
    };
    eprintln!("variant E: click before load at {:?}", rect.center());
    synthetic_click(&mut harness, rect.center());
    cleanup(&png);
}
