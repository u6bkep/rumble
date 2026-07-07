//! Shared schema-driven audio-pipeline editor.
//!
//! One editor, three call sites: the Settings→Processing tab renders it
//! twice (TX pipeline + RX defaults), and the per-user Audio panel
//! ([`crate::user_audio`]) renders it once for the RX override. Each
//! call site owns its pending [`PipelineConfig`] and passes a
//! [`PipelineKeys`] describing the routed-key namespace, so events from
//! one editor can't leak into another.
//!
//! Per-stage routes are `{prefix}{idx}:{slot}` where the slot vocabulary
//! is `enabled` (switch), `up` / `down` / `remove` (reorder buttons) and
//! `f:{field}` (schema-driven setting control). The "Add processor"
//! dropdown lives on its own `add_key` and is classified with
//! [`select::classify_event`]; the *open/closed* flag for that dropdown
//! stays with the caller (the settings dialog folds it into its
//! single `OpenSelect` slot, the user-audio panel keeps a bool), which
//! is why [`handle_event`] reports [`PipelineEvent::AddToggle`] /
//! [`AddDismiss`][PipelineEvent::AddDismiss] instead of mutating it.

use damascene_core::prelude::*;
use rumble_client::{PipelineConfig, ProcessorConfig, ProcessorRegistry};
use serde_json::Value as JsonValue;

/// Routed-key namespace for one editor instance.
///
/// `prefix` must end with `:` (per-stage routes append `{idx}:{slot}`),
/// and `add_key` must NOT be an extension of `prefix` so the add
/// dropdown never parses as a stage route.
pub struct PipelineKeys {
    /// Per-stage route prefix, e.g. `"settings:proc:"`.
    pub prefix: &'static str,
    /// Route key for the "Add processor" select trigger + menu.
    pub add_key: &'static str,
}

impl PipelineKeys {
    fn enabled_key(&self, idx: usize) -> String {
        format!("{}{idx}:enabled", self.prefix)
    }
    /// Route key for one schema field of a processor. Layout:
    /// `{prefix}{idx}:f:{field}`. The `:f:` separator avoids collisions
    /// with `enabled` / `up` / `down` / `remove` even if a schema were
    /// ever to define a field literally named one of those.
    fn field_key(&self, idx: usize, field: &str) -> String {
        format!("{}{idx}:f:{field}", self.prefix)
    }
    fn move_up_key(&self, idx: usize) -> String {
        format!("{}{idx}:up", self.prefix)
    }
    fn move_down_key(&self, idx: usize) -> String {
        format!("{}{idx}:down", self.prefix)
    }
    fn remove_key(&self, idx: usize) -> String {
        format!("{}{idx}:remove", self.prefix)
    }

    /// Inverse of the key builders above. Returns the
    /// (processor index, slot) for routes under this editor's
    /// namespace, or `None` for unrelated routes (including `add_key`,
    /// which is matched separately).
    fn parse_route<'a>(&self, route: &'a str) -> Option<(usize, ProcRouteSlot<'a>)> {
        let rest = route.strip_prefix(self.prefix)?;
        let (idx_str, tail) = rest.split_once(':')?;
        let idx: usize = idx_str.parse().ok()?;
        let slot = match tail {
            "enabled" => ProcRouteSlot::Enabled,
            "up" => ProcRouteSlot::MoveUp,
            "down" => ProcRouteSlot::MoveDown,
            "remove" => ProcRouteSlot::Remove,
            other => ProcRouteSlot::Field(other.strip_prefix("f:")?),
        };
        Some((idx, slot))
    }
}

/// Per-processor route slots. The key builders on [`PipelineKeys`]
/// produce the strings; [`PipelineKeys::parse_route`] inverts them back
/// to (idx, slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcRouteSlot<'a> {
    Enabled,
    Field(&'a str),
    MoveUp,
    MoveDown,
    Remove,
}

// ============================================================
// Render
// ============================================================

/// Card shell shared by the settings tabs and the pipeline editor: a
/// titled card with content padding, so each section reads as a set of
/// bordered groups instead of a flat run of dividers + headings.
pub fn section_card<I, E>(title: impl Into<String>, children: I) -> El
where
    I: IntoIterator<Item = E>,
    E: Into<El>,
{
    card([
        card_header([card_title(title)]).padding(Sides {
            left: tokens::SPACE_4,
            right: tokens::SPACE_4,
            top: tokens::SPACE_3,
            bottom: tokens::SPACE_2,
        }),
        card_content(children.into_iter().map(Into::into).collect::<Vec<_>>())
            .padding(Sides {
                left: tokens::SPACE_4,
                right: tokens::SPACE_4,
                top: 0.0,
                bottom: tokens::SPACE_4,
            })
            .gap(tokens::SPACE_2),
    ])
}

