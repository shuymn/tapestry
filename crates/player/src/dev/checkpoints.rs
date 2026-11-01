//! When a slow load's loading frames count as following one another
//! (dev-tools): the one rule the slow load holds by and its test checks,
//! which includes this file.

/// How many times the count must grow after its first positive checkpoint.
pub const GROWTHS: usize = 2;

/// Whether the loading frames presented, read at successive checkpoints,
/// grew at [`GROWTHS`] or more checkpoints after the first one that saw a
/// frame: frames kept being presented, not one drawn and then stopped.
pub fn frames_follow(checkpoints: &[u64]) -> bool {
    let Some(first) = checkpoints.iter().position(|&n| n > 0) else {
        return false;
    };
    checkpoints
        .get(first..)
        .unwrap_or_default()
        .windows(2)
        .filter(|w| w.get(1) > w.first())
        .count()
        >= GROWTHS
}

#[cfg(test)]
mod tests {
    use super::frames_follow;

    #[test]
    fn frames_follow_once_the_count_grows_twice_after_the_first_frame() {
        assert!(!frames_follow(&[]));
        assert!(!frames_follow(&[0, 0, 0]));
        // Drawn once and stopped.
        assert!(!frames_follow(&[0, 1, 1, 1, 1]));
        // Four frames between two checkpoints: one growth only.
        assert!(!frames_follow(&[0, 0, 4]));
        assert!(!frames_follow(&[0, 4, 4, 5]));
        assert!(frames_follow(&[0, 4, 5, 5, 6]));
        assert!(frames_follow(&[1, 2, 3]));
    }
}
