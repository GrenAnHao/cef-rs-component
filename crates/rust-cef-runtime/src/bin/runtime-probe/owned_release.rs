//! An absent observation is not an attempted release. Once an owned candidate
//! is selected, even a refused post consumes this test-host action.
#[derive(Default)]
pub(super) struct OwnedRelease(bool);

impl OwnedRelease {
    pub(super) fn claim<T>(&mut self, candidate: Option<T>) -> Option<T> {
        if self.0 {
            return None;
        }
        let owned = candidate?;
        self.0 = true;
        Some(owned)
    }

    pub(super) fn attempted(&self) -> bool {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::OwnedRelease;

    #[test]
    fn unobserved_hold_can_appear_later_but_is_claimed_once() {
        let mut release = OwnedRelease::default();
        assert_eq!(release.claim(None::<u64>), None);
        assert!(!release.attempted());
        assert_eq!(release.claim(Some(17)), Some(17));
        assert!(release.attempted());
        assert_eq!(release.claim(Some(17)), None);
        assert_eq!(release.claim(Some(18)), None);
    }

    #[test]
    fn refusing_a_post_never_reopens_the_original_action() {
        let mut release = OwnedRelease::default();
        let refused = release
            .claim(Some(17))
            .map(|_| Err::<(), _>("post refused"));
        assert_eq!(refused, Some(Err("post refused")));
        assert!(release.attempted());
        assert_eq!(release.claim(None::<u64>), None);
        assert_eq!(release.claim(Some(17)), None);
    }
}
