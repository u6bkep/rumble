# ACL System

Rumble's access-control system is a hybrid of **server-wide permission groups** and **per-room ACL overrides**, modeled loosely on Mumble. Permissions are a `u32` bitflag set; a user's effective permissions in a given room are computed by unioning their groups' base permissions and then walking the room tree from root to target applying per-room grant/deny deltas. The flag definitions and the pure evaluation function live in `crates/rumble-protocol/src/permissions.rs`; the server-side glue that loads groups/ACLs from sled and feeds them to the evaluator lives in `crates/server/src/acl.rs`. All persistent ACL state (groups, user→group assignments, room ACLs, sudo password) is stored in sled trees in `crates/server/src/persistence.rs`. Identity and membership tie-in is in `crates/server/src/state.rs`.

A companion UI plan is `docs/acl-ui-plan.md` (admin UI surfacing these features in `rumble-damascene`).

## Permission flags

Defined in `permissions.rs` as `bitflags! Permissions: u32`. Values are exact:

### Room-scoped (`ALL_ROOM_SCOPED`)
| Flag | Value | Meaning |
|------|-------|---------|
| `TRAVERSE` | `0x001` | May traverse *through* this room toward a descendant; absence aborts the walk |
| `ENTER` | `0x002` | May join the room |
| `SPEAK` | `0x004` | May transmit voice |
| `TEXT_MESSAGE` | `0x008` | May send text chat |
| `SHARE_FILE` | `0x010` | May share files |
| `MUTE_DEAFEN` | `0x020` | May server-mute/deafen others (moderation) |
| `MOVE_USER` | `0x040` | May move other users between rooms |
| `MAKE_ROOM` | `0x080` | May create child rooms |
| `MODIFY_ROOM` | `0x100` | May edit room metadata |
| `WRITE` | `0x200` | May edit *this room's ACL* |

> `WRITE` is **room-scoped** — it grants the ability to edit a room's ACL entries. It is **not** an "implies all permissions" master bit. The admin group simply has every bit set (`ADMIN_PERMISSIONS = ALL`); there is no single flag that means "all".

### Server-scoped (`ALL_SERVER_SCOPED`) — only meaningful at root
| Flag | Value | Meaning |
|------|-------|---------|
| `KICK` | `0x10000` | Kick a user (disconnect) |
| `BAN` | `0x20000` | Ban a user |
| `REGISTER` | `0x40000` | Register another user |
| `SELF_REGISTER` | `0x80000` | Register oneself |
| `MANAGE_ACL` | `0x100000` | Create/delete/modify groups, edit any room ACL |
| `SUDO` | `0x200000` | Eligible to elevate to superuser (with sudo password) |
| `BANNED` | `0x400000` | Marker flag — presence rejects the user at auth |
| `MANAGE_PARTICIPANTS` | `0x800000` | Authority to mint/drive participants (controllers/bridges) |

`ALL = ALL_ROOM_SCOPED ∪ ALL_SERVER_SCOPED`. Server-scoped bits are stripped from the result whenever the evaluated room is not the root (see evaluation step 5).

### Built-in presets
- `DEFAULT_PERMISSIONS` — `TRAVERSE | ENTER | SPEAK | TEXT_MESSAGE | SHARE_FILE | SELF_REGISTER | MAKE_ROOM | MODIFY_ROOM`. (Note: no `MUTE_DEAFEN`, `MOVE_USER`, or `WRITE`.)
- `ADMIN_PERMISSIONS = ALL`.

`Permissions` serializes as its raw `u32` bits (`Serialize`/`Deserialize` impls at the bottom of `permissions.rs`).

## Groups

Groups are named permission sets stored in the sled `groups` tree (`persistence.rs`, key = UTF-8 group name → `PersistedGroup { permissions: u32 }`).