/// Render one card per stage (plus an empty-state card when the
/// pipeline has no processors). `extra` is called for each *enabled*
/// stage after its schema fields — the Processing tab uses it to append
/// the live output meters for the TX pipeline; other call sites pass a
/// closure returning an empty `Vec`.
pub fn render_stage_cards(
    keys: &PipelineKeys,
    pipeline: &PipelineConfig,
    registry: &ProcessorRegistry,
    selection: &Selection,
    mut extra: impl FnMut(usize, &ProcessorConfig) -> Vec<El>,
) -> Vec<El> {
    let mut cards: Vec<El> = Vec::new();
    let processor_count = pipeline.processors.len();
    // Look up display metadata once per render. `list_available` walks
    // the registry's BTreeMap and allocates a Vec, so doing it inside
    // the per-processor loop would be O(n * m).
    let available: Vec<(String, String, String)> = registry
        .list_available()
        .into_iter()
        .map(|(type_id, name, desc)| (type_id.to_string(), name.to_string(), desc.to_string()))
        .collect();

    if processor_count == 0 {
        cards.push(section_card(
            "Processors",
            [paragraph("No processors in the pipeline. Add one below to get started.").muted()],
        ));
    }

    for (idx, proc_config) in pipeline.processors.iter().enumerate() {
        let (display_name, description) = available
            .iter()
            .find(|(type_id, _, _)| type_id == &proc_config.type_id)
            .map(|(_, name, desc)| (name.clone(), desc.clone()))
            .unwrap_or_else(|| (proc_config.type_id.clone(), String::new()));

        let is_first = idx == 0;
        let is_last = idx + 1 == processor_count;

        let mut move_up_btn = icon_button(IconName::ChevronUp)
            .key(keys.move_up_key(idx))
            .ghost()
            .tooltip("Move up");
        if is_first {
            move_up_btn = move_up_btn.disabled();
        }
        let mut move_down_btn = icon_button(IconName::ChevronDown)
            .key(keys.move_down_key(idx))
            .ghost()
            .tooltip("Move down");
        if is_last {
            move_down_btn = move_down_btn.disabled();
        }
        let remove_btn = icon_button(IconName::X)
            .key(keys.remove_key(idx))
            .ghost()
            .tooltip("Remove from pipeline");

        // Header row: action buttons on the left, enable switch flush
        // right. The `spacer` separates the two clusters and absorbs any
        // extra width as the container resizes.
        let mut body: Vec<El> = Vec::new();
        body.push(
            row([
                move_up_btn,
                move_down_btn,
                remove_btn,
                spacer(),
                switch(keys.enabled_key(idx), proc_config.enabled),
            ])
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .width(Size::Fill(1.0)),
        );
        if !description.is_empty() {
            body.push(paragraph(description).muted().font_size(tokens::TEXT_XS.size));
        }
        if proc_config.enabled
            && let Some(schema) = registry.settings_schema(&proc_config.type_id)
            && let Some(properties) = schema.get("properties").and_then(|p| p.as_object())
        {
            for (key, prop_schema) in properties {
                body.push(render_schema_field(
                    keys,
                    idx,
                    key,
                    prop_schema,
                    &proc_config.settings,
                    selection,
                ));
            }
        }

        // Caller-supplied trailing content (live output meters on the
        // TX editor). Only rendered for enabled stages, mirroring the
        // schema fields above.
        if proc_config.enabled {
            body.extend(extra(idx, proc_config));
        }

        cards.push(section_card(display_name, body));
    }

    cards
}

/// The "Add processor" card that sits at the bottom of the stage list.
/// The trigger always shows the same label — picking an option fires
/// Add and the dropdown closes; there is no "selected" state to display.
pub fn render_add_card(keys: &PipelineKeys) -> El {
    section_card(
        "Add processor",
        [row([
            select_trigger(keys.add_key, "Add a processor…").width(Size::Fixed(280.0)),
            spacer(),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0))],
    )
}

