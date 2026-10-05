//! Which of a model's two flash slots to run: activation of new images and rollback.
//!
//! Each model has slots A and B. One is *active*; a new image is written to the other, the
//! *standby* slot, and nothing else has to be told about it: at boot the firmware notices that
//! the standby slot holds an image it has not evaluated yet and gives it a *trial* (a golden
//! run, see `firmware/src/models.rs`). The state kept in NVS between boots is small:
//!
//! - `active`: the slot in use;
//! - `standby`: the image last evaluated in the other slot and the verdict, either `Previous`
//!   (it was active before; safe to roll back to) or `Rejected` (it failed its trial);
//! - `trying`: the candidate whose trial has started but not concluded.
//!
//! ```text
//! begin() ── standby holds an unevaluated image ──▶ Trial(slot) ── caller runs the model ──▶ conclude(passed)
//!    │                                                                     │
//!    │                                    passed: candidate becomes active, old active = Previous
//!    │                                    failed: candidate = Rejected, keep the active slot
//!    └── otherwise: Use(active), or roll back to a Previous standby if the active slot is unusable
//! ```
//!
//! `trying` is what makes a crash survivable. A bad model can take the whole chip down (ESP-DL
//! aborts on a model it cannot parse), so the caller must persist the state returned by
//! [`Activation::begin`] *before* running a trial. If the device then resets, the next `begin`
//! finds the marker; after [`MAX_TRIAL_ATTEMPTS`] unfinished trials the candidate is rejected
//! instead of crashing the device forever. More than one attempt is allowed so that a power
//! cut during a trial does not condemn a good model.
//!
//! Images are identified by [`crate::manifest::ModelManifest::image_id`], so writing the same
//! image again does not trigger another trial, while any new image does.
//!
//! Only the standby slot is watched. The state does not record which image is in the active
//! slot, so an image written over the active slot is loaded without a trial, and so is the
//! first image on a device that has no saved state. Updates must go to the standby slot.

use core::fmt;

pub type Digest = [u8; 32];

/// Trials of one candidate that may end without a conclusion before it is rejected.
const MAX_TRIAL_ATTEMPTS: u8 = 2;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    #[default]
    A,
    B,
}

impl Slot {
    pub const fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    /// Index into a `[_; 2]` of per-slot values.
    pub const fn index(self) -> usize {
        match self {
            Self::A => 0,
            Self::B => 1,
        }
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::A => "A",
            Self::B => "B",
        })
    }
}

/// The verified images in slots A and B (`None`: empty, corrupt, or the wrong model).
pub type Slots = [Option<Digest>; 2];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Was the active image before the current one; trusted for rollback.
    Previous,
    /// Failed its trial; never used.
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Load the image in this slot.
    Use(Slot),
    /// Persist the state, validate the image in this slot, then call [`Activation::conclude`].
    Trial(Slot),
    /// No usable image: the stage that needs this model stays off.
    Unavailable,
}

/// The default is a device that has never switched: slot A, nothing evaluated.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Activation {
    pub active: Slot,
    /// The image last evaluated in the standby slot.
    pub standby: Option<(Digest, Verdict)>,
    /// The candidate on trial and how many trials of it have been started.
    pub trying: Option<(Digest, u8)>,
}

impl Activation {
    /// Decides what to do at boot, given what the two slots hold.
    pub fn begin(&mut self, slots: &Slots) -> Decision {
        let interrupted = self.trying.take();
        let standby_slot = self.active.other();

        if let Some(candidate) = slots[standby_slot.index()] {
            let evaluated = matches!(self.standby, Some((digest, _)) if digest == candidate);
            let same_as_active = slots[self.active.index()] == Some(candidate);
            if !evaluated && !same_as_active {
                let attempts = match interrupted {
                    Some((digest, attempts)) if digest == candidate => attempts,
                    _ => 0,
                };
                if attempts < MAX_TRIAL_ATTEMPTS {
                    self.trying = Some((candidate, attempts + 1));
                    return Decision::Trial(standby_slot);
                }
                self.standby = Some((candidate, Verdict::Rejected));
            }
        }
        self.settle(slots)
    }

    /// Records the result of the trial started by [`Self::begin`] and decides what to load.
    pub fn conclude(&mut self, slots: &Slots, passed: bool) -> Decision {
        let Some((candidate, _)) = self.trying.take() else {
            return self.settle(slots);
        };
        if passed {
            self.standby = slots[self.active.index()].map(|digest| (digest, Verdict::Previous));
            self.active = self.active.other();
            Decision::Use(self.active)
        } else {
            self.standby = Some((candidate, Verdict::Rejected));
            self.settle(slots)
        }
    }

