//! Echo-bot plugin — an audio round-trip tester built on the plugin audio API.
//!
//! A user runs the `/echo` slash command to summon an "Echo" participant into
//! their current room. The bot [subscribes to the room's voice][sub], filters
//! to the summoner's own frames, and [re-emits them][send] as its own voice, so
//! the summoner hears themselves a moment later — a quick check that capture,
//! encode, transport, decode, and playback all work end to end. `/echo stop`
//! (or disconnecting) removes the bot.
//!
//! It exercises both halves of the server plugin extension surface:
//!
//! - **Slash commands** — [`ServerPlugin::commands`] / [`ServerPlugin::on_command`].
//! - **Audio subscription + egress** — [`ParticipantHandle::subscribe_room_audio`]
//!   and [`ParticipantHandle::send_voice`].
//!
//! Unlike the [link cleaner][crate::link_cleaner], which is a *hidden* text-only
//! participant, the echo bot is *present*: it [moves into the room][mv] so it
//! shows in the tree, which is exactly how "voice presence" is meant to work —
//! a participant gains it by joining a room to listen.
//!
//! ## Why only the summoner's audio
//!
//! The bot re-emits everything under its single `sender_id`. If it echoed every
//! speaker, the client would feed frames from several sources into one peer
//! decoder/jitter buffer and produce garbage. Echoing exactly one source keeps
//! the bot's outgoing stream coherent.
//!
//! [sub]: ParticipantHandle::subscribe_room_audio
//! [send]: ParticipantHandle::send_voice
//! [mv]: ParticipantHandle::move_to_room

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    plugin::{AudioSubscription, CommandSpec, ParticipantHandle, PluginFactory, ServerCtx, ServerPlugin},
    state::ClientHandle,
};

/// Bounded depth of an echo subscription's frame channel. At 20 ms/frame this
/// is several seconds of slack; if the echo task ever falls this far behind,
/// older frames are dropped rather than stalling the relay.
const CHANNEL_CAPACITY: usize = 256;

/// A live echo bot bound to one summoner.
struct EchoSession {
    /// The echo participant's user_id (for addressing it in chat replies).
    echo_id: u64,
    /// The room the bot joined; its audio subscription is bound to it.
    room: Uuid,
    /// The relay task; aborting it drops the [`ParticipantHandle`] (removing the
    /// participant) and the [`AudioSubscription`] (unsubscribing).
    task: JoinHandle<()>,
}

/// The echo-bot plugin. Holds one [`EchoSession`] per summoning user.
#[derive(Default)]
pub struct EchoBotPlugin {
    sessions: Mutex<HashMap<u64, EchoSession>>,
}

impl EchoBotPlugin {
    /// Construct an echo-bot plugin with no active sessions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Tear down the session owned by `summoner_id`, if any. Returns the
    /// removed session so the caller can post a farewell before the participant
    /// disappears.
    fn take_session(&self, summoner_id: u64) -> Option<EchoSession> {
        self.sessions.lock().unwrap().remove(&summoner_id)
    }

    /// `/echo` — summon (or report an existing) echo bot for `sender`.
    async fn start(&self, sender: &Arc<ClientHandle>, ctx: &ServerCtx) -> Result<()> {
        let summoner_id = sender.user_id;

        // The summoner must be in a room for the bot to join and listen.
        let Some(room) = ctx.get_user_room_async(summoner_id).await else {
            return Ok(());
        };

        // Already running? Nudge via the existing bot rather than spawning a
        // second one.
        if let Some((echo_id, room)) = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(&summoner_id).map(|s| (s.echo_id, s.room))
        } {
            let _ = ctx
                .post_chat_as(
                    echo_id,
                    room,
                    "Echo bot is already running — `/echo stop` to dismiss.".to_string(),
                )
                .await;
            return Ok(());
        }

        // Name the bot after the summoner so concurrent testers stay distinct.
        let summoner_name = match ctx.state().get_member(summoner_id) {
            Some(m) => m.identity.display_name().await,
            None => "?".to_string(),
        };
        let handle = ctx
            .register_participant(format!("Echo→{summoner_name}"), Some("echo".to_string()), Vec::new())
            .await?;
        let echo_id = handle.user_id();

