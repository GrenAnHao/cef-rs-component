//! Passive shipping-layout fixture. The separate loopback observer, not this
//! host, judges which values the Browser read from cookie and localStorage.
use super::{AsyncApp, Duration, Instant, PathBuf, Runtime, UiWakeReceiver, object, quote, turn};
use rust_cef_gpui::{
    contracts::{ProfileConfig, ProfileHandle, ProfileStorage, TrustedContentPolicy},
    runtime::{CookieFlushStatus, ProfileState},
};
use std::collections::BTreeMap;

const PROFILE_NAME: &str = "ReleasePersistent";

#[derive(Clone)]
pub(super) struct Restart {
    pub root: PathBuf,
    pub page: String,
    origin: String,
    persistent: bool,
    write: bool,
    token: String,
}

impl Restart {
    pub(super) fn parse(fields: &mut BTreeMap<String, String>) -> Result<Option<Self>, String> {
        let Some(storage) = fields.remove("--restart-storage") else {
            return Ok(None);
        };
        let persistent = match storage.as_str() {
            "Persistent" => true,
            "Memory" => false,
            _ => return Err("invalid restart storage".into()),
        };
        let write = match fields.remove("--restart-phase").as_deref() {
            Some("Write") => true,
            Some("Read") => false,
            _ => return Err("invalid restart phase".into()),
        };
        let root = PathBuf::from(
            fields
                .remove("--restart-root")
                .ok_or("missing restart root")?,
        );
        let page = fields
            .remove("--restart-page")
            .ok_or("missing restart page")?;
        let token = fields
            .remove("--restart-token")
            .ok_or("missing restart token")?;
        if !root.is_absolute()
            || page.len() > 2048
            || token.is_empty()
            || token.len() > 96
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err("invalid restart root, page or fixture token".into());
        }
        let (origin, query) = page
            .split_once("/profile-test?")
            .ok_or("invalid restart fixture page")?;
        let port = origin
            .strip_prefix("http://127.0.0.1:")
            .and_then(|n| n.parse::<u16>().ok())
            .filter(|n| *n != 0)
            .ok_or("not a loopback restart origin")?;
        let op = if write { "write" } else { "read" };
        let prefix = format!("token={token}&op={op}&value=");
        let value = query
            .strip_prefix(&prefix)
            .ok_or("restart page does not match its token and phase")?;
        // The runner generates only opaque ASCII fixture values (not account data).
        if origin != format!("http://127.0.0.1:{port}")
            || value.len() > 96
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || write == value.is_empty()
        {
            return Err("invalid loopback restart URL".into());
        }
        Ok(Some(Self {
            root,
            origin: origin.into(),
            page,
            persistent,
            write,
            token,
        }))
    }

    pub(super) fn policy(&self) -> TrustedContentPolicy {
        TrustedContentPolicy {
            allowed_origins: vec![self.origin.clone()],
            allow_embedded_data: false,
        }
    }

    pub(super) async fn ready_profile(
        &self,
        cx: &mut AsyncApp,
        rt: &mut Runtime,
        receiver: &UiWakeReceiver,
    ) -> Result<ProfileHandle, String> {
        let (profile, expected_storage) = if self.persistent {
            let storage = ProfileStorage::Persistent {
                directory: self.root.join(PROFILE_NAME),
            };
            let registration = rt
                .create_profile(ProfileConfig {
                    name: PROFILE_NAME.into(),
                    storage: storage.clone(),
                })
                .map_err(|e| format!("register restart profile: {e:?}"))?;
            if !registration.is_new || registration.handle == rt.default_profile_handle() {
                return Err("new named profile did not get its own handle".into());
            }
            (registration.handle, storage)
        } else {
            (rt.default_profile_handle(), ProfileStorage::Memory)
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rt.profile_state(profile) {
                Ok(ProfileState::Ready) => break,
                Ok(ProfileState::Pending) if Instant::now() < deadline => {
                    turn(cx, rt, receiver, false).await?;
                }
                other => return Err(format!("restart profile not ready: {other:?}")),
            }
        }
        let configured = rt
            .profile_config(profile)
            .map_err(|e| format!("restart profile configuration: {e:?}"))?;
        // Persistent registration canonicalizes its directory (including a Win32
        // verbatim prefix), so compare actual filesystem identity after Ready.
        let exact_storage = match (&expected_storage, &configured.storage) {
            (ProfileStorage::Memory, ProfileStorage::Memory) => true,
            (
                ProfileStorage::Persistent {
                    directory: expected,
                },
                ProfileStorage::Persistent { directory: actual },
            ) => {
                expected.canonicalize().map_err(|e| e.to_string())?
                    == actual.canonicalize().map_err(|e| e.to_string())?
            }
            _ => false,
        };
        if !exact_storage
            || (self.persistent && configured.name != PROFILE_NAME)
            || !rt.diagnostics().root_lease_held
        {
            return Err("restart profile storage or lease disagrees with the launch".into());
        }
        Ok(profile)
    }

    pub(super) async fn complete_write(
        &self,
        cx: &mut AsyncApp,
        rt: &mut Runtime,
        receiver: &UiWakeReceiver,
        profile: ProfileHandle,
    ) -> Result<bool, String> {
        if !self.write {
            return Ok(false);
        }
        // The owned observer has already acknowledged the real page result.
        // Only measured cookie completion is reported; no all-storage promise.
        let ticket = rt
            .flush_profile_cookies(profile)
            .map_err(|e| format!("restart cookie flush: {e:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match ticket.poll() {
                CookieFlushStatus::Complete => return Ok(true),
                CookieFlushStatus::Pending if Instant::now() < deadline => {
                    turn(cx, rt, receiver, false).await?;
                }
                state => return Err(format!("restart cookie flush not complete: {state:?}")),
            }
        }
    }

    pub(super) fn report(&self, profile: ProfileHandle, flushed: bool) -> String {
        object(vec![
            (
                "storage",
                quote(if self.persistent {
                    "Persistent"
                } else {
                    "Memory"
                }),
            ),
            ("phase", quote(if self.write { "Write" } else { "Read" })),
            ("root", quote(&self.root.to_string_lossy())),
            ("origin", quote(&self.origin)),
            ("token", quote(&self.token)),
            (
                "profile",
                format!("[{},{}]", profile.runtime().value(), profile.local()),
            ),
            ("root_lease_observed", "true".into()),
            ("cookie_flush_complete", flushed.to_string()),
        ])
    }
}
