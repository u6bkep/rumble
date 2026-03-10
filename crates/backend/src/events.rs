//! Backend state and commands — re-exported from api.
//!
//! Types have been moved to `api::types` for broader reuse.
//! This module re-exports them for backward compatibility.

pub use api::types::*;

/// Re-export ROOT_ROOM_UUID for convenience
pub use api::ROOT_ROOM_UUID;

#[cfg(test)]
mod tests {
    use super::*;
    use api::{
        proto::{RoomInfo, User, UserId},
        room_id_from_uuid,
    };
    use std::collections::HashMap;
    use uuid::Uuid;

    #[test]
    fn test_state_users_in_room() {
        let room1_uuid = ROOT_ROOM_UUID;
        let room2_uuid = Uuid::new_v4();

        let state = State {
            connection: ConnectionState::Connected {
                server_name: "Test".to_string(),
                user_id: 1,
            },
            my_user_id: Some(1),
            my_room_id: Some(room1_uuid),
            my_session_public_key: None,
            my_session_id: None,
            rooms: vec![
                RoomInfo {
                    id: Some(room_id_from_uuid(room1_uuid)),
                    name: "Root".to_string(),
                    parent_id: None,
                    description: None,
                    inherit_acl: true,
                    acls: vec![],
                    effective_permissions: 0,
                },
                RoomInfo {
                    id: Some(room_id_from_uuid(room2_uuid)),
                    name: "Room2".to_string(),
                    parent_id: None,
                    description: None,
                    inherit_acl: true,
                    acls: vec![],
                    effective_permissions: 0,
                },
            ],
            users: vec![
                User {
                    user_id: Some(UserId { value: 1 }),
                    current_room: Some(room_id_from_uuid(room1_uuid)),
                    username: "user1".to_string(),
                    is_muted: false,
                    is_deafened: false,
                    server_muted: false,
                    is_elevated: false,
                    groups: vec![],
                },
                User {
                    user_id: Some(UserId { value: 2 }),
                    current_room: Some(room_id_from_uuid(room1_uuid)),
                    username: "user2".to_string(),
                    is_muted: false,
                    is_deafened: false,
                    server_muted: false,
                    is_elevated: false,
                    groups: vec![],
                },
                User {
                    user_id: Some(UserId { value: 3 }),
                    current_room: Some(room_id_from_uuid(room2_uuid)),
                    username: "user3".to_string(),
                    is_muted: false,
                    is_deafened: false,
                    server_muted: false,
                    is_elevated: false,
                    groups: vec![],
                },
            ],
            audio: AudioState::default(),
            chat_messages: vec![],
            file_transfers: vec![],
            file_transfer_settings: FileTransferSettings::default(),
            room_tree: RoomTree::default(),
            p2p_peers: HashMap::new(),
            effective_permissions: 0,
            per_room_permissions: HashMap::new(),
            permission_denied: None,
            kicked: None,
            group_definitions: vec![],
        };

        let users_in_room1 = state.users_in_room(room1_uuid);
        assert_eq!(users_in_room1.len(), 2);

        let users_in_room2 = state.users_in_room(room2_uuid);
        assert_eq!(users_in_room2.len(), 1);
        assert_eq!(users_in_room2[0].username, "user3");
    }

    #[test]
    fn test_state_get_room() {
        let room_uuid = ROOT_ROOM_UUID;
        let state = State {
            rooms: vec![RoomInfo {
                id: Some(room_id_from_uuid(room_uuid)),
                name: "Root".to_string(),
                parent_id: None,
                description: None,
                inherit_acl: true,
                acls: vec![],
                effective_permissions: 0,
            }],
            ..Default::default()
        };

        let room = state.get_room(room_uuid);
        assert!(room.is_some());
        assert_eq!(room.unwrap().name, "Root");

        assert!(state.get_room(Uuid::new_v4()).is_none());
    }