        // Joining the room makes the bot present (visible in the tree) and is
        // what its audio subscription keys off of.
        if !handle.move_to_room(room).await? {
            warn!(summoner_id, ?room, "echo-bot: summoner's room vanished before join");
            return Ok(()); // handle drops → participant cleaned up
        }
        let _ = handle
            .post_chat(
                "🔊 Echo bot listening — speak and you'll hear yourself back. `/echo stop` to dismiss.".to_string(),
            )
            .await;

        let sub = handle.subscribe_room_audio(room, CHANNEL_CAPACITY);
        let task = tokio::spawn(echo_loop(handle, sub, summoner_id));

        info!(summoner_id, echo_id, ?room, "echo-bot: started");
        if let Some(old) = self
            .sessions
            .lock()
            .unwrap()
            .insert(summoner_id, EchoSession { echo_id, room, task })
        {
            // Lost a race with a concurrent `/echo`; keep the newest, abort the
            // older task (its participant is removed on drop).
            old.task.abort();
        }
        Ok(())
    }

    /// `/echo stop` — dismiss the summoner's echo bot.
    async fn stop_session(&self, sender: &Arc<ClientHandle>, ctx: &ServerCtx) -> Result<()> {
        match self.take_session(sender.user_id) {
            Some(session) => {
                let _ = ctx
                    .post_chat_as(session.echo_id, session.room, "Echo bot dismissed.".to_string())
                    .await;
                session.task.abort();
                info!(summoner_id = sender.user_id, "echo-bot: stopped");
            }
            None => {
                // Nothing to stop; stay quiet rather than spamming the room.
            }
        }
        Ok(())
    }
}

/// Relay loop: forward the summoner's frames back out as the bot's own voice.
///
/// Owns `handle` and `sub` for its lifetime; when the task is aborted both drop,
/// removing the participant and unsubscribing. The bot's own re-emitted frames
/// are filtered out by [`crate::state::ServerState::fanout_audio`]'s
/// loopback-skip, so this never feeds itself.
async fn echo_loop(handle: ParticipantHandle, mut sub: AudioSubscription, summoner_id: u64) {
    while let Some(frame) = sub.recv().await {
        if frame.sender_id != summoner_id {
            continue;
        }
        handle
            .send_voice(frame.opus_data, frame.sequence, frame.end_of_stream)
            .await;
    }
}

#[async_trait::async_trait]
impl ServerPlugin for EchoBotPlugin {
    fn name(&self) -> &str {
        "echo-bot"
    }

    fn commands(&self) -> Vec<CommandSpec> {
        vec![CommandSpec::new(
            "echo",
            "Summon an echo bot that plays your audio back; `/echo stop` to dismiss",
        )]
    }

    async fn on_command(&self, _cmd: &str, args: &str, sender: &Arc<ClientHandle>, ctx: &ServerCtx) -> Result<()> {
        if args.eq_ignore_ascii_case("stop") {
            self.stop_session(sender, ctx).await
        } else {
            self.start(sender, ctx).await
        }
    }

    async fn on_message(
        &self,
        _envelope: &rumble_protocol::proto::Envelope,
        _sender: &Arc<ClientHandle>,
        _ctx: &ServerCtx,
    ) -> Result<bool> {
        // Command-driven only; never consumes ordinary chat.
        Ok(false)
    }

    async fn on_disconnect(&self, client: &Arc<ClientHandle>, _ctx: &ServerCtx) {
        if let Some(session) = self.take_session(client.user_id) {
            session.task.abort();
            info!(summoner_id = client.user_id, "echo-bot: summoner disconnected, stopped");
        }
    }

    async fn stop(&self) -> Result<()> {
        let sessions: Vec<EchoSession> = self.sessions.lock().unwrap().drain().map(|(_, s)| s).collect();
        for session in sessions {
            session.task.abort();
        }
        Ok(())
    }
}

/// Factory for [`EchoBotPlugin`]. Takes no configuration.
pub struct EchoBotFactory;

impl PluginFactory for EchoBotFactory {
    fn name(&self) -> &str {
        "echo-bot"
    }

    fn create(&self, _config: Option<toml::Value>) -> anyhow::Result<Box<dyn ServerPlugin>> {
        Ok(Box::new(EchoBotPlugin::new()))
    }
}
