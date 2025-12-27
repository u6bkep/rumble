# notes

## backend

- `backend::handle::BackendHandle::send` is marked as non-blocking, but it actually is blocking. the backend api should be fully non-blocking.
  - the gui initiates a connect on startup, which blocks the gui thread until the connection is established.
- `backend::events::BackendEvent` may have redundant variants. `BackendEvent::ConnectionStatusChanged` and any of the more specific connection status events (e.g. `BackendEvent::Connected`, `BackendEvent::Disconnected`) may be redundant. probably only need the specific events.
- audio assumes push to talk. there should be events for ptt state changes, as well as a way to set continuous transmission mode, and mute mode.
- backend exposes a poll event api in `backend::handle::BackendHandle::poll_event`. for imidiate mode UIs, this should be a clonable state object, and a callback to trigger redraws when state changes. there should also still be an event api for other kinds of UIs.
- backend running should be independent of both connection state and audio configuration so that the client can run and configure itself before connecting or enabling audio, as well as allow audio to be re configured while connected. APIs to the UI that depend on connection state or audio configuration should return errors if those preconditions are not met.


## api
- need audio transmit start and stop messages to help with decoder state and a small initial playback delay.
- too much redundant information in messages
  -  `UserPresence` is a code smell. when sending a full state update, server should send a list of channels, then a list of users each with the id of the channel they are in.
  -  redundant event messages. there should be one event message per event. for example, when a user changes channels, the client will send a change channel event with their own user id, then the server will validate permission, calculate a new state hash, and forward the same event to all clients including the original sender. if the server rejects the update, it should send a reject message with a denied reson. this allows a sufficiently privileged client to move other users around. before ACLs are implemented, the server should reject change channel events for other users.


## anylitics
- audio pipeline keeps counters for frames dropped in various stages, mic capture buffer, jitter buffer overflows, anything that might indicate client or network performance issues.
  - need to be carefull about when various counters are initialized and reset so audio stopping and starting doesn't delete useful data. maybe make the counters part of the audio state that persists across stops and starts.
- new backend anylitics task periodically reads backend state and sends anylitics stats to server in anylitics messages.
- server logs anylitics counters per user session, collecting things like time and session length with the various stats from the client.
- server saves anylitics data to the database for later analysis.