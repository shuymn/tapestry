//! Leases (docs/test-device.md "Lease and Disarm"): every held input and
//! every pattern belongs to one, and a disarm ends it for good. A lease
//! ends at a deadline the host names on the device's clock, never later
//! than the lease's length from the moment the device reads the grant or
//! renewal, so a command delayed on its way can only end it sooner.

/// The longest lease.
pub const MAX_MS: u32 = 30_000;

/// Why the last lease ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reason {
    /// No lease has ended since boot.
    #[default]
    None,
    Expired,
    /// The host dropped DTR or closed the port, or the link's read failed.
    Closed,
    Suspended,
    /// A bus reset.
    Reset,
    /// The host unconfigured the device (`SET_CONFIGURATION 0`).
    Unconfigured,
    /// The `detach` command took the device off the bus.
    Detached,
    /// The `release` command.
    Released,
    /// A line on the control link that is not a command: malformed, not
    /// UTF-8, or longer than the longest line.
    Protocol,
}

const REASONS: [Reason; 9] = [
    Reason::None,
    Reason::Expired,
    Reason::Closed,
    Reason::Suspended,
    Reason::Reset,
    Reason::Unconfigured,
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
            Self::Unconfigured => "unconfigured",
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
    /// A length of zero or more than [`MAX_MS`], or a deadline further than
    /// the lease's length from the device's time.
    BadLength,
    /// The deadline is not after the device's time: the command was delayed
    /// on its way.
    Stale,
    /// The grant or renewal names another connection's token, or the boot
    /// has no token, or the connection has no grant number left (the device
    /// then reboots, [`crate::device::Outcome::Reboot`]).
    Session,
    /// A lease is already held: release it first.
    Held,
    /// No lease is held.
    NoLease,
    /// The id is not the held lease's: it belongs to an ended lease or
    /// another connection.
    WrongId,
    /// The lease's pattern is already armed.
    Armed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    n: u32,
    ms: u32,
    deadline: u64,
    armed: bool,
}

/// The lease state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Lease {
    held: Option<Held>,
    /// The connection's last grant number.
    last_n: u32,
    reason: Reason,
}

