# Rumble icons

SVGs lifted directly from `mumble-voip/mumble/themes/Default/`
(LGPL-3.0-or-later, same license as Mumble itself; redistributable
under the GPL-2.0+ in the rumble project).

## Wiring icons in

Damascene's `icon()` builder accepts both built-in `IconName` values and
app-supplied SVGs through `SvgIcon`. The pattern: parse each SVG once
into a `LazyLock<SvgIcon>` static, then `icon(MY_GLYPH.clone())` at
the call site. Cloning is a cheap `Arc` bump, and content-hashing
inside `damascene-core` dedups the MSDF atlas slot automatically.

```rust
use std::sync::LazyLock;
use damascene_core::prelude::*;

static SVG_TALKING_ON: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse(include_str!("../assets/icons/talking_on.svg"))
        .expect("talking_on.svg parses")
});

icon(SVG_TALKING_ON.clone()).icon_size(12.0)
```

The Mumble theme bakes its semantic colors into each SVG (red for
self-mute, blue for talking, etc.), so we use `SvgIcon::parse` rather
than `SvgIcon::parse_current_color` — the authored fill *is* the
visual signal, and `.text_color(...)` would override it. Reach for
`parse_current_color` only on monochrome lucide-style glyphs that
need theme tinting.

## What's wired today

User-state glyphs in `room_tree.rs::push_room_subtree`. The first five
form the *primary voice-state glyph* (one per user, by priority: server →
deafen → self-mute → talking → idle); deafen supersedes self-mute because
deafening implies muting client-side. Local mute is orthogonal (our own
choice not to hear a peer), so it rides as an additive second badge.

| State | SVG | Kind |
|---|---|---|
| Talking | `talking_on.svg` | primary |
| Idle | `talking_off.svg` | primary |
| Self-muted | `muted_self.svg` | primary |
| Self-deafened | `deafened_self.svg` | primary |
| Server-muted | `muted_server.svg` | primary |
| Locally muted (by us) | `muted_local.svg` | additive badge |

Top toolbar (`app.rs::top_toolbar`):

| Control | SVG |
|---|---|
| Mute toggle (active mic) | `talking_off.svg` |
| Mute toggle (muted) | `muted_self.svg` |
| Deafen toggle (active sound) | `self_undeafened.svg` |
| Deafen toggle (deafened) | `deafened_self.svg` |
| Voice mode trigger (Continuous/VAD) | `talking_off.svg` |
| Voice mode trigger (PTT) | `muted_pushtomute.svg` |
| Disconnect | `disconnect.svg` |

## Index — wire as needed

- `talking_on.svg` / `talking_off.svg` — speaking indicators (wired).
- `muted_self.svg` / `muted_pushtomute.svg` / `muted_suppressed.svg` — mic-off variants.
- `muted_server.svg` — server-imposed mute (wired).
- `muted_local.svg` — locally muted other user (wired, additive badge).
- `deafened_self.svg` — self-deafened (wired in room tree + toolbar).
- `deafened_server.svg` / `self_undeafened.svg` — deafen states (`self_undeafened` wired in toolbar; `deafened_server` unused — no `server_deafened` proto field yet).
- `channel.svg` / `channel_active.svg` — currently empty `<svg>` placeholders in the upstream theme. Use `IconName::Folder` for the room glyph until rumble ships its own.
- `authenticated.svg` — registered-user indicator.
- `priority_speaker.svg` — priority-speaker badge.
- `filter_on.svg` / `filter_off.svg` — chat/user-list filter toggle.
- `disconnect.svg` — disconnect button.
- `comment.svg` / `comment_seen.svg` — chat / unread indicator.
- `lock_locked.svg` / `lock_unlocked.svg` — ACL / privacy.
- `arrow_left.svg` — back / collapse.
- `pin.svg` — pinned message / room.
