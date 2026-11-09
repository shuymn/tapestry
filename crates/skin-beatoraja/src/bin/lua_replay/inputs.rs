//! What one production frame gave the program to read: the only thing
//! the reference takes from production.

/// One frame's inputs: the play schema's fields, the timers' readings
/// (ms, `TIMER_OFF` while off) and the state clock.
#[derive(Clone, Debug)]
pub struct FrameInputs {
    fields: Vec<i32>,
    timers: Vec<i32>,
    state_us: i64,
}

impl FrameInputs {
    pub fn new(fields: &[i32], timers: &[i32], state_us: i64) -> Self {
        Self {
            fields: fields.to_vec(),
            timers: timers.to_vec(),
            state_us,
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
}