- **`default`** and **`admin`** are created on first startup by `Persistence::ensure_default_groups()` (only if the `groups` tree is empty) with `DEFAULT_PERMISSIONS` / `ADMIN_PERMISSIONS`.
- **Custom groups** are created at runtime via `CreateGroup` (handler `handle_create_group` in `handlers.rs`), gated on `MANAGE_ACL` at root. `default`/`admin` cannot be re-created; duplicate names are rejected.
- **User→group assignments** live in the `user_groups` tree (key = 32-byte Ed25519 public key → bincode `Vec<String>`). Managed via `add_user_to_group` / `remove_user_from_group` / `set_user_groups`.
- A `controllers` group (granting only `MANAGE_PARTICIPANTS`) is lazily created by the `add-controller` CLI subcommand.

### Implicit username-as-group
Every member that has a **verified identity** additionally belongs to a group named after that verified username. This is derived in `acl::evaluate_identity_permissions` from `identity.verified_username()` — **not** from the display name. Anonymous participants (no verified identity, e.g. Mumble-bridge participants) have `verified_username == None` and therefore never gain an implicit username-group. See `Identity` in `state.rs` (field `verified_username`, doc comment explicitly notes this).

This lets an admin grant per-room permissions to a specific named user by adding an `AclEntry` whose `group` equals that username.

### Collision rule
A custom group name may not collide with a registered username. `handle_create_group` rejects the name if `persist.is_username_registered(&name)` returns true ("Group name conflicts with a registered username"). This keeps the implicit username-group namespace distinct from named groups.

## Per-room ACLs

Stored in the `room_acls` tree (key = 16-byte room UUID → `PersistedRoomAcl`). Each room ACL is:

```
PersistedRoomAcl {
    inherit_acl: bool,          // false => reset to base (drop inherited deltas) at this room
    entries: Vec<PersistedAclEntry>,
}
PersistedAclEntry {
    group: String,              // group name (named group OR a username)
    grant: u32,                 // bits to add
    deny:  u32,                 // bits to remove (deny wins over grant)
    apply_here: bool,           // entry applies when this room is the target
    apply_subs: bool,           // entry applies when this room is an ancestor of the target
}
```

The runtime equivalents are `RoomAclData` / `AclEntry` in `permissions.rs`. Set via `SetRoomAcl` (handler `handle_set_room_acl`, gated on `MANAGE_ACL` or room `WRITE`). Entries are **ordered** and applied in sequence.

## Evaluation algorithm

The pure function is `permissions::effective_permissions(user_groups, group_permissions, room_chain, is_superuser)` in `permissions.rs`. The server wraps it in `acl::evaluate_identity_permissions` (builds `user_groups`, loads group perms from sled, builds the root→target room chain via `build_room_chain_owned`).

Algorithm:

1. **Superuser bypass** — if `is_superuser`, return `ALL` immediately.
2. **Base** = union of the base permissions of every group the member belongs to (always includes `default`, explicitly assigned groups, and the implicit username-group).
3. **Walk the chain root → target.** For each room with ACL data:
   - If `inherit_acl == false`, reset `granted` back to `base` (discard everything inherited from ancestors).
   - For each entry, in order: skip if the member is not in `entry.group`; skip if `is_target && !apply_here`; skip if `!is_target && !apply_subs`. Otherwise apply `granted |= grant; granted &= !deny`. **Deny overrides grant** because the deny mask is applied after the grant within the same entry.
