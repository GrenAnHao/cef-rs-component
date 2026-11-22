//! Pure profile identities and readiness. Native contexts stay in the runtime.
use std::collections::BTreeMap;

use crate::contracts::{ProfileConfig, ProfileHandle, ProfileStorage, RuntimeGeneration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileState {
    Pending,
    Ready,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileError {
    WrongRuntime,
    UnknownProfile,
    NotReady,
    ConfigConflict,
    InvalidName,
    InvalidTransition,
    IdExhausted,
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "profile error: {self:?}")
    }
}
impl std::error::Error for ProfileError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileRegistration {
    pub handle: ProfileHandle,
    /// Only the first reservation may issue the native context creation.
    pub is_new: bool,
}

#[derive(Debug)]
struct Entry {
    config: ProfileConfig,
    state: ProfileState,
    create_result: Option<bool>,
    initialized: bool,
    prepared: bool,
}

/// One catalog per explicit runtime owner. Entries remain after browsers close.
#[derive(Debug)]
pub struct ProfileCatalog {
    runtime: RuntimeGeneration,
    entries: BTreeMap<u64, Entry>,
    names: BTreeMap<String, u64>,
    next_local: Option<u64>,
    preparation_required: bool,
}

impl ProfileCatalog {
    pub fn new(runtime: RuntimeGeneration, storage: ProfileStorage) -> Self {
        let default = Entry {
            config: ProfileConfig {
                name: "default".into(),
                storage,
            },
            state: ProfileState::Pending,
            create_result: None,
            initialized: false,
            prepared: true,
        };
        Self {
            runtime,
            entries: BTreeMap::from([(0, default)]),
            names: BTreeMap::new(),
            next_local: Some(1),
            preparation_required: false,
        }
    }

    pub fn default_handle(&self) -> ProfileHandle {
        ProfileHandle::new(self.runtime, 0)
    }

    /// Only a persistent default uses CEF's handlerless global context.
    /// Memory defaults follow the same factory/handler merge as named contexts.
    pub fn default_context_ready(&mut self) -> Result<(), ProfileError> {
        let entry = self.entries.get_mut(&0).expect("default entry retained");
        if matches!(entry.config.storage, ProfileStorage::Memory) {
            return Err(ProfileError::InvalidTransition);
        }
        entry.create_result = Some(true);
        entry.initialized = true;
        if entry.state != ProfileState::Failed && entry.prepared {
            entry.state = ProfileState::Ready;
        }
        Ok(())
    }

    fn requires_handler(&self, handle: ProfileHandle) -> Result<bool, ProfileError> {
        Ok(handle.local() != 0
            || matches!(self.entry(handle)?.config.storage, ProfileStorage::Memory))
    }

    pub fn default_context_failed(&mut self) {
        self.entries
            .get_mut(&0)
            .expect("default entry retained")
            .state = ProfileState::Failed;
    }

    /// Enable an adapter prerequisite before native initialization begins.
    /// Context identity/storage is unchanged; the catalog remains the one
    /// readiness authority while native IO policy installation completes.
    pub fn require_preparation(&mut self) -> Result<(), ProfileError> {
        if self
            .entries
            .values()
            .any(|entry| entry.create_result.is_some() || entry.initialized)
        {
            return Err(ProfileError::InvalidTransition);
        }
        self.preparation_required = true;
        for entry in self.entries.values_mut() {
            entry.prepared = false;
        }
        Ok(())
    }

    pub fn native_initialized(&self, handle: ProfileHandle) -> Result<bool, ProfileError> {
        let entry = self.entry(handle)?;
        Ok(entry.state != ProfileState::Failed
            && entry.create_result == Some(true)
            && entry.initialized)
    }

    pub fn preparation_complete(
        &mut self,
        handle: ProfileHandle,
        success: bool,
    ) -> Result<(), ProfileError> {
        let entry = self.entry_mut(handle)?;
        if entry.state == ProfileState::Failed {
            return Err(ProfileError::InvalidTransition);
        }
        if !success {
            entry.state = ProfileState::Failed;
        } else {
            entry.prepared = true;
            if entry.create_result == Some(true) && entry.initialized {
                entry.state = ProfileState::Ready;
            }
        }
        Ok(())
    }