    /// Ends the trial started by [`Self::begin`] without a verdict, e.g. because the trial
    /// marker could not be saved. The candidate is tried again at the next start.
    pub fn abandon(&mut self, slots: &Slots) -> Decision {
        self.trying = None;
        self.settle(slots)
    }

    /// Uses the active slot, or rolls back to the standby slot if the active image is unusable
    /// and the standby one is the image that was active before.
    fn settle(&mut self, slots: &Slots) -> Decision {
        if slots[self.active.index()].is_some() {
            return Decision::Use(self.active);
        }
        let standby_slot = self.active.other();
        match (slots[standby_slot.index()], self.standby) {
            (Some(image), Some((digest, Verdict::Previous))) if image == digest => {
                self.active = standby_slot;
                self.standby = None;
                Decision::Use(standby_slot)
            }
            _ => Decision::Unavailable,
        }
    }
}

/// Length of an [`Activation`] record in NVS.
pub const ENCODED_LEN: usize = 68;

/// Version 1 had a trailing checksum byte; NVS already protects each blob with a CRC.
const RECORD_VERSION: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordError {
    Length(usize),
    UnsupportedVersion(u8),
    /// A field holds a value this firmware never writes.
    Invalid(&'static str),
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length(len) => write!(f, "record is {len} bytes, expected {ENCODED_LEN}"),
            Self::UnsupportedVersion(v) => {
                write!(
                    f,
                    "unsupported record version {v} (expected {RECORD_VERSION})"
                )
            }
            Self::Invalid(field) => write!(f, "invalid {field}"),
        }
    }
}

impl std::error::Error for RecordError {}

impl Activation {
    /// Layout: version, active slot, standby verdict (0 none, 1 previous, 2 rejected), standby
    /// digest, trial attempts (0 = no trial), candidate digest.
    pub fn encode(&self) -> [u8; ENCODED_LEN] {
        let mut record = [0u8; ENCODED_LEN];
        record[0] = RECORD_VERSION;
        record[1] = self.active.index() as u8;
        if let Some((digest, verdict)) = self.standby {
            record[2] = match verdict {
                Verdict::Previous => 1,
                Verdict::Rejected => 2,
            };
            record[3..35].copy_from_slice(&digest);
        }
        if let Some((digest, attempts)) = self.trying {
            record[35] = attempts;
            record[36..68].copy_from_slice(&digest);
        }
        record
    }

