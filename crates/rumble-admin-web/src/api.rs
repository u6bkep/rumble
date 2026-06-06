//! Browser `fetch` client for the server's REST control-plane.
//!
//! Each public function spawns its request on the wasm microtask queue
//! (`spawn_local`) and funnels the typed outcome back into the shared
//! [`Inbox`] — the app never blocks the frame loop on a network round-trip.
//! Cookies ride along automatically: the UI is served from the same origin
//! as the API, so the `SameSite=Strict` session cookie is included by the
//! default same-origin credentials mode.
//!
//! Mutating calls, on success, both report an `ActionOk` status line and
//! kick off a fresh [`fetch_state`] so the dashboard reflects the change.

use std::{cell::RefCell, rc::Rc};

use rumble_web_types::{
    BanRequest, BootstrapRequest, CreateGroupRequest, CreateRoomRequest, KickRequest, LoginRequest, ModifyGroupRequest,
    OkMessage, SessionInfo, SetRoomAclRequest, SetUserGroupRequest, StateSnapshot,
};

use crate::inbox::{Inbox, Msg};

/// Shared handle to the app's mailbox.
pub type Inb = Rc<RefCell<Inbox>>;

// --- transport --------------------------------------------------------------
//
// The browser build performs real `fetch`es on the wasm microtask queue; the
// native build stubs the same surface so the `App` (and its `build` projection)
// type-checks and renders on the host for the `lint` binary, without pulling in
// the wasm-only browser deps. The stubs are never polled — `spawn_local` drops
// the future on the host — so they only need to type-check.

#[cfg(target_arch = "wasm32")]
use transport::{delete_empty, get, patch_json, post_empty, post_json, put_json};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::spawn_local;

#[cfg(target_arch = "wasm32")]
mod transport {
    use gloo_net::http::Request;
    use serde::{Serialize, de::DeserializeOwned};

    use rumble_web_types::ApiError;

    /// Send a built request and decode the JSON body, mapping a non-2xx
    /// response to the server's `{ "error": ... }` reason where present.
    async fn send<T: DeserializeOwned>(req: Request) -> Result<T, String> {
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if resp.ok() {
            resp.json::<T>().await.map_err(|e| format!("decode error: {e}"))
        } else {
            let status = resp.status();
            match resp.json::<ApiError>().await {
                Ok(err) => Err(err.error),
                Err(_) => Err(format!("HTTP {status}")),
            }
        }
    }

    pub async fn get<T: DeserializeOwned>(url: &str) -> Result<T, String> {
        let req = Request::get(url).build().map_err(|e| e.to_string())?;
        send(req).await
    }

    pub async fn post_json<B: Serialize, T: DeserializeOwned>(url: &str, body: &B) -> Result<T, String> {
        let req = Request::post(url).json(body).map_err(|e| e.to_string())?;
        send(req).await
    }

    pub async fn patch_json<B: Serialize, T: DeserializeOwned>(url: &str, body: &B) -> Result<T, String> {
        let req = Request::patch(url).json(body).map_err(|e| e.to_string())?;
        send(req).await
    }

    pub async fn put_json<B: Serialize, T: DeserializeOwned>(url: &str, body: &B) -> Result<T, String> {
        let req = Request::put(url).json(body).map_err(|e| e.to_string())?;
        send(req).await
    }

    pub async fn post_empty<T: DeserializeOwned>(url: &str) -> Result<T, String> {
        let req = Request::post(url).build().map_err(|e| e.to_string())?;
        send(req).await
    }

    pub async fn delete_empty<T: DeserializeOwned>(url: &str) -> Result<T, String> {
        let req = Request::delete(url).build().map_err(|e| e.to_string())?;
        send(req).await
    }
}

// Native stubs: the host never performs IO, so every request resolves to an
// error and the dropped future is never polled.
#[cfg(not(target_arch = "wasm32"))]
use host_stub::*;

#[cfg(not(target_arch = "wasm32"))]
mod host_stub {
    use serde::{Serialize, de::DeserializeOwned};

    const OFFLINE: &str = "network unavailable on host";

    pub fn spawn_local<F: core::future::Future<Output = ()> + 'static>(_fut: F) {}

    pub async fn get<T: DeserializeOwned>(_url: &str) -> Result<T, String> {
        Err(OFFLINE.to_string())
    }
    pub async fn post_json<B: Serialize, T: DeserializeOwned>(_url: &str, _body: &B) -> Result<T, String> {
        Err(OFFLINE.to_string())
    }
    pub async fn patch_json<B: Serialize, T: DeserializeOwned>(_url: &str, _body: &B) -> Result<T, String> {
        Err(OFFLINE.to_string())
    }
    pub async fn put_json<B: Serialize, T: DeserializeOwned>(_url: &str, _body: &B) -> Result<T, String> {
        Err(OFFLINE.to_string())
    }
    pub async fn post_empty<T: DeserializeOwned>(_url: &str) -> Result<T, String> {
        Err(OFFLINE.to_string())
    }
    pub async fn delete_empty<T: DeserializeOwned>(_url: &str) -> Result<T, String> {
        Err(OFFLINE.to_string())
    }
}