    pub fn validate_name(name: &str) -> Result<(), ProfileError> {
        if name.is_empty() || name.trim() != name || name.eq_ignore_ascii_case("default") {
            return Err(ProfileError::InvalidName);
        }
        Ok(())
    }

    /// Inspect an existing reservation before an adapter performs filesystem I/O.
    pub fn named_handle(&self, name: &str) -> Option<ProfileHandle> {
        self.names
            .get(name)
            .map(|&local| ProfileHandle::new(self.runtime, local))
    }

    pub fn reserve(&mut self, config: ProfileConfig) -> Result<ProfileRegistration, ProfileError> {
        Self::validate_name(&config.name)?;
        if let Some(&local) = self.names.get(&config.name) {
            if self.entries[&local].config != config {
                return Err(ProfileError::ConfigConflict);
            }
            return Ok(ProfileRegistration {
                handle: ProfileHandle::new(self.runtime, local),
                is_new: false,
            });
        }
        let local = self.next_local.ok_or(ProfileError::IdExhausted)?;
        self.next_local = local.checked_add(1);
        self.names.insert(config.name.clone(), local);
        self.entries.insert(
            local,
            Entry {
                config,
                state: ProfileState::Pending,
                create_result: None,
                initialized: false,
                prepared: !self.preparation_required,
            },
        );
        Ok(ProfileRegistration {
            handle: ProfileHandle::new(self.runtime, local),
            is_new: true,
        })
    }

    pub fn state(&self, handle: ProfileHandle) -> Result<ProfileState, ProfileError> {
        Ok(self.entry(handle)?.state)
    }

    pub fn config(&self, handle: ProfileHandle) -> Result<&ProfileConfig, ProfileError> {
        Ok(&self.entry(handle)?.config)
    }

    /// No fallback: unknown, foreign and pending explicit handles are rejected.
    pub fn select(&self, profile: Option<ProfileHandle>) -> Result<ProfileHandle, ProfileError> {
        let handle = profile.unwrap_or_else(|| self.default_handle());
        if self.entry(handle)?.state != ProfileState::Ready {
            return Err(ProfileError::NotReady);
        }
        Ok(handle)
    }

    pub fn create_returned(
        &mut self,
        handle: ProfileHandle,
        accepted: bool,
    ) -> Result<(), ProfileError> {
        if !self.requires_handler(handle)? {
            return Err(ProfileError::InvalidTransition);
        }
        let entry = self.entry_mut(handle)?;
        if entry.create_result.is_some() || (entry.state == ProfileState::Failed && accepted) {
            return Err(ProfileError::InvalidTransition);
        }
        entry.create_result = Some(accepted);
        if !accepted {
            entry.state = ProfileState::Failed;
        } else if entry.initialized && entry.prepared {
            entry.state = ProfileState::Ready;
        }
        Ok(())
    }

    /// Readiness requires both the actual creation result and its own callback.
    /// Duplicate callbacks are harmless; a failed creation never revives.
    pub fn context_initialized(&mut self, handle: ProfileHandle) -> Result<(), ProfileError> {
        if !self.requires_handler(handle)? {
            return Err(ProfileError::InvalidTransition);
        }
        let entry = self.entry_mut(handle)?;
        entry.initialized = true;
        if entry.state != ProfileState::Failed
            && entry.create_result == Some(true)
            && entry.prepared
        {
            entry.state = ProfileState::Ready;
        }
        Ok(())
    }

    fn entry(&self, handle: ProfileHandle) -> Result<&Entry, ProfileError> {
        if handle.runtime() != self.runtime {
            return Err(ProfileError::WrongRuntime);
        }
        self.entries
            .get(&handle.local())
            .ok_or(ProfileError::UnknownProfile)
    }

    fn entry_mut(&mut self, handle: ProfileHandle) -> Result<&mut Entry, ProfileError> {
        if handle.runtime() != self.runtime {
            return Err(ProfileError::WrongRuntime);
        }
        self.entries
            .get_mut(&handle.local())
            .ok_or(ProfileError::UnknownProfile)
    }
}