    #[test]
    fn test_room_tree_rebuild() {
        let root_uuid = ROOT_ROOM_UUID;
        let child1_uuid = Uuid::new_v4();
        let child2_uuid = Uuid::new_v4();
        let grandchild_uuid = Uuid::new_v4();

        let rooms = vec![
            RoomInfo {
                id: Some(room_id_from_uuid(root_uuid)),
                name: "Root".to_string(),
                parent_id: None,
                description: None,
                inherit_acl: true,
                acls: vec![],
                effective_permissions: 0,
            },
            RoomInfo {
                id: Some(room_id_from_uuid(child1_uuid)),
                name: "Alpha Channel".to_string(),
                parent_id: Some(room_id_from_uuid(root_uuid)),
                description: None,
                inherit_acl: true,
                acls: vec![],
                effective_permissions: 0,
            },
            RoomInfo {
                id: Some(room_id_from_uuid(child2_uuid)),
                name: "Beta Channel".to_string(),
                parent_id: Some(room_id_from_uuid(root_uuid)),
                description: None,
                inherit_acl: true,
                acls: vec![],
                effective_permissions: 0,
            },
            RoomInfo {
                id: Some(room_id_from_uuid(grandchild_uuid)),
                name: "Private".to_string(),
                parent_id: Some(room_id_from_uuid(child1_uuid)),
                description: None,
                inherit_acl: true,
                acls: vec![],
                effective_permissions: 0,
            },
        ];

        let mut tree = RoomTree::new();
        tree.rebuild(&rooms);

        // Check basic structure
        assert_eq!(tree.len(), 4);
        assert_eq!(tree.roots.len(), 1);
        assert_eq!(tree.roots[0], root_uuid);

        // Check root has two children (sorted by name: Alpha, Beta)
        let root_node = tree.get(root_uuid).unwrap();
        assert_eq!(root_node.children.len(), 2);
        assert_eq!(root_node.children[0], child1_uuid); // Alpha
        assert_eq!(root_node.children[1], child2_uuid); // Beta

        // Check child1 has grandchild
        let child1_node = tree.get(child1_uuid).unwrap();
        assert_eq!(child1_node.children.len(), 1);
        assert_eq!(child1_node.children[0], grandchild_uuid);

        // Check child2 has no children
        let child2_node = tree.get(child2_uuid).unwrap();
        assert!(child2_node.children.is_empty());

        // Check ancestors
        let ancestors: Vec<Uuid> = tree.ancestors(grandchild_uuid).collect();
        assert_eq!(ancestors.len(), 2);
        assert_eq!(ancestors[0], child1_uuid);
        assert_eq!(ancestors[1], root_uuid);

        // Check depth
        assert_eq!(tree.depth(root_uuid), 0);
        assert_eq!(tree.depth(child1_uuid), 1);
        assert_eq!(tree.depth(grandchild_uuid), 2);

        // Check is_ancestor
        assert!(tree.is_ancestor(root_uuid, grandchild_uuid));
        assert!(tree.is_ancestor(child1_uuid, grandchild_uuid));
        assert!(!tree.is_ancestor(child2_uuid, grandchild_uuid));

        #[test]
        fn p2p_file_message_roundtrip() {
            let msg = P2pFileMessage::new(
                "clip.wav".to_string(),
                1234,
                "abcd".to_string(),
                "12D3KooWTestPeer".to_string(),
                vec!["/ip4/1.2.3.4/tcp/1234/p2p/12D3KooWTestPeer".to_string()],
            );

            let json = msg.to_json();
            let parsed = P2pFileMessage::parse(&json).expect("parse");
            assert_eq!(parsed.file.name, "clip.wav");
            assert_eq!(parsed.file.size, 1234);
            assert_eq!(parsed.file.file_id, "abcd");
            assert_eq!(parsed.file.peer_id, "12D3KooWTestPeer");
            assert_eq!(parsed.file.addrs.len(), 1);

            let magnet = parsed.magnet_link();
            assert!(magnet.starts_with("rumblep2p://12D3KooWTestPeer/abcd"));
            assert!(magnet.contains("ma=%2Fip4%2F1.2.3.4%2Ftcp%2F1234%2Fp2p%2F12D3KooWTestPeer"));
        }
    }
}
