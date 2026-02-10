//! Test harness for programmatic control of the Rumble application.
//!
//! This module provides [`TestHarness`], which allows automated testing of the
//! UI by taking screenshots, sending input events, and introspecting state.
//!
//! # Widget Queries (kittest integration)
//!
//! When built with the `test-harness` feature, the harness supports finding
//! widgets by their accessible label using egui_kittest:
//!
//! ```ignore
//! use egui_test::{TestHarness, Args};
//!
//! let mut harness = TestHarness::with_args(args);
//! harness.run();
//!
//! // Find and click a button by its label
//! if let Some(node) = harness.try_get_by_label("Connect") {
//!     harness.click_widget("Connect");
//! }
//! ```
//!
//! **Note**: The room/user tree view (egui_ltreeview) does not expose AccessKit
//! metadata, so tree nodes cannot be found by label. Use coordinate-based clicking
//! or backend state inspection for tree interaction.
//!
//! # Example
//!
//! ```ignore
//! use egui_test::{TestHarness, Args};
//!
//! let mut harness = TestHarness::new();
//!
//! // Run frames until UI settles
//! harness.run();
//!
//! // Check state
//! assert!(!harness.is_connected());
//! ```

use eframe::egui;

use crate::{RumbleApp, settings::Args};

#[cfg(feature = "test-harness")]
use egui_kittest::{Harness, kittest::Queryable};

/// Test harness for automated UI testing.
///
/// Wraps a [`RumbleApp`] instance and provides methods for:
/// - Running frames
/// - Sending input events (keys, clicks, text)
/// - Finding widgets by label (requires `test-harness` feature)
/// - Introspecting application state
pub struct TestHarness {
    #[cfg(feature = "test-harness")]
    harness: Harness<'static, HarnessState>,

    #[cfg(not(feature = "test-harness"))]
    ctx: egui::Context,
    #[cfg(not(feature = "test-harness"))]
    app: RumbleApp,

    runtime: tokio::runtime::Runtime,
}

/// State held by the kittest harness.
#[cfg(feature = "test-harness")]
pub struct HarnessState {
    /// The RumbleApp instance.
    pub app: RumbleApp,
}

impl TestHarness {
    /// Create a new test harness with default arguments.
    pub fn new() -> Self {
        Self::with_args(Args::default())
    }

    /// Create a new test harness with custom arguments.
    #[cfg(feature = "test-harness")]
    pub fn with_args(args: Args) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("Failed to create Tokio runtime");

        // Create app with a temporary context - it will be replaced by kittest's
        // context on first render() call
        let temp_ctx = egui::Context::default();
        let app = RumbleApp::new(temp_ctx, runtime.handle().clone(), args);

        let state = HarnessState { app };

        let harness = Harness::new_state(
            |ctx, state: &mut HarnessState| {
                state.app.render(ctx);
            },
            state,
        );