4. **TRAVERSE abort** — after processing each *non-target* room, if `granted` no longer contains `TRAVERSE`, return `Permissions::empty()` (the walk cannot continue through a room the member can't traverse).
5. **Server-scoped strip** — if the chain length > 1 (target is not root), clear all `ALL_SERVER_SCOPED` bits. Server-scoped permissions only count when evaluated at the root room.

`has_permission(effective, required)` is just `effective.contains(required)`. Server-side, `acl::require` produces a `proto::PermissionDenied` with the required-bits and room id on failure; `check_permission` / `check_member_permission` are the entry points used by handlers.

The unit tests at the bottom of `permissions.rs` exercise each branch (deny, TRAVERSE abort, `inherit_acl=false` reset, `apply_here`-only, server-scoped-at-root-only).

## Server mute

Two interacting AtomicBools live on `ClientHandle` (`state.rs`): `server_muted` (effective state checked during voice relay) and `manually_server_muted` (a moderator's explicit override). `UserStatus.server_muted` mirrors the effective state for the roster.

- **Auto-mute on SPEAK-denied join** — in `handle_join_room` (`handlers.rs` ~line 1059): after a move, the server evaluates `SPEAK` in the new room. `should_server_mute = speak_denied || manually_muted`. The change is broadcast on **both** transitions (mute and unmute) via `UserStatusChanged`.
- **Manual mute via `MUTE_DEAFEN`** — `handle_set_server_mute` (`SetServerMute`) requires `MUTE_DEAFEN` in the target's room, sets `manually_server_muted`, then recomputes the effective `server_muted`. When unmuting, it re-evaluates `SPEAK` so a user in a SPEAK-denied room stays muted.
- When a room ACL changes, `handle_set_room_acl` re-evaluates `SPEAK` for all users currently in that room.
- The voice relay drops packets from server-muted senders (`handlers.rs` ~line 1758, checking `server_muted`).

## Superuser / SUDO bypass

Elevation is **session-only** (no persistence of the elevated state):

- `ClientHandle.is_superuser: AtomicBool` (and `Member::is_superuser()`, true only for `Binding::Client`).
- `handle_elevate` (`Elevate` message): loads the bcrypt sudo-password hash from the `sudo_password` tree, verifies `elev.password` with `bcrypt::verify`, and on success sets `is_superuser = true` and broadcasts `is_elevated`.
- When `is_superuser`, `effective_permissions` returns `ALL` (step 1), bypassing all group and room-ACL evaluation.
- Elevated superusers cannot be kicked or banned (`handle_kick_user` / `handle_ban_user` reject targets with `is_superuser`).

> Eligibility to elevate is conceptually the `SUDO` flag, but `handle_elevate` gates purely on knowing the configured sudo password (bcrypt verify); it does not separately re-check the `SUDO` bit.

## Bootstrap CLI

Admin subcommands run against the sled DB and exit (`handle_subcommand` in `crates/server/src/main.rs`). All take an optional trailing `<data-dir>` (default `data`, DB at `<data-dir>/rumble.db`):

- `server add-admin <base64-public-key>` — adds the key to the `admin` group (`ensure_default_groups` first).
- `server set-sudo-password <password>` — bcrypt-hashes and stores the sudo password.
- `server add-controller <base64-public-key>` — ensures the `controllers` group (granting `MANAGE_PARTICIPANTS`) exists, then adds the key to it.
- `server set-participant-group <base64-public-key> <group>` — sets the default participant group a controller's anonymous participants inherit (`participant_defaults` tree, separate from the controller's own group so participants never inherit `MANAGE_PARTICIPANTS`).

## Kick vs ban

- **Kick** (`handle_kick_user`, requires `KICK`) — disconnect only, no persistence. Rejects elevated superusers.
- **Ban** (`handle_ban_user`, requires `BAN`) — sled-persisted. Ensures a `banned` group exists with the `BANNED` flag, adds the target's public key to it (and updates cached groups if connected), then kicks them with a ban message. `BanUser` carries `duration_seconds` (0 = permanent) and an optional reason. Rejects superusers and bridge connections.
- **Ban enforcement at auth** — in the auth handler (`handlers.rs` ~line 405, step "4b"): after signature verification the server loads the key's groups (+ implicit username-group), evaluates permissions at root, and if `BANNED` is present rejects with "You are banned from this server".

## Proto wire fields

ACL-related payloads occupy proto fields 80–93 in `crates/rumble-protocol/proto/api.proto`: `KickUser`=80, `BanUser`=81, `SetServerMute`=83, `Elevate`=84, `CreateGroup`=85, `DeleteGroup`=86, `ModifyGroup`=87, `SetUserGroup`=88, `SetRoomAcl`=89, `PermissionDenied`=91, `UserKicked`=92. State updates include `GroupChanged` and `UserGroupChanged`. `proto::User` carries `server_muted` / `is_elevated` for the roster.
