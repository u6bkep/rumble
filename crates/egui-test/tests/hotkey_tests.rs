//! Integration tests for hotkey functionality using TestHarness.
//!
//! Run with: cargo test -p egui-test --features test-harness --test hotkey_tests

use egui_test::{HotkeyBinding, HotkeyManager, HotkeyModifiers, TestHarness};

/// Test that the default PTT hotkey (Space) is configured.
#[test]
fn test_default_ptt_hotkey_configured() {
    let harness = TestHarness::new();

    // Check that keyboard settings have a default PTT hotkey
    let ptt_hotkey = &harness.app().persistent_settings().keyboard.ptt_hotkey;
    assert!(ptt_hotkey.is_some(), "Default PTT hotkey should be configured");

    let binding = ptt_hotkey.as_ref().unwrap();
    assert_eq!(binding.key, "Space", "Default PTT key should be Space");
    assert!(!binding.modifiers.ctrl, "Default PTT should have no Ctrl");
    assert!(!binding.modifiers.shift, "Default PTT should have no Shift");
    assert!(!binding.modifiers.alt, "Default PTT should have no Alt");
}

/// Test key string to egui key conversion.
#[test]
fn test_key_string_to_egui_conversion() {
    // Test that our key conversion works for common keys
    assert!(HotkeyManager::key_string_to_egui_key("Space").is_some());
    assert!(HotkeyManager::key_string_to_egui_key("F1").is_some());
    assert!(HotkeyManager::key_string_to_egui_key("A").is_some());
    assert!(HotkeyManager::key_string_to_egui_key("InvalidKey").is_none());

    // Test case insensitivity
    assert!(HotkeyManager::key_string_to_egui_key("space").is_some());
    assert!(HotkeyManager::key_string_to_egui_key("SPACE").is_some());
}

/// Test hotkey binding display format.
#[test]
fn test_hotkey_binding_display() {
    let binding = HotkeyBinding {
        modifiers: HotkeyModifiers {
            ctrl: true,
            shift: true,
            alt: false,
            super_key: false,
        },
        key: "Space".to_string(),
    };

    assert_eq!(binding.display(), "Ctrl+Shift+Space");

    // Test with no modifiers
    let binding_no_mods = HotkeyBinding {
        modifiers: HotkeyModifiers::default(),
        key: "F1".to_string(),
    };
    assert_eq!(binding_no_mods.display(), "F1");
}

/// Test that PTT fallback detects key presses via egui input.
/// Note: This test requires the test-harness feature for key injection.
#[test]
#[cfg(feature = "test-harness")]
fn test_ptt_fallback_key_press() {
    use eframe::egui;

    let mut harness = TestHarness::new();

    // Verify initial state - PTT should be inactive
    assert!(
        !harness.app().push_to_talk_active(),
        "PTT should be inactive initially"
    );

    // Run a few frames to initialize
    harness.run_frames(5);

    // Note: PTT only works when connected, but we can still verify the state tracking
    // The key press won't start transmission without a connection, but the fallback
    // code path should at least not crash.

    // Inject Space key press
    harness.key_press(egui::Key::Space);
    harness.run_frames(2);

    // Since we're not connected, PTT shouldn't activate
    // (the code checks is_connected() before setting push_to_talk_active)
    assert!(
        !harness.app().push_to_talk_active(),
        "PTT should not activate without connection"
    );

    // Release the key
    harness.key_release(egui::Key::Space);
    harness.run_frames(2);
}