impl Lease {
    /// No lease held, none ended.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            held: None,
            last_n: 0,
            reason: Reason::None,
        }
    }

    /// Grants a lease of `ms` whose first deadline is `deadline`: its
    /// number within the connection and its deadline. Numbers only grow
    /// within a connection and never wrap, so an ended lease's id never
    /// acts again.
    ///
    /// # Errors
    ///
    /// [`Refusal::BadLength`] for a length of zero or above [`MAX_MS`], or a
    /// deadline further than `ms` from `now`; [`Refusal::Held`];
    /// [`Refusal::Stale`] when `deadline` is not after `now`;
    /// [`Refusal::Session`] when the connection's numbers are spent.
    pub fn grant(&mut self, now: u64, ms: u32, deadline: u64) -> Result<(u32, u64), Refusal> {
        if ms == 0 || ms > MAX_MS {
            return Err(Refusal::BadLength);
        }
        if self.held.is_some() {
            return Err(Refusal::Held);
        }
        if deadline <= now {
            return Err(Refusal::Stale);
        }
        if deadline > now.saturating_add(u64::from(ms) * 1000) {
            return Err(Refusal::BadLength);
        }
        self.last_n = self.last_n.checked_add(1).ok_or(Refusal::Session)?;
        self.held = Some(Held {
            n: self.last_n,
            ms,
            deadline,
            armed: false,
        });
        Ok((self.last_n, deadline))
    }

    fn held_as(&mut self, n: u32) -> Result<&mut Held, Refusal> {
        match &mut self.held {
            None => Err(Refusal::NoLease),
            Some(h) if h.n != n => Err(Refusal::WrongId),
            Some(h) => Ok(h),
        }
    }

    /// Moves lease `n`'s deadline to `deadline`, which the host derived
    /// from a check it made: a renewal delayed on its way can only end the
    /// lease sooner, never later.
    ///
    /// # Errors
    ///
    /// [`Refusal::NoLease`] or [`Refusal::WrongId`]; [`Refusal::Stale`]
    /// when `deadline` is not after `now`; [`Refusal::BadLength`] when it
    /// lies further than the lease's length from `now`.
    pub fn renew(&mut self, n: u32, now: u64, deadline: u64) -> Result<u64, Refusal> {
        let h = self.held_as(n)?;
        if deadline <= now {
            return Err(Refusal::Stale);
        }
        if deadline > now.saturating_add(u64::from(h.ms) * 1000) {
            return Err(Refusal::BadLength);
        }
        h.deadline = deadline;
        Ok(deadline)
    }

    /// Arms the loaded pattern under lease `n`.
    ///
    /// # Errors
    ///
    /// [`Refusal::NoLease`], [`Refusal::WrongId`] or [`Refusal::Armed`].
    pub fn arm(&mut self, n: u32) -> Result<(), Refusal> {
        let h = self.held_as(n)?;
        if h.armed {
            return Err(Refusal::Armed);
        }
        h.armed = true;
        Ok(())
    }

    /// The held lease's number: held input is allowed.
    #[must_use]
    pub const fn held(&self) -> Option<u32> {
        match self.held {
            Some(h) => Some(h.n),
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

    /// Starts the connection's grant numbers after `n`.
    #[cfg(test)]
    pub(crate) const fn set_last_grant(&mut self, n: u32) {
        self.last_n = n;
    }

    /// A new connection: its grants are numbered from 1.
    pub const fn new_connection(&mut self) {
        self.last_n = 0;
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
    fn a_lease_expires_at_its_deadline_and_renews_to_the_deadline_asked() {
        let mut l = Lease::default();
        assert_eq!(l.grant(0, 0, 1), Err(Refusal::BadLength));
        assert_eq!(l.grant(0, MAX_MS + 1, 1), Err(Refusal::BadLength));
        let (n, deadline) = l.grant(1_000, 5_000, 5_001_000).unwrap();
        assert_eq!((n, deadline), (1, 5_001_000));
        assert_eq!(l.grant(1_000, 5_000, 5_001_000), Err(Refusal::Held));
        assert!(!l.expired(deadline - 1));
        assert_eq!(l.renew(n, 2_000_000, 6_500_000), Ok(6_500_000));
        assert!(!l.expired(deadline));
        assert!(l.expired(6_500_000));
        // At most the lease's length from now.
        assert_eq!(l.renew(n, 2_000_000, 7_000_000), Ok(7_000_000));
        assert_eq!(l.renew(n, 2_000_000, 7_000_001), Err(Refusal::BadLength));
        assert!(l.expired(7_000_000), "a refused renewal changes nothing");
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
        let (old, _) = l.grant(0, 1_000, 1_000_000).unwrap();
        l.arm(old).unwrap();
        assert_eq!(l.arm(old), Err(Refusal::Armed));
        assert!(l.armed());
        assert!(l.disarm(Reason::Suspended));
        assert!(!l.armed());
        assert_eq!(l.arm(old), Err(Refusal::NoLease));
        let (new, _) = l.grant(0, 1_000, 1_000_000).unwrap();
        assert_ne!(new, old);
        assert!(!l.armed(), "a new lease starts disarmed");
        assert_eq!(l.arm(old), Err(Refusal::WrongId));
        assert_eq!(l.renew(old, 0, 1), Err(Refusal::WrongId));
        l.arm(new).unwrap();
        assert!(l.armed());
    }

    /// A grant delayed past the deadline its host set is refused: its
    /// delay cannot start a lease the host's check no longer covers.
    #[test]
    fn a_grant_that_arrives_after_its_deadline_is_refused() {
        let mut l = Lease::default();
        assert_eq!(l.grant(204_000, 250, 204_000), Err(Refusal::Stale));
        assert_eq!(l.grant(204_000, 250, 200_000), Err(Refusal::Stale));
        assert_eq!(l.held(), None);
        // In time, the lease ends at the deadline asked, not at receipt
        // plus its length.
        assert_eq!(l.grant(204_000, 250, 300_000), Ok((1, 300_000)));
        assert!(!l.expired(299_999));
        assert!(l.expired(300_000));
    }

    /// A renewal delayed past its deadline is refused and leaves the lease
    /// to expire where it was: a slow host cannot keep input held.
    #[test]
    fn a_renewal_that_arrives_after_its_deadline_is_refused() {
        let mut l = Lease::default();
        let (n, deadline) = l.grant(0, 250, 250_000).unwrap();
        assert_eq!(deadline, 250_000);
        assert_eq!(l.renew(n, 204_000, 204_000), Err(Refusal::Stale));
        assert_eq!(l.renew(n, 204_000, 200_000), Err(Refusal::Stale));
        assert!(l.expired(250_000));
        assert_eq!(l.renew(n, 204_000, 334_000), Ok(334_000));
    }

    /// Neither a grant nor a renewal sets a deadline further than the
    /// lease's length from the device's time.
    #[test]
    fn a_deadline_past_the_length_is_refused() {
        let mut l = Lease::default();
        assert_eq!(l.grant(0, 250, 250_001), Err(Refusal::BadLength));
        let (n, _) = l.grant(0, 250, 250_000).unwrap();
        assert_eq!(l.renew(n, 100_000, 350_001), Err(Refusal::BadLength));
        assert!(l.expired(250_000), "a refused renewal changes nothing");
        assert_eq!(l.renew(n, 100_000, 350_000), Ok(350_000));
    }

    #[test]
    fn every_reason_parses_back() {
        for r in REASONS {
            assert_eq!(Reason::parse(r.name()), Some(r));
        }
    }
}
