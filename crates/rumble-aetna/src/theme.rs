//! Mumble-dark inspired palette.
//!
//! Aetna ships its own dark theme tokens (`tokens::BACKGROUND`,
//! `tokens::FOREGROUND`, etc.) which already lean dark-slate. This
//! module adds a few semantic colors specific to the Rumble UI — the
//! green "talking" indicator, the red "muted" indicator, the gold
//! "elevated" badge, and a chat-message-kind palette — sourced from
//! Mumble's default theme so the client feels familiar.

use aetna_core::Color;

/// Mic-on / actively talking. Brighter than the aetna SUCCESS green so
/// it reads as "live" against dark cards.
pub const TALKING: Color = Color::srgb_token("rumble-talking", 76, 209, 99, 255);

/// Mic-off / self-muted (dark red).
pub const MUTED_SELF: Color = Color::srgb_token("rumble-muted-self", 198, 80, 80, 255);

/// Server-muted (locked). Red with a slight magenta tint to distinguish
/// from voluntary self-mute.
pub const MUTED_SERVER: Color = Color::srgb_token("rumble-muted-server", 220, 70, 90, 255);

/// Locally muted other user — yellow bell.
pub const MUTED_LOCAL: Color = Color::srgb_token("rumble-muted-local", 230, 190, 60, 255);

/// Elevated / superuser shield.
pub const ELEVATED: Color = Color::srgb_token("rumble-elevated", 230, 195, 75, 255);

/// Disconnected / idle.
pub const DIM: Color = Color::srgb_token("rumble-dim", 110, 120, 130, 255);

/// Tree-broadcast chat (`/tree`) — pale green.
pub const CHAT_TREE: Color = Color::srgb_token("rumble-chat-tree", 150, 200, 150, 255);

/// Direct message — muted lavender.
pub const CHAT_DM: Color = Color::srgb_token("rumble-chat-dm", 200, 160, 230, 255);

/// Local/system message — italic gray.
pub const CHAT_SYS: Color = Color::srgb_token("rumble-chat-sys", 140, 150, 160, 255);