/// The dropdown popover for the "Add processor" trigger. Render only
/// while the caller's open flag is set, on a layer above the panel the
/// trigger lives in.
pub fn render_add_menu(keys: &PipelineKeys, registry: &ProcessorRegistry) -> El {
    select_menu(
        keys.add_key,
        registry
            .list_available()
            .into_iter()
            .map(|(type_id, name, _desc)| (type_id.to_string(), name.to_string())),
    )
}

/// Render one schema-defined property as a labelled control. `number` /
/// `integer` types become sliders (the only general-purpose continuous
/// control damascene ships out of the box), `boolean` becomes a switch,
/// everything else falls back to a text input.
fn render_schema_field(
    keys: &PipelineKeys,
    proc_idx: usize,
    key: &str,
    schema: &JsonValue,
    settings: &JsonValue,
    selection: &Selection,
) -> El {
    let title = schema.get("title").and_then(|t| t.as_str()).unwrap_or(key).to_string();
    let prop_type = schema.get("type").and_then(|t| t.as_str()).unwrap_or("string");
    let route = keys.field_key(proc_idx, key);

    match prop_type {
        "number" => {
            let min = schema.get("minimum").and_then(|m| m.as_f64()).unwrap_or(-100.0) as f32;
            let max = schema.get("maximum").and_then(|m| m.as_f64()).unwrap_or(100.0) as f32;
            let default = schema.get("default").and_then(|d| d.as_f64()).unwrap_or(0.0) as f32;
            let value = settings
                .get(key)
                .and_then(|v| v.as_f64())
                .map(|v| v as f32)
                .unwrap_or(default);
            let normalized = if (max - min).abs() < f32::EPSILON {
                0.0
            } else {
                ((value - min) / (max - min)).clamp(0.0, 1.0)
            };
            field_row(
                format!("{title} ({:.2})", value),
                slider(&route, normalized).width(Size::Fixed(220.0)),
            )
        }
        "integer" => {
            let min = schema.get("minimum").and_then(|m| m.as_i64()).unwrap_or(0);
            let max = schema.get("maximum").and_then(|m| m.as_i64()).unwrap_or(100);
            let default = schema.get("default").and_then(|d| d.as_i64()).unwrap_or(0);
            let value = settings.get(key).and_then(|v| v.as_i64()).unwrap_or(default);
            let span = (max - min).max(1) as f32;
            let normalized = ((value - min) as f32 / span).clamp(0.0, 1.0);
            field_row(
                format!("{title} ({value})"),
                slider(&route, normalized).width(Size::Fixed(220.0)),
            )
        }
        "boolean" => {
            let default = schema.get("default").and_then(|d| d.as_bool()).unwrap_or(false);
            let value = settings.get(key).and_then(|v| v.as_bool()).unwrap_or(default);
            field_row(title, switch(&route, value))
        }
        _ => {
            let default = schema.get("default").and_then(|d| d.as_str()).unwrap_or("");
            let value = settings
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or(default)
                .to_string();
            field_row(title, text_input(&route, &value, selection).width(Size::Fixed(220.0)))
        }
    }
}

// ============================================================
// Event handling
// ============================================================

/// What [`handle_event`] did with the event.
pub enum PipelineEvent {
    /// Not one of this editor's routes — keep dispatching.
    Ignored,
    /// Consumed; `pipeline` was (possibly) mutated in place.
    Changed,
    /// The "Add processor" trigger was clicked — the caller flips its
    /// open flag for the dropdown.
    AddToggle,
    /// The dropdown's outside-click scrim fired — the caller closes it.
    AddDismiss,
    /// An option was picked (the stage is already appended to
    /// `pipeline`) — the caller closes the dropdown.
    AddPicked,
}