    pub fn decode(record: &[u8]) -> Result<Self, RecordError> {
        let record: &[u8; ENCODED_LEN] = record
            .try_into()
            .map_err(|_| RecordError::Length(record.len()))?;
        if record[0] != RECORD_VERSION {
            return Err(RecordError::UnsupportedVersion(record[0]));
        }
        let digest = |start: usize| -> Digest {
            record[start..start + 32]
                .try_into()
                .expect("32 bytes within the record")
        };
        let active = match record[1] {
            0 => Slot::A,
            1 => Slot::B,
            _ => return Err(RecordError::Invalid("active slot")),
        };
        let standby = match record[2] {
            0 => None,
            1 => Some((digest(3), Verdict::Previous)),
            2 => Some((digest(3), Verdict::Rejected)),
            _ => return Err(RecordError::Invalid("standby verdict")),
        };
        let trying = match record[35] {
            0 => None,
            attempts => Some((digest(36), attempts)),
        };
        Ok(Self {
            active,
            standby,
            trying,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: Digest = [0x11; 32];
    const NEW: Digest = [0x22; 32];
    const OTHER: Digest = [0x33; 32];

    /// Runs one boot: `begin`, and if it asks for a trial, concludes it with `verdict`.
    fn boot(state: &mut Activation, slots: &Slots, verdict: bool) -> Decision {
        match state.begin(slots) {
            Decision::Trial(_) => state.conclude(slots, verdict),
            decision => decision,
        }
    }

    #[test]
    fn fresh_device_uses_slot_a() {
        let mut state = Activation::default();
        assert_eq!(state.begin(&[Some(OLD), None]), Decision::Use(Slot::A));
        assert_eq!(state, Activation::default());
    }

    #[test]
    fn nothing_flashed_is_unavailable() {
        let mut state = Activation::default();
        assert_eq!(state.begin(&[None, None]), Decision::Unavailable);
    }

    #[test]
    fn new_image_in_standby_gets_a_trial_and_is_activated_when_it_passes() {
        let slots = [Some(OLD), Some(NEW)];
        let mut state = Activation::default();
        assert_eq!(state.begin(&slots), Decision::Trial(Slot::B));
        assert_eq!(state.trying, Some((NEW, 1)));
        assert_eq!(state.active, Slot::A, "not switched before the trial ends");

        assert_eq!(state.conclude(&slots, true), Decision::Use(Slot::B));
        assert_eq!(
            state,
            Activation {
                active: Slot::B,
                standby: Some((OLD, Verdict::Previous)),
                trying: None,
            }
        );
    }

    #[test]
    fn activated_image_is_not_tried_again_and_the_old_one_is_not_a_candidate() {
        let slots = [Some(OLD), Some(NEW)];
        let mut state = Activation::default();
        boot(&mut state, &slots, true);
        let after_switch = state;
        // Later boots: no trial, even though the standby slot (A) holds a different image.
        assert_eq!(state.begin(&slots), Decision::Use(Slot::B));
        assert_eq!(state, after_switch);
    }

    #[test]
    fn failed_trial_keeps_the_active_slot_and_is_not_repeated() {
        let slots = [Some(OLD), Some(NEW)];
        let mut state = Activation::default();
        assert_eq!(boot(&mut state, &slots, false), Decision::Use(Slot::A));
        assert_eq!(state.standby, Some((NEW, Verdict::Rejected)));
        assert_eq!(state.begin(&slots), Decision::Use(Slot::A));
    }

    #[test]
    fn a_different_image_after_a_rejection_gets_its_own_trial() {
        let mut state = Activation::default();
        boot(&mut state, &[Some(OLD), Some(NEW)], false);
        assert_eq!(
            state.begin(&[Some(OLD), Some(OTHER)]),
            Decision::Trial(Slot::B)
        );
    }

    #[test]
    fn updates_alternate_between_the_slots() {
        let mut state = Activation::default();
        boot(&mut state, &[Some(OLD), Some(NEW)], true);
        // The next update is written to slot A, which is now the standby slot.
        let slots = [Some(OTHER), Some(NEW)];
        assert_eq!(state.begin(&slots), Decision::Trial(Slot::A));
        assert_eq!(state.conclude(&slots, true), Decision::Use(Slot::A));
        assert_eq!(state.standby, Some((NEW, Verdict::Previous)));
    }

    #[test]
    fn same_image_in_both_slots_needs_no_trial() {
        let mut state = Activation::default();
        assert_eq!(state.begin(&[Some(OLD), Some(OLD)]), Decision::Use(Slot::A));
        assert_eq!(state, Activation::default());
    }

    #[test]
    fn interrupted_trial_is_retried_then_rejected() {
        let slots = [Some(OLD), Some(NEW)];
        let mut state = Activation::default();
        // Each `begin` without a `conclude` is a boot that died during the trial.
        for attempt in 1..=MAX_TRIAL_ATTEMPTS {
            assert_eq!(state.begin(&slots), Decision::Trial(Slot::B));
            assert_eq!(state.trying, Some((NEW, attempt)));
        }
        assert_eq!(state.begin(&slots), Decision::Use(Slot::A));
        assert_eq!(
            state,
            Activation {
                active: Slot::A,
                standby: Some((NEW, Verdict::Rejected)),
                trying: None,
            }
        );
        assert_eq!(state.begin(&slots), Decision::Use(Slot::A));
    }

    #[test]
    fn interrupted_trial_that_then_passes_is_activated() {
        let slots = [Some(OLD), Some(NEW)];
        let mut state = Activation::default();
        state.begin(&slots); // power cut
        assert_eq!(boot(&mut state, &slots, true), Decision::Use(Slot::B));
    }

    #[test]
    fn attempts_are_counted_per_image() {
        let mut state = Activation::default();
        // Interrupted; then a different image is written before the next boot.
        state.begin(&[Some(OLD), Some(NEW)]);
        assert_eq!(
            state.begin(&[Some(OLD), Some(OTHER)]),
            Decision::Trial(Slot::B)
        );
        assert_eq!(state.trying, Some((OTHER, 1)));
    }

    #[test]
    fn stale_trial_marker_is_dropped_when_the_candidate_is_gone() {
        let mut state = Activation::default();
        state.begin(&[Some(OLD), Some(NEW)]); // interrupted
        assert_eq!(state.begin(&[Some(OLD), None]), Decision::Use(Slot::A));
        assert_eq!(state, Activation::default());
    }

    #[test]
    fn corrupt_active_slot_rolls_back_to_the_previous_image() {
        let mut state = Activation::default();
        boot(&mut state, &[Some(OLD), Some(NEW)], true);
        // The new image in B later fails verification.
        assert_eq!(state.begin(&[Some(OLD), None]), Decision::Use(Slot::A));
        assert_eq!(
            state,
            Activation {
                active: Slot::A,
                standby: None,
                trying: None,
            }
        );
    }

    #[test]
    fn corrupt_active_slot_does_not_fall_back_to_a_rejected_image() {
        let mut state = Activation::default();
        boot(&mut state, &[Some(OLD), Some(NEW)], false);
        assert_eq!(state.begin(&[None, Some(NEW)]), Decision::Unavailable);
        assert_eq!(state.active, Slot::A);
    }

    #[test]
    fn empty_active_slot_with_a_new_standby_image_trials_it() {
        // E.g. a fresh device whose only image was written to slot B.
        let slots = [None, Some(NEW)];
        let mut state = Activation::default();
        assert_eq!(state.begin(&slots), Decision::Trial(Slot::B));
        assert_eq!(state.conclude(&slots, true), Decision::Use(Slot::B));
        assert_eq!(state.standby, None, "there is no previous image");
    }

    #[test]
    fn failed_trial_with_no_active_image_is_unavailable() {
        let slots = [None, Some(NEW)];
        let mut state = Activation::default();
        assert_eq!(boot(&mut state, &slots, false), Decision::Unavailable);
    }

    #[test]
    fn conclude_without_a_trial_changes_nothing() {
        let slots = [Some(OLD), None];
        let mut state = Activation::default();
        assert_eq!(state.conclude(&slots, true), Decision::Use(Slot::A));
        assert_eq!(state, Activation::default());
    }

    #[test]
    fn abandoned_trial_is_started_again_at_the_next_boot() {
        let slots = [Some(OLD), Some(NEW)];
        let mut state = Activation::default();
        assert_eq!(state.begin(&slots), Decision::Trial(Slot::B));
        assert_eq!(state.abandon(&slots), Decision::Use(Slot::A));
        // Nothing was decided about the candidate, and the attempt does not count.
        assert_eq!(state, Activation::default());
        assert_eq!(state.begin(&slots), Decision::Trial(Slot::B));
        assert_eq!(state.trying, Some((NEW, 1)));
    }

    #[test]
    fn record_round_trips_every_shape() {
        for state in [
            Activation::default(),
            Activation {
                active: Slot::B,
                standby: Some((OLD, Verdict::Previous)),
                trying: None,
            },
            Activation {
                active: Slot::A,
                standby: Some((NEW, Verdict::Rejected)),
                trying: Some((OTHER, 2)),
            },
        ] {
            assert_eq!(Activation::decode(&state.encode()), Ok(state));
        }
    }

    #[test]
    fn record_rejects_damage() {
        let record = Activation::default().encode();
        assert_eq!(
            Activation::decode(&record[..10]),
            Err(RecordError::Length(10))
        );
        assert_eq!(
            Activation::decode(&[0u8; ENCODED_LEN]),
            Err(RecordError::UnsupportedVersion(0))
        );
        assert_eq!(
            Activation::decode(&[0xFF; ENCODED_LEN]),
            Err(RecordError::UnsupportedVersion(0xFF))
        );
        // A version 1 record (69 bytes, with a checksum) is not read as version 2.
        assert_eq!(Activation::decode(&[1u8; 69]), Err(RecordError::Length(69)));
    }

    #[test]
    fn record_rejects_out_of_range_fields() {
        for (offset, field) in [(1, "active slot"), (2, "standby verdict")] {
            let mut record = Activation::default().encode();
            record[offset] = 7;
            assert_eq!(
                Activation::decode(&record),
                Err(RecordError::Invalid(field))
            );
        }
    }

    #[test]
    fn slot_helpers() {
        assert_eq!(Slot::A.other(), Slot::B);
        assert_eq!(Slot::B.other(), Slot::A);
        assert_eq!((Slot::A.index(), Slot::B.index()), (0, 1));
        assert_eq!(format!("{}{}", Slot::A, Slot::B), "AB");
    }
}
