//! What one production frame gave the program to read: the only thing
//! the reference takes from production.

/// The host a frame's reads are answered by (s7-06e-2-design.md §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Host {
    /// A play frame: every mapped read.
    Play,
    /// A frame production builds no play frame for: only reads loading
    /// fields `allowed` accepts (by slot), and the clock; `page` is the
    /// practice page's ordinal in the script, `None` while loading.
    Outside {
        allowed: Vec<bool>,
        page: Option<usize>,
    },
}

/// One frame's inputs: the play schema's fields, the timers' readings
/// (ms, `TIMER_OFF` while off), the state clock and the host.
#[derive(Clone, Debug)]
pub struct FrameInputs {
    fields: Vec<i32>,
    timers: Vec<i32>,
    state_us: i64,
    host: Host,
    /// On a play frame, the play state's score data as the source keeps
    /// it: `None` before its first judgment, else `[ex_score,
    /// rate.current_bp, rate.whole_bp]` at its last (s7-06e-3-design.md
    /// §2a).
    score: Option<[i32; 3]>,
}

impl FrameInputs {
    pub fn new(fields: &[i32], timers: &[i32], state_us: i64) -> Self {
        Self {
            fields: fields.to_vec(),
            timers: timers.to_vec(),
            state_us,
            host: Host::Play,
            score: None,
        }
    }

    /// The frame with the play state's score data `score`.
    #[must_use]
    pub fn with_score(mut self, score: Option<[i32; 3]>) -> Self {
        self.score = score;
        self
    }

    pub fn score(&self) -> Option<[i32; 3]> {
        self.score
    }

    /// A frame of `host`, which reads `fields` and no timer.
    pub fn outside(fields: &[i32], allowed: Vec<bool>, page: Option<usize>, state_us: i64) -> Self {
        Self {
            fields: fields.to_vec(),
            timers: Vec::new(),
            state_us,
            host: Host::Outside { allowed, page },
            score: None,
        }
    }

    pub fn fields(&self) -> &[i32] {
        &self.fields
    }

    pub fn timers(&self) -> &[i32] {
        &self.timers
    }

    pub fn state_us(&self) -> i64 {
        self.state_us
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    pub fn is_play(&self) -> bool {
        self.host == Host::Play
    }
}