        Self { harness, runtime }
    }

    /// Create a new test harness with custom arguments (non-kittest version).
    #[cfg(not(feature = "test-harness"))]
    pub fn with_args(args: Args) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("Failed to create Tokio runtime");

        let ctx = egui::Context::default();
        let app = RumbleApp::new(ctx.clone(), runtime.handle().clone(), args);

        Self { ctx, app, runtime }
    }

    /// Run frames until UI animations complete and no more repaints are requested.
    ///
    /// This is the recommended way to advance the UI after interactions.
    #[cfg(feature = "test-harness")]
    pub fn run(&mut self) {
        self.harness.run();
    }

    /// Run frames until UI settles (non-kittest version runs a fixed number of frames).
    #[cfg(not(feature = "test-harness"))]
    pub fn run(&mut self) {
        // Without kittest, run a reasonable number of frames
        self.run_frames(10);
    }

    /// Run one frame of the application.
    #[cfg(feature = "test-harness")]
    pub fn run_frame(&mut self) {
        self.harness.run_steps(1);
    }

    /// Run one frame of the application.
    #[cfg(not(feature = "test-harness"))]
    pub fn run_frame(&mut self) {
        let raw_input = egui::RawInput::default();
        let _ = self.ctx.run(raw_input, |ctx| {
            self.app.render(ctx);
        });
    }

    /// Run multiple frames of the application.
    #[cfg(feature = "test-harness")]
    pub fn run_frames(&mut self, count: usize) {
        self.harness.run_steps(count);
    }

    /// Run multiple frames of the application.
    #[cfg(not(feature = "test-harness"))]
    pub fn run_frames(&mut self, count: usize) {
        for _ in 0..count {
            self.run_frame();
        }
    }

    // =========================================================================
    // Widget queries (kittest feature)
    // =========================================================================

    /// Access the underlying kittest harness for advanced widget queries.
    ///
    /// Use this to access methods like `get_by_label`, `query_by_label`, etc.
    ///
    /// # Example
    /// ```ignore
    /// let node = harness.kittest().get_by_label("Connect");
    /// node.click();
    /// harness.run();
    /// ```
    ///
    /// # Note
    /// Tree view nodes (rooms/users) are not accessible via kittest queries
    /// because egui_ltreeview doesn't expose AccessKit metadata.
    #[cfg(feature = "test-harness")]
    pub fn kittest(&self) -> &Harness<'static, HarnessState> {
        &self.harness
    }

    /// Access the underlying kittest harness mutably.
    #[cfg(feature = "test-harness")]
    pub fn kittest_mut(&mut self) -> &mut Harness<'static, HarnessState> {
        &mut self.harness
    }

    /// Click a widget by its accessible label.
    ///
    /// Finds the widget, clicks it, and runs frames until stable.
    /// Panics if the widget is not found.
    ///
    /// # Note
    /// Tree view nodes (rooms/users) are not clickable via this method.
    #[cfg(feature = "test-harness")]
    pub fn click_widget(&mut self, label: &str) {
        self.harness.get_by_label(label).click();
        self.harness.run();
    }

    /// Try to click a widget by its accessible label.
    ///
    /// Returns `true` if the widget was found and clicked, `false` otherwise.
    #[cfg(feature = "test-harness")]
    pub fn try_click_widget(&mut self, label: &str) -> bool {
        if self.harness.query_by_label(label).is_some() {
            self.harness.get_by_label(label).click();
            self.harness.run();
            true
        } else {
            false
        }
    }

    /// Check if a widget with the given label exists.
    #[cfg(feature = "test-harness")]
    pub fn has_widget(&self, label: &str) -> bool {
        self.harness.query_by_label(label).is_some()
    }

    /// Get the bounding rectangle of a widget by label.
    ///
    /// Returns `None` if the widget is not found.
    #[cfg(feature = "test-harness")]
    pub fn widget_rect(&self, label: &str) -> Option<egui::Rect> {
        self.harness.query_by_label(label).map(|n| n.rect())
    }

    /// Type text into the UI by injecting a text event, then run frames.
    #[cfg(feature = "test-harness")]
    pub fn type_into_focused(&mut self, text: &str) {
        self.harness
            .input_mut()
            .events
            .push(egui::Event::Text(text.to_string()));
        self.harness.run();
    }

    // =========================================================================
    // Low-level input injection
    // =========================================================================

    /// Inject a key press event.
    #[cfg(feature = "test-harness")]
    pub fn key_press(&mut self, key: egui::Key) {
        self.harness.key_down(key);
        self.harness.run_steps(1);
    }

    /// Inject a key press event.
    #[cfg(not(feature = "test-harness"))]
    pub fn key_press(&mut self, key: egui::Key) {
        let mut raw_input = egui::RawInput::default();
        raw_input.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = self.ctx.run(raw_input, |ctx| {
            self.app.render(ctx);
        });
    }

    /// Inject a key release event.
    #[cfg(feature = "test-harness")]
    pub fn key_release(&mut self, key: egui::Key) {
        self.harness.key_up(key);
        self.harness.run_steps(1);
    }

    /// Inject a key release event.
    #[cfg(not(feature = "test-harness"))]
    pub fn key_release(&mut self, key: egui::Key) {
        let mut raw_input = egui::RawInput::default();
        raw_input.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = self.ctx.run(raw_input, |ctx| {
            self.app.render(ctx);
        });
    }

    /// Press and release a key.
    #[cfg(feature = "test-harness")]
    pub fn key_tap(&mut self, key: egui::Key) {
        self.harness.key_press(key);
        self.harness.run();
    }

    /// Press and release a key.
    #[cfg(not(feature = "test-harness"))]
    pub fn key_tap(&mut self, key: egui::Key) {
        self.key_press(key);
        self.key_release(key);
    }

    /// Inject a mouse click at the specified position.
    #[cfg(feature = "test-harness")]
    pub fn click(&mut self, pos: egui::Pos2) {
        // Hover first, then press and release
        self.harness.hover_at(pos);
        self.harness.drag_at(pos);
        self.harness.drop_at(pos);
        self.harness.run();
    }

    /// Inject a mouse click at the specified position.
    #[cfg(not(feature = "test-harness"))]
    pub fn click(&mut self, pos: egui::Pos2) {
        let mut raw_input = egui::RawInput::default();
        raw_input.events.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = self.ctx.run(raw_input.clone(), |ctx| {
            self.app.render(ctx);
        });

        // Release
        let mut raw_input = egui::RawInput::default();
        raw_input.events.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = self.ctx.run(raw_input, |ctx| {
            self.app.render(ctx);
        });
    }

    /// Move the mouse to the specified position.
    #[cfg(feature = "test-harness")]
    pub fn mouse_move(&mut self, pos: egui::Pos2) {
        self.harness.hover_at(pos);
        self.harness.run_steps(1);
    }

    /// Move the mouse to the specified position.
    #[cfg(not(feature = "test-harness"))]
    pub fn mouse_move(&mut self, pos: egui::Pos2) {
        let mut raw_input = egui::RawInput::default();
        raw_input.events.push(egui::Event::PointerMoved(pos));
        let _ = self.ctx.run(raw_input, |ctx| {
            self.app.render(ctx);
        });
    }

    /// Inject text input.
    #[cfg(feature = "test-harness")]
    pub fn type_text(&mut self, text: &str) {
        self.harness
            .input_mut()
            .events
            .push(egui::Event::Text(text.to_string()));
        self.harness.run_steps(1);
    }

    /// Inject text input.
    #[cfg(not(feature = "test-harness"))]
    pub fn type_text(&mut self, text: &str) {
        let mut raw_input = egui::RawInput::default();
        raw_input.events.push(egui::Event::Text(text.to_string()));
        let _ = self.ctx.run(raw_input, |ctx| {
            self.app.render(ctx);
        });
    }

    // =========================================================================
    // State access
    // =========================================================================

    /// Get a reference to the underlying RumbleApp.
    #[cfg(feature = "test-harness")]
    pub fn app(&self) -> &RumbleApp {
        &self.harness.state().app
    }

    /// Get a reference to the underlying RumbleApp.
    #[cfg(not(feature = "test-harness"))]
    pub fn app(&self) -> &RumbleApp {
        &self.app
    }

    /// Get a mutable reference to the underlying RumbleApp.
    #[cfg(feature = "test-harness")]
    pub fn app_mut(&mut self) -> &mut RumbleApp {
        &mut self.harness.state_mut().app
    }

    /// Get a mutable reference to the underlying RumbleApp.
    #[cfg(not(feature = "test-harness"))]
    pub fn app_mut(&mut self) -> &mut RumbleApp {
        &mut self.app
    }

    /// Check if the application is connected to a server.
    pub fn is_connected(&self) -> bool {
        self.app().is_connected()
    }

    /// Get the egui context.
    #[cfg(feature = "test-harness")]
    pub fn ctx(&self) -> &egui::Context {
        &self.harness.ctx
    }

    /// Get the egui context.
    #[cfg(not(feature = "test-harness"))]
    pub fn ctx(&self) -> &egui::Context {
        &self.ctx
    }

    /// Get access to the tokio runtime for async operations in tests.
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        &self.runtime
    }

    /// Get the last frame's full output (for rendering/screenshots).
    #[cfg(feature = "test-harness")]
    pub fn output(&self) -> &egui::FullOutput {
        self.harness.output()
    }
}

impl Default for TestHarness {
    fn default() -> Self {
        Self::new()
    }
}