/// Test that egui key_down detection works correctly in the test harness.
/// This verifies the test infrastructure can detect held keys.
#[test]
#[cfg(feature = "test-harness")]
fn test_key_down_detection() {
    use eframe::egui;

    let mut harness = TestHarness::new();
    harness.run_frames(2);

    // Test 1: Check key_down immediately after harness.key_down()
    harness.kittest_mut().key_down(egui::Key::Space);
    let key_is_down_immediate = harness.kittest().ctx.input(|i| i.key_down(egui::Key::Space));
    println!(
        "Test 1 - Key down state immediately after key_down(): {}",
        key_is_down_immediate
    );

    // Test 2: Check after running a step (which processes input)
    harness.kittest_mut().run_steps(1);
    let key_is_down_after_step = harness.kittest().ctx.input(|i| i.key_down(egui::Key::Space));
    println!(
        "Test 2 - Key down state after run_steps(1): {}",
        key_is_down_after_step
    );

    // Test 3: Check key_pressed (event-based) instead of key_down (state-based)
    harness.kittest_mut().key_down(egui::Key::F1);
    harness.kittest_mut().run_steps(1);
    let key_was_pressed = harness.kittest().ctx.input(|i| i.key_pressed(egui::Key::F1));
    println!(
        "Test 3 - key_pressed after key_down + run: {}",
        key_was_pressed
    );

    // Release keys
    harness.kittest_mut().key_up(egui::Key::Space);
    harness.kittest_mut().key_up(egui::Key::F1);
    harness.run_frames(1);

    let key_is_down_after_release =
        harness.kittest().ctx.input(|i| i.key_down(egui::Key::Space));
    println!(
        "Test 4 - Key down state after release: {}",
        key_is_down_after_release
    );
    assert!(
        !key_is_down_after_release,
        "Key should not be down after release"
    );
}

/// Test keyboard settings can be modified.
#[test]
fn test_keyboard_settings_modification() {
    let mut harness = TestHarness::new();

    // Change the PTT key to F1
    let new_binding = HotkeyBinding {
        modifiers: HotkeyModifiers::default(),
        key: "F1".to_string(),
    };

    harness.app_mut().persistent_settings_mut().keyboard.ptt_hotkey = Some(new_binding);
    harness.run_frames(2);

    // Verify the change
    let ptt_hotkey = &harness.app().persistent_settings().keyboard.ptt_hotkey;
    assert!(ptt_hotkey.is_some());
    assert_eq!(ptt_hotkey.as_ref().unwrap().key, "F1");
}

/// Test round-trip conversion of egui keys to string and back.
#[test]
fn test_key_roundtrip() {
    use eframe::egui::Key;

    // Test a few representative keys
    let keys_to_test = [Key::Space, Key::F1, Key::A, Key::Num0, Key::Enter];

    for key in keys_to_test {
        if let Some(key_string) = HotkeyManager::egui_key_to_string(key) {
            let converted_back = HotkeyManager::key_string_to_egui_key(&key_string);
            assert_eq!(
                converted_back,
                Some(key),
                "Roundtrip failed for key {:?} -> {} -> {:?}",
                key,
                key_string,
                converted_back
            );
        }
    }
}

/// Test that handle_hotkey_event works correctly.
#[test]
fn test_handle_hotkey_event() {
    use egui_test::HotkeyEvent;

    let mut harness = TestHarness::new();
    harness.run_frames(2);

    // Verify initial state
    assert!(
        !harness.app().push_to_talk_active(),
        "PTT should be inactive initially"
    );

    // Simulate a PttPressed event (this bypasses global-hotkey and tests the handler directly)
    harness.app_mut().handle_hotkey_event(HotkeyEvent::PttPressed);

    // PTT should still be inactive because we're not connected
    assert!(
        !harness.app().push_to_talk_active(),
        "PTT should not activate without connection (via handle_hotkey_event)"
    );

    // Verify the event handling didn't crash and state is consistent
    harness.run_frames(2);
}

/// Debug test to verify key detection works during render.
#[test]
#[cfg(feature = "test-harness")]
fn test_ptt_key_detection_during_render() {
    use eframe::egui;

    let mut harness = TestHarness::new();
    harness.run_frames(2);

    // Verify the PTT key is configured
    let ptt_key = harness
        .app()
        .persistent_settings()
        .keyboard
        .ptt_hotkey
        .as_ref()
        .map(|b| b.key.clone());
    println!("Configured PTT key: {:?}", ptt_key);
    assert_eq!(ptt_key, Some("Space".to_string()));

    // Verify the key conversion works
    let egui_key = HotkeyManager::key_string_to_egui_key("Space");
    println!("Converted to egui key: {:?}", egui_key);
    assert_eq!(egui_key, Some(egui::Key::Space));

    // Now hold Space and run a frame - the app's render() should detect the key
    harness.kittest_mut().key_down(egui::Key::Space);
    harness.kittest_mut().run_steps(1);

    // Check if the app saw the key press
    // (PTT won't activate without connection, but we can check the key detection worked)
    let key_was_down = harness.kittest().ctx.input(|i| i.key_down(egui::Key::Space));
    println!("Key down state during/after render: {}", key_was_down);

    // This should be true if key detection is working
    assert!(key_was_down, "Key should be detected as down");

    // Clean up
    harness.kittest_mut().key_up(egui::Key::Space);
    harness.run_frames(1);
}

