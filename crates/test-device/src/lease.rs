//! Leases (docs/test-device.md "Lease and disarm"): every held input and
//! every pattern belongs to one, and a disarm ends it for good.

/// The longest lease.
pub const MAX_MS: u32 = 30_000;

/// Why the last lease ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reason {
    /// No lease has ended since boot.
    #[default]
    None,
    Expired,
    /// The host dropped DTR or closed the port.
    Closed,
    Suspended,
    /// A bus reset.
    Reset,
    /// The `detach` command took the device off the bus.
    Detached,
    /// The `release` command.
    Released,
    /// A line on the control link that is not a command: malformed, not
    /// UTF-8, or longer than the longest line.
    Protocol,
}

const REASONS: [Reason; 8] = [
    Reason::None,
    Reason::Expired,
    Reason::Closed,
    Reason::Suspended,
    Reason::Reset,
    Reason::Detached,
    Reason::Released,
    Reason::Protocol,
];

impl Reason {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Expired => "expired",
            Self::Closed => "closed",
            Self::Suspended => "suspended",
            Self::Reset => "reset",
            Self::Detached => "detached",
            Self::Released => "released",
            Self::Protocol => "protocol",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        REASONS.into_iter().find(|r| r.name() == s)
    }
}

/// Why a lease command was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Zero or more than [`MAX_MS`].
    BadLength,
    /// A lease is already held: release it first.
    Held,
    /// No lease is held.
    NoLease,
    /// The id is not the held lease's: it belongs to an ended lease.
    WrongId,
    /// The lease's pattern is already armed.
    Armed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    id: u32,
    ms: u32,
    deadline: u64,
    armed: bool,
}

/// The lease state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Lease {
    held: Option<Held>,
    last_id: u32,
    reason: Reason,
}

impl Lease {
    /// No lease held, none ended.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            held: None,
            last_id: 0,
            reason: Reason::None,
        }
    }

    /// Grants a lease of `ms` from `now`: its id and deadline. Ids only
    /// grow, so an ended lease's id never arms anything again.
    ///
    /// # Errors
    ///
    /// [`Refusal::BadLength`] or [`Refusal::Held`].
    pub fn grant(&mut self, now: u64, ms: u32) -> Result<(u32, u64), Refusal> {
        if ms == 0 || ms > MAX_MS {
            return Err(Refusal::BadLength);
        }
        if self.held.is_some() {
            return Err(Refusal::Held);
        }
        self.last_id = self.last_id.wrapping_add(1);
        let deadline = now.saturating_add(u64::from(ms) * 1000);
        self.held = Some(Held {
            id: self.last_id,
            ms,
            deadline,
            armed: false,
        });
        Ok((self.last_id, deadline))
    }

    fn held_as(&mut self, id: u32) -> Result<&mut Held, Refusal> {
        match &mut self.held {
            None => Err(Refusal::NoLease),
            Some(h) if h.id != id => Err(Refusal::WrongId),
            Some(h) => Ok(h),
        }
    }

    /// Extends lease `id` by its own length from `now`.
    ///
    /// # Errors
    ///
    /// [`Refusal::NoLease`] or [`Refusal::WrongId`].
    pub fn renew(&mut self, id: u32, now: u64) -> Result<u64, Refusal> {
        let h = self.held_as(id)?;
        h.deadline = now.saturating_add(u64::from(h.ms) * 1000);
        Ok(h.deadline)
    }

    /// Arms the loaded pattern under lease `id`.
    ///
    /// # Errors
    ///
    /// [`Refusal::NoLease`], [`Refusal::WrongId`] or [`Refusal::Armed`].
    pub fn arm(&mut self, id: u32) -> Result<(), Refusal> {
        let h = self.held_as(id)?;
        if h.armed {
            return Err(Refusal::Armed);
        }
        h.armed = true;
        Ok(())
    }

    /// Whether a lease is held: held input is allowed.
    #[must_use]
    pub const fn held(&self) -> Option<u32> {
        match self.held {
            Some(h) => Some(h.id),
            None => None,
        }
    }

    /// Whether the held lease's pattern is armed.
    #[must_use]
    pub const fn armed(&self) -> bool {
        matches!(self.held, Some(Held { armed: true, .. }))
    }

    /// Brings the held lease's deadline forward to `deadline`, never later:
    /// the revocation trial ends its lease through the expiry path.
    pub fn cut(&mut self, deadline: u64) {
        if let Some(h) = &mut self.held {
            h.deadline = h.deadline.min(deadline);
        }
    }

    /// Whether the held lease's deadline has passed at `now`.
    #[must_use]
    pub const fn expired(&self, now: u64) -> bool {
        matches!(self.held, Some(h) if now >= h.deadline)
    }

    /// Ends the held lease for `reason`. Returns false when none was held
    /// (the last reason then stays).
    pub const fn disarm(&mut self, reason: Reason) -> bool {
        if self.held.take().is_some() {
            self.reason = reason;
            true
        } else {
            false
        }
    }

    /// Why the last lease ended.
    #[must_use]
    pub const fn reason(&self) -> Reason {
        self.reason
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_expires_at_its_deadline_and_renews_by_its_length() {
        let mut l = Lease::default();
        assert_eq!(l.grant(0, 0), Err(Refusal::BadLength));
        assert_eq!(l.grant(0, MAX_MS + 1), Err(Refusal::BadLength));
        let (id, deadline) = l.grant(1_000, 5_000).unwrap();
        assert_eq!(deadline, 5_001_000);
        assert_eq!(l.grant(1_000, 5_000), Err(Refusal::Held));
        assert!(!l.expired(deadline - 1));
        assert_eq!(l.renew(id, 2_000_000), Ok(7_000_000));
        assert!(!l.expired(deadline));
        assert!(l.expired(7_000_000));
        assert!(l.disarm(Reason::Expired));
        assert_eq!(l.reason(), Reason::Expired);
        assert!(!l.disarm(Reason::Reset), "nothing held");
        assert_eq!(l.reason(), Reason::Expired);
    }

    /// After any disarm, nothing plays until a new lease and a new arm: the
    /// ended lease's id arms nothing.
    #[test]
    fn a_disarm_needs_a_new_lease_and_arm() {
        let mut l = Lease::default();
        let (old, _) = l.grant(0, 1_000).unwrap();
        l.arm(old).unwrap();
        assert_eq!(l.arm(old), Err(Refusal::Armed));
        assert!(l.armed());
        assert!(l.disarm(Reason::Suspended));
        assert!(!l.armed());
        assert_eq!(l.arm(old), Err(Refusal::NoLease));
        let (new, _) = l.grant(0, 1_000).unwrap();
        assert_ne!(new, old);
        assert!(!l.armed(), "a new lease starts disarmed");
        assert_eq!(l.arm(old), Err(Refusal::WrongId));
        assert_eq!(l.renew(old, 0), Err(Refusal::WrongId));
        l.arm(new).unwrap();
        assert!(l.armed());
    }

    #[test]
    fn every_reason_parses_back() {
        for r in REASONS {
            assert_eq!(Reason::parse(r.name()), Some(r));
        }
    }
}
