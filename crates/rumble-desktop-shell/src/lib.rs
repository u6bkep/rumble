//! Desktop client shell — settings, identity/key management, and
//! hotkeys shared between desktop client frontends.

pub mod hotkeys;
pub mod identity;
pub mod settings;

pub use hotkeys::{
    HotkeyAction, HotkeyBinding, HotkeyEvent, HotkeyManager, HotkeyModifiers, HotkeyRegistrationStatus,
    KeyboardSettings,
};
pub use identity::{
    KeyConfig, KeyInfo, KeyManager, KeyManagerSigner, KeySource, compute_fingerprint, parse_signing_key,
};
pub use settings::{
    AcceptedCertificate, AutoDownloadRule, ChatSettings, FileTransferSettings, PersistentAudioSettings,
    PersistentVoiceMode, RecentServer, Settings, SettingsStore, SfxSettings, TimestampFormat,
};
