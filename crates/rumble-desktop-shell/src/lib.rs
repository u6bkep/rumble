//! Desktop client shell — settings, identity/key management, hotkeys,
//! and toast notifications shared between `rumble-egui` and
//! `rumble-next`. See `docs/rumble-next-bringup.md` for the migration
//! plan.

pub mod hotkeys;
pub mod identity;
pub mod settings;
pub mod toasts;

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
pub use toasts::{Toast, ToastLevel, ToastManager};
