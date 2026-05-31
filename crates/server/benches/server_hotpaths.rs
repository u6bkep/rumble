//! Server hot-path microbenchmarks.
//!
//! Three benchmark groups over the production-critical server paths, all reached
//! through connection-independent seams so no QUIC/network is needed (see
//! `docs/performance-testing.md`):
//!
//! 1. **Voice relay** (`relay_fanout`) — the per-packet routing core
//!    [`server::handlers::build_relay_packet`]: sender resolution, mute/rate
//!    checks, the membership snapshot, server-authoritative re-encode, and
//!    recipient gather. Scales with room size.
//! 2. **State sync** (`state_rebuild` + `state_hash`) — the per-state-change
//!    cost: the full `build_room_list` + `build_user_list` projection and the
//!    BLAKE3 rehash over the assembled `ServerState`. O(rooms × users), paid on
//!    every join/leave/mute/room op.
//! 3. **ACL evaluation** (`acl_eval`) — `evaluate_member_permissions` for one
//!    member, over an increasingly deep room chain.
//!
//! Synthetic state is populated with owned participants (no connection needed).
//!
//! Baseline-gated optimization workflow:
//! ```bash
//! cargo bench -p server -- --save-baseline before
//! # ... make the optimization ...
//! cargo bench -p server -- --baseline before
//! ```

use std::{hint::black_box, sync::Arc};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rumble_protocol::{
    ROOT_ROOM_UUID,
    permissions::Permissions,
    proto::{ServerState as ProtoServerState, VoiceDatagram},
};
use server::{
    Binding, Identity, Member, OwnerId, Persistence, ServerState, acl::evaluate_member_permissions,
    handlers::build_relay_packet, state::compute_server_state_hash,
};
use tokio::runtime::Runtime;

/// An owned (controller-driven) participant — no connection required. Its
/// verified username doubles as its implicit ACL group.
fn participant(uid: u64, groups: &[&str]) -> Arc<Member> {
    let name = format!("user{uid}");
    Arc::new(Member {
        user_id: uid,
        identity: Arc::new(Identity::participant(
            name.clone(),
            Some(name),
            None,
            groups.iter().map(|s| s.to_string()).collect(),
        )),
        binding: Binding::Owned {
            owner: OwnerId::Plugin(1),
        },
    })
}

/// `ServerState` with `n_users` owned participants spread round-robin across
/// `n_rooms` rooms (root included). Rooms are flat children of root.
fn populated_state(rt: &Runtime, n_users: usize, n_rooms: usize) -> ServerState {
    let state = ServerState::new();
    rt.block_on(async {
        let mut rooms = vec![ROOT_ROOM_UUID];
        for r in 0..n_rooms.saturating_sub(1) {
            rooms.push(state.create_room(format!("room{r}")).await);
        }
        for u in 0..n_users {
            let uid = u as u64 + 1;
            state.register_participant(participant(uid, &[]));
            state.set_user_room(uid, rooms[u % rooms.len()]).await;
        }
    });
    state
}

/// Relay one datagram into a single room holding the sender + `n` recipients.
fn bench_relay_fanout(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("relay_fanout");
    for &n in &[10usize, 50, 200] {
        let state = ServerState::new();
        rt.block_on(async {
            for u in 0..=n {
                let uid = u as u64 + 1;
                state.register_participant(participant(uid, &[]));
                state.set_user_room(uid, ROOT_ROOM_UUID).await;
            }
        });
        let sender = 1u64;
        let dgram = VoiceDatagram {
            opus_data: vec![0xAB; 80],
            sequence: 1,
            ..Default::default()
        };

        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                // Reset the 1s rate window so a long sample doesn't trip the
                // limiter mid-run and start measuring the early-return path.
                // The DashMap remove is O(1) and dwarfed by the snapshot+encode.
                state.remove_voice_rate(sender);
                black_box(rt.block_on(build_relay_packet(&state, sender, dgram.clone(), 80)))
            });
        });
    }
    group.finish();
}

/// Full projection + hash over a users × rooms grid.
fn bench_state_sync(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let grid = [(10usize, 4usize), (50, 8), (200, 16)];

    let mut rebuild = c.benchmark_group("state_rebuild");
    for &(users, rooms) in &grid {
        let state = populated_state(&rt, users, rooms);
        let id = format!("{users}u_{rooms}r");
        rebuild.bench_with_input(BenchmarkId::from_parameter(&id), &(), |b, _| {
            b.iter(|| {
                rt.block_on(async {
                    let r = state.build_room_list(&None).await;
                    let u = state.build_user_list().await;
                    black_box((r, u))
                })
            });
        });
    }
    rebuild.finish();

    let mut hash = c.benchmark_group("state_hash");
    for &(users, rooms) in &grid {
        let state = populated_state(&rt, users, rooms);
        let proto = rt.block_on(async {
            ProtoServerState {
                rooms: state.build_room_list(&None).await,
                users: state.build_user_list().await,
                groups: vec![],
                slash_commands: vec![],
            }
        });
        let id = format!("{users}u_{rooms}r");
        hash.bench_with_input(BenchmarkId::from_parameter(&id), &proto, |b, proto| {
            b.iter(|| black_box(compute_server_state_hash(proto)));
        });
    }
    hash.finish();
}

/// Evaluate one member's permissions over a root→leaf room chain of depth `d`.
fn bench_acl_eval(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("acl_eval");
    for &depth in &[1usize, 4, 16] {
        let state = ServerState::new();
        let persist: Option<Arc<Persistence>> = Some(Arc::new(Persistence::in_memory().unwrap()));
        let persist_ref = persist.as_ref().unwrap();
        persist_ref
            .create_group("default", Permissions::TRAVERSE.bits())
            .unwrap();

        // Build a linear room chain root → r1 → … → r(depth-1).
        let leaf = rt.block_on(async {
            let mut parent = ROOT_ROOM_UUID;
            for i in 0..depth.saturating_sub(1) {
                parent = state.create_room_with_parent(format!("r{i}"), Some(parent)).await;
            }
            parent
        });

        let member = participant(1, &[]); // Arc<Member> derefs to &Member at the call site
        group.bench_with_input(BenchmarkId::from_parameter(depth), &leaf, |b, &leaf| {
            b.iter(|| black_box(rt.block_on(evaluate_member_permissions(&state, &member, leaf, &persist))));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_relay_fanout, bench_state_sync, bench_acl_eval);
criterion_main!(benches);