/// Push a message into the mailbox (wakes the render loop).
fn deliver(inbox: &Inb, msg: Msg) {
    inbox.borrow_mut().push(msg);
}

/// Common tail for a mutating call: report the success line and refresh the
/// dashboard snapshot, or report the failure.
fn settle_action(inbox: Inb, res: Result<OkMessage, String>) {
    match res {
        Ok(ok) => {
            deliver(&inbox, Msg::ActionOk(ok.message));
            fetch_state(inbox);
        }
        Err(e) => deliver(&inbox, Msg::Error(e)),
    }
}

// --- auth & bootstrap -------------------------------------------------------

pub fn fetch_session(inbox: Inb) {
    spawn_local(async move {
        let msg = match get::<SessionInfo>("/api/session").await {
            Ok(s) => Msg::Session(s),
            Err(e) => Msg::Error(format!("session check failed: {e}")),
        };
        deliver(&inbox, msg);
    });
}

pub fn login(inbox: Inb, password: String) {
    spawn_local(async move {
        let req = LoginRequest { password };
        let msg = match post_json::<_, OkMessage>("/api/login", &req).await {
            Ok(_) => Msg::LoginOk,
            Err(e) => Msg::Error(e),
        };
        deliver(&inbox, msg);
    });
}

pub fn bootstrap(inbox: Inb, req: BootstrapRequest) {
    spawn_local(async move {
        let msg = match post_json::<_, OkMessage>("/api/bootstrap", &req).await {
            Ok(_) => Msg::Bootstrapped,
            Err(e) => Msg::Error(e),
        };
        deliver(&inbox, msg);
    });
}

pub fn logout(inbox: Inb) {
    spawn_local(async move {
        let msg = match post_empty::<OkMessage>("/api/logout").await {
            Ok(_) => Msg::LoggedOut,
            Err(e) => Msg::Error(e),
        };
        deliver(&inbox, msg);
    });
}

// --- monitoring -------------------------------------------------------------

pub fn fetch_state(inbox: Inb) {
    spawn_local(async move {
        let msg = match get::<StateSnapshot>("/api/state").await {
            Ok(s) => Msg::State(s),
            Err(e) => Msg::Error(format!("state refresh failed: {e}")),
        };
        deliver(&inbox, msg);
    });
}

// --- mutations --------------------------------------------------------------

pub fn create_room(inbox: Inb, req: CreateRoomRequest) {
    spawn_local(async move {
        let res = post_json::<_, OkMessage>("/api/rooms", &req).await;
        settle_action(inbox, res);
    });
}

pub fn delete_room(inbox: Inb, id: String) {
    spawn_local(async move {
        let res = delete_empty::<OkMessage>(&format!("/api/rooms/{id}")).await;
        settle_action(inbox, res);
    });
}

pub fn create_group(inbox: Inb, req: CreateGroupRequest) {
    spawn_local(async move {
        let res = post_json::<_, OkMessage>("/api/groups", &req).await;
        settle_action(inbox, res);
    });
}

pub fn delete_group(inbox: Inb, name: String) {
    spawn_local(async move {
        let res = delete_empty::<OkMessage>(&format!("/api/groups/{name}")).await;
        settle_action(inbox, res);
    });
}

pub fn modify_group(inbox: Inb, name: String, permissions: u32) {
    spawn_local(async move {
        let req = ModifyGroupRequest { permissions };
        let res = patch_json::<_, OkMessage>(&format!("/api/groups/{name}"), &req).await;
        settle_action(inbox, res);
    });
}

pub fn set_room_acl(inbox: Inb, room_id: String, req: SetRoomAclRequest) {
    spawn_local(async move {
        let res = put_json::<_, OkMessage>(&format!("/api/rooms/{room_id}/acl"), &req).await;
        settle_action(inbox, res);
    });
}

pub fn kick_user(inbox: Inb, user_id: u64, reason: String) {
    spawn_local(async move {
        let req = KickRequest { reason };
        let res = post_json::<_, OkMessage>(&format!("/api/users/{user_id}/kick"), &req).await;
        settle_action(inbox, res);
    });
}

pub fn ban_user(inbox: Inb, user_id: u64, duration_seconds: u64, reason: String) {
    spawn_local(async move {
        let req = BanRequest {
            duration_seconds,
            reason,
        };
        let res = post_json::<_, OkMessage>(&format!("/api/users/{user_id}/ban"), &req).await;
        settle_action(inbox, res);
    });
}

pub fn register_user(inbox: Inb, user_id: u64) {
    spawn_local(async move {
        let res = post_empty::<OkMessage>(&format!("/api/users/{user_id}/register")).await;
        settle_action(inbox, res);
    });
}

/// Add or remove a registered user (by its URL-safe-base64 public key) to/from
/// `group`. Drives the per-user group editor; each toggle is one call.
pub fn set_user_group(inbox: Inb, public_key: String, group: String, add: bool) {
    spawn_local(async move {
        let req = SetUserGroupRequest {
            group,
            add,
            expires_at: 0,
        };
        let res = post_json::<_, OkMessage>(&format!("/api/registered-users/{public_key}/groups"), &req).await;
        settle_action(inbox, res);
    });
}
