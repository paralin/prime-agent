//! Settings subsystem: typed settings documents, global/project storage,
//! legacy migrations, and the manager.

pub mod documents;
pub(crate) mod interactive_settings;
pub(crate) mod load;
pub(crate) mod manager;
pub(crate) mod merge;
mod runtime;
pub(crate) mod storage;
pub(crate) mod types;
mod watched;
pub use watched::WatchedSettingsManager;

pub use manager::{
    IdleEviction, SessionArchivePolicy, SettingsError, SettingsManager,
    DEFAULT_IDLE_EVICTION_MINUTES, DEFAULT_SESSION_ARCHIVE_MAX_AGE_DAYS,
    DEFAULT_SESSION_ARCHIVE_MAX_SESSIONS,
};
pub use storage::{
    FileSettingsStorage, InMemorySettingsStorage, SettingsScope, SettingsStorage, CONFIG_DIR_NAME,
};
pub use types::{
    AutoRefineSettings, AutonomousSettings, ClaudeCodeSettings, CompactionSettings,
    CompactionStrategy, EnabledFeatureSettings, McpServerConfig, ModelRoleSelector,
    OpenRouterSettings, QueueModeSetting, ScratchHandoffSettings, Settings, ThinkingLevelSetting,
    TransportSetting, UpdateChannel,
};
