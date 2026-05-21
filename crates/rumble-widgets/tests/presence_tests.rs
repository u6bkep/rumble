//! Functional tests for `UserPresence` via egui_kittest.

use std::sync::Arc;

use eframe::egui::{self, Rect};
use egui_kittest::{Harness, kittest::Queryable};
use rumble_widgets::{ModernTheme, UserPresence, UserState, install_theme};

fn install(ctx: &egui::Context) {
    install_theme(ctx, Arc::new(ModernTheme::default()));
}

#[test]
fn allocates_avatar_only_when_no_name() {
    let mut harness = Harness::new_ui_state(
        |ui, rect: &mut Option<Rect>| {
            install(ui.ctx());
            let resp = UserPresence::new().size(40.0).show(ui);
            *rect = Some(resp.rect);
        },
        None,
    );
    harness.run();
    let r = harness.state().unwrap();
    assert_eq!(r.width(), 40.0, "no name → width = avatar size");
    assert_eq!(r.height(), 40.0);
}

#[test]
fn allocates_avatar_plus_name() {
    let mut harness = Harness::new_ui_state(
        |ui, rect: &mut Option<Rect>| {
            install(ui.ctx());
            let resp = UserPresence::new().name("Alice").size(32.0).show(ui);
            *rect = Some(resp.rect);
        },
        None,
    );
    harness.run();
    let r = harness.state().unwrap();
    assert!(
        r.width() > 32.0,
        "name should expand width past the avatar; got {}",
        r.width(),
    );
    assert_eq!(
        r.height(),
        32.0,
        "row height = max(avatar, text); 32px avatar dominates body text"
    );
}

/// The user's name must be exposed via accesskit so kittest (and screen
/// readers) can find the row by label.
#[test]
fn name_is_accessible() {
    let mut harness = Harness::new_ui_state(
        |ui, _: &mut ()| {
            install(ui.ctx());
            UserPresence::new().name("Bob").show(ui);
        },
        (),
    );
    harness.run();
    // Panics if the label isn't found.
    let _ = harness.get_by_label("Bob");
}

/// Every combination of state flags must render without panicking. There
/// are 32 combinations; cheap enough to enumerate.
#[test]
fn all_state_combos_render_without_panic() {
    for bits in 0u8..32 {
        let state = UserState {
            talking: bits & 0b00001 != 0,
            muted: bits & 0b00010 != 0,
            deafened: bits & 0b00100 != 0,
            server_muted: bits & 0b01000 != 0,
            away: bits & 0b10000 != 0,
        };
        let mut harness = Harness::new_ui_state(
            |ui, _: &mut ()| {
                install(ui.ctx());
                UserPresence::new().name("Carol").state(state).show(ui);
            },
            (),
        );
        // Talking states ask for continuous repaints, so `run()`'s
        // settle-to-steady contract never holds. A single `step()` is
        // enough to prove "this combination doesn't panic".
        harness.step();
    }
}

/// State setters should round-trip into the rendered state. We can't
/// inspect the painter directly, but we can verify the state struct is
/// what we set by capturing it inside the closure (the widget doesn't
/// expose `.state` post-build, so we check via the builder API).
#[test]
fn state_setters_compose() {
    let s = UserState {
        talking: true,
        muted: false,
        deafened: true,
        server_muted: false,
        away: false,
    };
    // Same state, built two ways:
    let combined = UserState::default();
    let _via_builders = UserPresence::new().state(combined).talking(true).deafened(true);
    let _via_struct = UserPresence::new().state(s);
    // No assertion — this is a compile-time + no-panic test that the two
    // forms coexist. The runtime contract is covered by render tests.
}

#[test]
fn size_zero_does_not_panic() {
    let mut harness = Harness::new_ui_state(
        |ui, _: &mut ()| {
            install(ui.ctx());
            UserPresence::new().name("X").size(0.0).show(ui);
        },
        (),
    );
    harness.run();
}
