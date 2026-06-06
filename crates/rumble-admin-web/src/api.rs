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

use gloo_net::http::Request;
use serde::{Serialize, de::DeserializeOwned};
use wasm_bindgen_futures::spawn_local;

use rumble_web_types::{
    ApiError, BanRequest, BootstrapRequest, CreateGroupRequest, CreateRoomRequest, KickRequest, LoginRequest,
    ModifyGroupRequest, OkMessage, SessionInfo, SetRoomAclRequest, StateSnapshot,
};

use crate::inbox::{Inbox, Msg};

/// Shared handle to the app's mailbox.
pub type Inb = Rc<RefCell<Inbox>>;

// --- low-level helpers ------------------------------------------------------

/// Send a built request and decode the JSON body, mapping a non-2xx response
/// to the server's `{ "error": ... }` reason where present.
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

async fn get<T: DeserializeOwned>(url: &str) -> Result<T, String> {
    let req = Request::get(url).build().map_err(|e| e.to_string())?;
    send(req).await
}

async fn post_json<B: Serialize, T: DeserializeOwned>(url: &str, body: &B) -> Result<T, String> {
    let req = Request::post(url).json(body).map_err(|e| e.to_string())?;
    send(req).await
}

async fn patch_json<B: Serialize, T: DeserializeOwned>(url: &str, body: &B) -> Result<T, String> {
    let req = Request::patch(url).json(body).map_err(|e| e.to_string())?;
    send(req).await
}

async fn put_json<B: Serialize, T: DeserializeOwned>(url: &str, body: &B) -> Result<T, String> {
    let req = Request::put(url).json(body).map_err(|e| e.to_string())?;
    send(req).await
}

async fn post_empty<T: DeserializeOwned>(url: &str) -> Result<T, String> {
    let req = Request::post(url).build().map_err(|e| e.to_string())?;
    send(req).await
}

async fn delete_empty<T: DeserializeOwned>(url: &str) -> Result<T, String> {
    let req = Request::delete(url).build().map_err(|e| e.to_string())?;
    send(req).await
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