/// Route a single event into the editor. Dispatches text/switch/slider
/// events against whichever control shape the schema picked, so the
/// keyboard + pointer codepaths converge into a single update of
/// `pipeline`.
///
/// Slider discipline: pointer drags mutate the *pending* config in
/// place — no backend command fires here. Call sites decide when the
/// accumulated edits actually land (the settings dialog on Save/Apply,
/// the per-user panel likewise), which is what keeps mid-drag frames
/// from spamming the audio task.
pub fn handle_event(
    keys: &PipelineKeys,
    event: &UiEvent,
    pipeline: &mut PipelineConfig,
    registry: &ProcessorRegistry,
    selection: &mut Selection,
) -> PipelineEvent {
    // "Add processor" dropdown. Picking a type appends a fresh
    // `ProcessorConfig` (defaults from the registry) to the end of the
    // pipeline — the user then moves / reorders / configures from
    // there. Duplicates of the same type_id are allowed; the pipeline
    // is keyed by index, not type, and a chain like "gain → denoise →
    // gain" is a legitimate use case.
    if let Some(action) = select::classify_event(event, keys.add_key) {
        return match action {
            select::SelectAction::Toggle => PipelineEvent::AddToggle,
            select::SelectAction::Dismiss => PipelineEvent::AddDismiss,
            select::SelectAction::Pick(value) => {
                if let Some(mut new_config) = registry.default_config(&value) {
                    // Default to enabled — that's what the user just
                    // explicitly asked for. The factory's
                    // `default_settings()` already populates each
                    // schema field, so no backfill is needed.
                    new_config.enabled = true;
                    pipeline.processors.push(new_config);
                }
                PipelineEvent::AddPicked
            }
            _ => PipelineEvent::Changed,
        };
    }

    // Text-input events arrive on `target_key` rather than `route`, so
    // check this before the generic `route()`-driven handlers.
    if let Some(target) = event.target_key()
        && let Some((idx, ProcRouteSlot::Field(field))) = keys.parse_route(target)
        && let Some(proc_config) = pipeline.processors.get_mut(idx)
        && schema_field_type(registry, &proc_config.type_id, field) == Some("string")
    {
        let route = keys.field_key(idx, field);
        let mut value = proc_config
            .settings
            .get(field)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        text_input::apply_event(&mut value, selection, &route, event);
        ensure_object(&mut proc_config.settings)[field] = JsonValue::String(value);
        return PipelineEvent::Changed;
    }

    let Some(route) = event.route() else {
        return PipelineEvent::Ignored;
    };
    let Some((idx, slot)) = keys.parse_route(route) else {
        return PipelineEvent::Ignored;
    };

    // Mutating actions on the pipeline list itself. These can fire
    // even when `idx` would otherwise be out of bounds for a
    // settings access, so handle them before the per-processor
    // `get_mut` below — though in practice the route was just
    // produced by the render at `idx`, so it should always be live.
    if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
        match slot {
            ProcRouteSlot::MoveUp => {
                if idx > 0 && idx < pipeline.processors.len() {
                    pipeline.processors.swap(idx - 1, idx);
                }
                return PipelineEvent::Changed;
            }
            ProcRouteSlot::MoveDown => {
                if idx + 1 < pipeline.processors.len() {
                    pipeline.processors.swap(idx, idx + 1);
                }
                return PipelineEvent::Changed;
            }
            ProcRouteSlot::Remove => {
                if idx < pipeline.processors.len() {
                    pipeline.processors.remove(idx);
                }
                return PipelineEvent::Changed;
            }
            _ => {}
        }
    }

    let Some(proc_config) = pipeline.processors.get_mut(idx) else {
        return PipelineEvent::Ignored;
    };

    match slot {
        // Per-processor enable switch.
        ProcRouteSlot::Enabled => {
            if switch::apply_event(&mut proc_config.enabled, event, route) {
                return PipelineEvent::Changed;
            }
        }
        // Action buttons handled above; we only get here for non-
        // Click/Activate events (e.g. focus traversal), which the
        // ghost icon-button surface absorbs on its own.
        ProcRouteSlot::MoveUp | ProcRouteSlot::MoveDown | ProcRouteSlot::Remove => {}
        // Schema-driven field. The schema lookup is the source of
        // truth for "what control type was rendered" — the event
        // codepaths can't peek at the rendered tree.
        ProcRouteSlot::Field(field) => {
            let prop_type = schema_field_type(registry, &proc_config.type_id, field);
            let prop_schema = registry
                .settings_schema(&proc_config.type_id)
                .and_then(|s| s.get("properties").cloned())
                .and_then(|p| p.get(field).cloned());

            match prop_type {
                Some("boolean") => {
                    // After `merge_with_default_tx_pipeline` runs,
                    // every schema-defined field is present on the
                    // persisted config. The schema-default fallback
                    // is the same one the display path uses
                    // (`render_schema_field`), so a missing field
                    // can't make the toggle's "current value" disagree
                    // with what's painted on screen.
                    let schema_default = prop_schema
                        .as_ref()
                        .and_then(|s| s.get("default"))
                        .and_then(|d| d.as_bool())
                        .unwrap_or(false);
                    let mut value = proc_config
                        .settings
                        .get(field)
                        .and_then(|v| v.as_bool())
                        .unwrap_or(schema_default);
                    if switch::apply_event(&mut value, event, route) {
                        ensure_object(&mut proc_config.settings)[field] = JsonValue::Bool(value);
                        return PipelineEvent::Changed;
                    }
                }
                Some("number") => {
                    if let Some(schema) = prop_schema.as_ref() {
                        let min = schema.get("minimum").and_then(|m| m.as_f64()).unwrap_or(-100.0) as f32;
                        let max = schema.get("maximum").and_then(|m| m.as_f64()).unwrap_or(100.0) as f32;
                        let span = (max - min).max(f32::EPSILON);

                        // Pointer drag.
                        if matches!(
                            event.kind,
                            UiEventKind::PointerDown | UiEventKind::Drag | UiEventKind::Click
                        ) && let (Some(rect), Some(x)) = (event.target_rect(), event.pointer_x())
                        {
                            let n = slider::normalized_from_event(rect, x);
                            let value = (min + n * span).clamp(min, max);
                            ensure_object(&mut proc_config.settings)[field] = serde_json::json!(value as f64);
                            return PipelineEvent::Changed;
                        }

                        // Keyboard nudge.
                        let current = proc_config
                            .settings
                            .get(field)
                            .and_then(|v| v.as_f64())
                            .map(|v| v as f32)
                            .unwrap_or(min);
                        let mut n = ((current - min) / span).clamp(0.0, 1.0);
                        if slider::apply_event(&mut n, event, route, 0.05, 0.25) {
                            let value = (min + n.clamp(0.0, 1.0) * span).clamp(min, max);
                            ensure_object(&mut proc_config.settings)[field] = serde_json::json!(value as f64);
                            return PipelineEvent::Changed;
                        }
                    }
                }
                Some("integer") => {
                    if let Some(schema) = prop_schema.as_ref() {
                        let min = schema.get("minimum").and_then(|m| m.as_i64()).unwrap_or(0);
                        let max = schema.get("maximum").and_then(|m| m.as_i64()).unwrap_or(100);
                        let span = (max - min).max(1) as f32;

                        if matches!(
                            event.kind,
                            UiEventKind::PointerDown | UiEventKind::Drag | UiEventKind::Click
                        ) && let (Some(rect), Some(x)) = (event.target_rect(), event.pointer_x())
                        {
                            let n = slider::normalized_from_event(rect, x);
                            let value = (min as f32 + n * span).round().clamp(min as f32, max as f32) as i64;
                            ensure_object(&mut proc_config.settings)[field] = serde_json::json!(value);
                            return PipelineEvent::Changed;
                        }

                        let current = proc_config.settings.get(field).and_then(|v| v.as_i64()).unwrap_or(min);
                        let mut n = ((current - min) as f32 / span).clamp(0.0, 1.0);
                        if slider::apply_event(&mut n, event, route, 0.05, 0.25) {
                            let value = (min as f32 + n.clamp(0.0, 1.0) * span)
                                .round()
                                .clamp(min as f32, max as f32) as i64;
                            ensure_object(&mut proc_config.settings)[field] = serde_json::json!(value);
                            return PipelineEvent::Changed;
                        }
                    }
                }
                _ => {
                    // String / unknown — text-input branch handles
                    // those above via `target_key`.
                }
            }
        }
    }

    PipelineEvent::Ignored
}

/// Look up the JSON-Schema `type` for a single property of the named
/// processor. Returns `None` when the processor or property is unknown
/// — callers fall through to "ignore the event" in that case.
fn schema_field_type(registry: &ProcessorRegistry, type_id: &str, field: &str) -> Option<&'static str> {
    let schema = registry.settings_schema(type_id)?;
    let properties = schema.get("properties")?.as_object()?;
    let prop = properties.get(field)?;
    let ty = prop.get("type")?.as_str()?;
    // `serde_json::Value` strings are `&str` borrowed from the value
    // itself, not `&'static`. The set of types we care about is small
    // and known, so map them to `'static` literals to avoid leaking
    // the borrow into the caller's lifetime.
    Some(match ty {
        "number" => "number",
        "integer" => "integer",
        "boolean" => "boolean",
        "string" => "string",
        _ => return None,
    })
}

/// `serde_json::Value::object_mut` doesn't exist on stable; this helper
/// makes sure `value` is an object so we can index into it. Replaces a
/// non-object `value` with an empty object as a side effect.
fn ensure_object(value: &mut JsonValue) -> &mut JsonValue {
    if !value.is_object() {
        *value = JsonValue::Object(serde_json::Map::new());
    }
    value
}