/// Test that handle_hotkey_event works for mute toggle (requires connection).
#[test]
fn test_handle_hotkey_event_mute_toggle() {
    use egui_test::HotkeyEvent;

    let mut harness = TestHarness::new();
    harness.run_frames(2);

    // Verify we're not connected
    let is_connected = harness.app().backend().state().connection.is_connected();
    assert!(!is_connected, "Should not be connected for this test");

    // Get initial mute state from backend
    let initial_muted = harness.app().backend().state().audio.self_muted;
    println!("Initial mute state: {}", initial_muted);

    // Simulate a ToggleMute event - should NOT toggle without connection
    harness.app_mut().handle_hotkey_event(HotkeyEvent::ToggleMute);
    harness.run_frames(2);

    // Mute should NOT change without connection
    let after_toggle = harness.app().backend().state().audio.self_muted;
    println!("Mute state after toggle (no connection): {}", after_toggle);
    assert_eq!(
        initial_muted, after_toggle,
        "Mute state should NOT change without connection (via handle_hotkey_event)"
    );

    // Verify the event handling didn't crash and state is consistent
    harness.run_frames(2);
}

/// Test that handle_hotkey_event works for deafen toggle (requires connection).
#[test]
fn test_handle_hotkey_event_deafen_toggle() {
    use egui_test::HotkeyEvent;

    let mut harness = TestHarness::new();
    harness.run_frames(2);

    // Verify we're not connected
    let is_connected = harness.app().backend().state().connection.is_connected();
    assert!(!is_connected, "Should not be connected for this test");

    // Get initial deafen state from backend
    let initial_deafened = harness.app().backend().state().audio.self_deafened;
    println!("Initial deafen state: {}", initial_deafened);

    // Simulate a ToggleDeafen event - should NOT toggle without connection
    harness.app_mut().handle_hotkey_event(HotkeyEvent::ToggleDeafen);
    harness.run_frames(2);

    // Deafen should NOT change without connection
    let after_toggle = harness.app().backend().state().audio.self_deafened;
    println!("Deafen state after toggle (no connection): {}", after_toggle);
    assert_eq!(
        initial_deafened, after_toggle,
        "Deafen state should NOT change without connection (via handle_hotkey_event)"
    );

    // Verify the event handling didn't crash and state is consistent
    harness.run_frames(2);
}

/// Test mute toggle hotkey settings can be configured.
#[test]
fn test_mute_hotkey_settings_modification() {
    let mut harness = TestHarness::new();

    // Initially, mute hotkey may not be set
    let initial = harness
        .app()
        .persistent_settings()
        .keyboard
        .toggle_mute_hotkey
        .clone();
    println!("Initial mute hotkey: {:?}", initial);

    // Set the mute hotkey to M
    let new_binding = HotkeyBinding {
        modifiers: HotkeyModifiers::default(),
        key: "M".to_string(),
    };

    harness
        .app_mut()
        .persistent_settings_mut()
        .keyboard
        .toggle_mute_hotkey = Some(new_binding);
    harness.run_frames(2);

    // Verify the change
    let mute_hotkey = &harness
        .app()
        .persistent_settings()
        .keyboard
        .toggle_mute_hotkey;
    assert!(mute_hotkey.is_some());
    assert_eq!(mute_hotkey.as_ref().unwrap().key, "M");
}

/// Test deafen toggle hotkey settings can be configured.
#[test]
fn test_deafen_hotkey_settings_modification() {
    let mut harness = TestHarness::new();

    // Set the deafen hotkey to D
    let new_binding = HotkeyBinding {
        modifiers: HotkeyModifiers::default(),
        key: "D".to_string(),
    };

    harness
        .app_mut()
        .persistent_settings_mut()
        .keyboard
        .toggle_deafen_hotkey = Some(new_binding);
    harness.run_frames(2);

    // Verify the change
    let deafen_hotkey = &harness
        .app()
        .persistent_settings()
        .keyboard
        .toggle_deafen_hotkey;
    assert!(deafen_hotkey.is_some());
    assert_eq!(deafen_hotkey.as_ref().unwrap().key, "D");
}
