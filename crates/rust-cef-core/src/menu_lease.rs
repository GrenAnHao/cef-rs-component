//! Revocable menu qualification, independent of the UI-thread paint token.
//!
//! This is trusted-host wiring, NOT proof that a native window/frame is alive.
//! The UI adapter must retire/invalidate the owner from its actual lifecycle.
//! The Runtime must check the stamp at response admission, then enter again on
//! the CEF UI sequence and requalify the original native Browser/frame. No lock
//! spans native calls. An entered call may finish after retirement; drain waits
//! for its permit instead of pretending to revoke already-started work.
use crate::ports::WindowBinding;
use std::{
    marker::PhantomData,
    num::NonZeroUsize,
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard, Weak},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuLeaseError {
    InvalidBinding,
    WrongLease,
    Stale,
    Retired,
    Backpressure,
    RevisionExhausted,
    Poisoned,
    Invariant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuLeaseSnapshot {
    pub binding: WindowBinding,
    pub revision: u64,
    pub retired: bool,
    pub in_flight: usize,
    pub capacity: NonZeroUsize,
    pub faulted: bool,
}
impl MenuLeaseSnapshot {
    /// Only the lease's owned permits, not native/Runtime/global shutdown proof.
    pub fn is_drained(self) -> bool {
        self.retired && self.in_flight == 0 && !self.faulted
    }
}
#[derive(Debug)]
struct State {
    snapshot: MenuLeaseSnapshot,
}

/// Unique mutation authority. !Send + !Sync; create/keep it on the actual host UI.
/// Drop only retires std data, never calls a presenter, posts, or touches native.
#[derive(Debug)]
pub struct MenuLeaseOwner {
    state: Arc<Mutex<State>>,
    _ui: PhantomData<Rc<()>>,
}
/// Cloneable data-only observer, Send + Sync. Does not keep any native owner alive.
#[derive(Clone, Debug)]
pub struct MenuLease {
    state: Arc<Mutex<State>>,
}
/// Exact lease allocation plus immutable binding/revision; metadata cannot forge
/// a stamp for a different registration, even with identical numeric values.
#[derive(Clone, Debug)]
pub struct MenuLeaseStamp {
    state: Weak<Mutex<State>>,
    binding: WindowBinding,
    revision: u64,
}
impl MenuLeaseStamp {
    pub fn binding(&self) -> WindowBinding {
        self.binding
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
}
/// One admitted call. !Send + !Sync; obtain on the executing sequence. Its Drop
/// returns only the std counter; actual native completion is the caller's duty.
#[derive(Debug)]
pub struct MenuExecutionPermit {
    state: Arc<Mutex<State>>,
    _sequence: PhantomData<Rc<()>>,
}

fn lock(state: &Mutex<State>) -> Result<MutexGuard<'_, State>, MenuLeaseError> {
    state.lock().map_err(|_| MenuLeaseError::Poisoned)
}
impl MenuLeaseOwner {
    pub fn new(binding: WindowBinding, capacity: NonZeroUsize) -> Result<Self, MenuLeaseError> {
        if binding.window == 0
            || binding.browser.local() == 0
            || binding.browser.runtime().value() == 0
        {
            return Err(MenuLeaseError::InvalidBinding);
        }
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                snapshot: MenuLeaseSnapshot {
                    binding,
                    revision: 1,
                    retired: false,
                    in_flight: 0,
                    capacity,
                    faulted: false,
                },
            })),
            _ui: PhantomData,
        })
    }
    pub fn lease(&self) -> MenuLease {
        MenuLease {
            state: self.state.clone(),
        }
    }
    /// Explicit menu-relevant invalidation, not every paint/frame replacement.
    /// Existing permits remain counted; exhaustion permanently retires the owner.
    pub fn invalidate(&mut self) -> Result<(), MenuLeaseError> {
        let mut state = lock(&self.state)?;
        let s = &mut state.snapshot;
        if s.faulted {
            return Err(MenuLeaseError::Invariant);
        }
        if s.retired {
            return Err(MenuLeaseError::Retired);
        }
        let Some(next) = s.revision.checked_add(1) else {
            s.retired = true;
            return Err(MenuLeaseError::RevisionExhausted);
        };
        s.revision = next;
        Ok(())
    }
    pub fn retire(&mut self) -> Result<bool, MenuLeaseError> {
        let mut state = lock(&self.state)?;
        let changed = !state.snapshot.retired;
        state.snapshot.retired = true;
        Ok(changed)
    }
}
impl Drop for MenuLeaseOwner {
    fn drop(&mut self) {
        match self.state.lock() {
            Ok(mut s) => s.snapshot.retired = true,
            Err(error) => {
                let mut s = error.into_inner();
                s.snapshot.retired = true;
                s.snapshot.faulted = true;
            }
        }
    }
}
impl MenuLease {
    pub fn snapshot(&self) -> Result<MenuLeaseSnapshot, MenuLeaseError> {
        Ok(lock(&self.state)?.snapshot)
    }
    pub fn capture(&self) -> Result<MenuLeaseStamp, MenuLeaseError> {
        let s = self.snapshot()?;
        if s.faulted {
            return Err(MenuLeaseError::Invariant);
        }
        if s.retired {
            return Err(MenuLeaseError::Retired);
        }
        Ok(MenuLeaseStamp {
            state: Arc::downgrade(&self.state),
            binding: s.binding,
            revision: s.revision,
        })
    }
    fn validate(&self, stamp: &MenuLeaseStamp, s: MenuLeaseSnapshot) -> Result<(), MenuLeaseError> {
        if !Weak::ptr_eq(&Arc::downgrade(&self.state), &stamp.state) || stamp.binding != s.binding {
            return Err(MenuLeaseError::WrongLease);
        }
        if s.faulted {
            return Err(MenuLeaseError::Invariant);
        }
        if s.retired {
            return Err(MenuLeaseError::Retired);
        }
        if s.revision != stamp.revision {
            return Err(MenuLeaseError::Stale);
        }
        Ok(())
    }
    /// Admission-time observation only: it reserves nothing and cannot promise
    /// future validity. Execution must use try_enter, never rely on this boolean.
    pub fn is_current(&self, stamp: &MenuLeaseStamp) -> Result<bool, MenuLeaseError> {
        let s = self.snapshot()?;
        match self.validate(stamp, s) {
            Ok(()) => Ok(true),
            Err(MenuLeaseError::WrongLease | MenuLeaseError::Retired | MenuLeaseError::Stale) => {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
    /// Linearizes with invalidate/retire under this std-only mutex, then releases
    /// the lock before returning. No polling, callback, retry, or native work.
    pub fn try_enter(&self, stamp: &MenuLeaseStamp) -> Result<MenuExecutionPermit, MenuLeaseError> {
        let mut state = lock(&self.state)?;
        self.validate(stamp, state.snapshot)?;
        if state.snapshot.in_flight >= state.snapshot.capacity.get() {
            return Err(MenuLeaseError::Backpressure);
        }
        let Some(next) = state.snapshot.in_flight.checked_add(1) else {
            state.snapshot.retired = true;
            state.snapshot.faulted = true;
            return Err(MenuLeaseError::Invariant);
        };
        state.snapshot.in_flight = next;
        Ok(MenuExecutionPermit {
            state: self.state.clone(),
            _sequence: PhantomData,
        })
    }
}
impl Drop for MenuExecutionPermit {
    fn drop(&mut self) {
        // A poisoned counter is still retired/returned where possible, but its
        // fault is permanent. No recovered state is allowed to authorize work.
        let (mut state, poisoned) = match self.state.lock() {
            Ok(s) => (s, false),
            Err(e) => (e.into_inner(), true),
        };
        let s = &mut state.snapshot;
        if poisoned {
            s.retired = true;
            s.faulted = true;
        }
        if let Some(next) = s.in_flight.checked_sub(1) {
            s.in_flight = next;
        } else {
            s.retired = true;
            s.faulted = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{BrowserId, RuntimeGeneration};
    fn binding() -> WindowBinding {
        WindowBinding {
            browser: BrowserId::new(RuntimeGeneration::new(7), 3),
            window: 41,
        }
    }
    fn owner() -> MenuLeaseOwner {
        MenuLeaseOwner::new(binding(), NonZeroUsize::new(1).unwrap()).unwrap()
    }
    #[test]
    fn identical_metadata_never_authorizes_another_registration() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<MenuLease>();
        send_sync::<MenuLeaseStamp>();
        let a = owner();
        let b = owner();
        let lease = a.lease();
        let stamp = b.lease().capture().unwrap();
        assert_eq!(stamp.binding(), binding());
        assert_eq!(stamp.revision(), 1);
        assert!(!lease.is_current(&stamp).unwrap());
        assert_eq!(
            lease.try_enter(&stamp).unwrap_err(),
            MenuLeaseError::WrongLease
        );
        assert_eq!(lease.snapshot().unwrap().in_flight, 0);
    }
    #[test]
    fn invalidation_capacity_and_retirement_preserve_inflight_accounting() {
        let mut owner = owner();
        let lease = owner.lease();
        let old = lease.capture().unwrap();
        let permit = lease.try_enter(&old).unwrap();
        assert_eq!(lease.snapshot().unwrap().in_flight, 1);
        owner.invalidate().unwrap();
        assert!(!lease.is_current(&old).unwrap());
        assert_eq!(lease.try_enter(&old).unwrap_err(), MenuLeaseError::Stale);
        let new = lease.capture().unwrap();
        assert_eq!(new.revision(), 2);
        assert_eq!(
            lease.try_enter(&new).unwrap_err(),
            MenuLeaseError::Backpressure
        );
        drop(permit);
        let permit = lease.try_enter(&new).unwrap();
        assert!(owner.retire().unwrap());
        assert!(!owner.retire().unwrap());
        let s = lease.snapshot().unwrap();
        assert_eq!(s.in_flight, 1);
        assert!(!s.is_drained());
        assert_eq!(lease.try_enter(&new).unwrap_err(), MenuLeaseError::Retired);
        drop(permit);
        assert!(lease.snapshot().unwrap().is_drained());
    }
    #[test]
    fn exhausted_revision_and_owner_drop_never_revive_or_reset_counts() {
        let mut o = owner();
        let lease = o.lease();
        o.state.lock().unwrap().snapshot.revision = u64::MAX;
        let stamp = lease.capture().unwrap();
        let permit = lease.try_enter(&stamp).unwrap();
        assert_eq!(o.invalidate(), Err(MenuLeaseError::RevisionExhausted));
        assert_eq!(lease.snapshot().unwrap().revision, u64::MAX);
        assert!(!lease.is_current(&stamp).unwrap());
        assert!(lease.capture().is_err());
        assert_eq!(lease.snapshot().unwrap().in_flight, 1);
        drop(permit);
        assert!(lease.snapshot().unwrap().is_drained());
        let o = owner();
        let lease = o.lease();
        let stamp = lease.capture().unwrap();
        let permit = lease.try_enter(&stamp).unwrap();
        drop(o);
        assert!(!lease.is_current(&stamp).unwrap());
        assert!(!lease.snapshot().unwrap().is_drained());
        drop(permit);
        assert!(lease.snapshot().unwrap().is_drained());
        let mut bad = binding();
        bad.window = 0;
        assert_eq!(
            MenuLeaseOwner::new(bad, NonZeroUsize::new(1).unwrap()).unwrap_err(),
            MenuLeaseError::InvalidBinding
        );
    }
    #[test]
    fn poisoned_state_never_authorizes_or_becomes_successful_drain() {
        let owner = owner();
        let lease = owner.lease();
        let stamp = lease.capture().unwrap();
        let permit = lease.try_enter(&stamp).unwrap();
        let state = lease.state.clone();
        assert!(
            std::panic::catch_unwind(move || {
                let _guard = state.lock().unwrap();
                panic!("injected menu lease mutex poison");
            })
            .is_err()
        );
        assert_eq!(lease.snapshot().unwrap_err(), MenuLeaseError::Poisoned);
        assert_eq!(lease.capture().unwrap_err(), MenuLeaseError::Poisoned);
        assert_eq!(
            lease.try_enter(&stamp).unwrap_err(),
            MenuLeaseError::Poisoned
        );
        drop(permit);
        let snapshot = lease.state.lock().unwrap_err().into_inner().snapshot;
        assert_eq!(snapshot.in_flight, 0);
        assert!(snapshot.retired && snapshot.faulted);
        assert!(!snapshot.is_drained());
    }

    #[test]
    fn concurrent_retirement_rejects_later_entry_but_waits_for_entered_call() {
        use std::{sync::mpsc::sync_channel, thread, time::Duration};
        let mut o = owner();
        let lease = o.lease();
        let worker_lease = lease.clone();
        let stamp = lease.capture().unwrap();
        let other = stamp.clone();
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let worker = thread::spawn(move || {
            let p = worker_lease.try_enter(&stamp).unwrap();
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(p);
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        o.retire().unwrap();
        assert_eq!(lease.snapshot().unwrap().in_flight, 1);
        assert!(!lease.snapshot().unwrap().is_drained());
        assert_eq!(
            lease.try_enter(&other).unwrap_err(),
            MenuLeaseError::Retired
        );
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(lease.snapshot().unwrap().is_drained());
    }
}
