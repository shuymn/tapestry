//! Where the media worker runs (design.md "Threat Model" T02): a subset of
//! the whole processor cores of the game's primary processor group, bound
//! by the worker's job, so the worker's decoding, however many players it
//! runs, uses those cores alone, and no core's two hardware threads are
//! split by the bound. The standard technique is the one game engines use
//! for background work: the system's topology read from its CPU sets, and
//! the background work's affinity kept to whole cores. It bounds the worker
//! only: the game's own scheduling is left alone, its threads may still run
//! on the worker's cores. A process that may run on fewer than two whole
//! cores gets no worker: its videos are lost, the rest of the game goes on.
//!
//! The split itself is pure, so a test can try every topology.

use std::collections::BTreeMap;

/// A logical processor, as the system's CPU sets describe it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cpu {
    /// Its CPU set's id.
    pub id: u32,
    /// Its processor group.
    pub group: u16,
    /// Its index in the group (its bit in the group's affinity masks).
    pub index: u8,
    /// Its core's index in the group: the hardware threads of one core
    /// share it.
    pub core: u8,
    /// Its core's efficiency class: higher is faster.
    pub efficiency: u8,
    /// Not reserved for another process.
    pub free: bool,
}

/// Where the worker runs, in one group, and the cores it leaves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The game's primary group.
    pub group: u16,
    /// The worker's logical processors, as the group's affinity mask.
    pub worker: usize,
    /// The whole cores of the group the game may run on that the worker
    /// leaves, as an affinity mask.
    pub game: usize,
    /// The whole cores each side has.
    pub worker_cores: usize,
    pub game_cores: usize,
}

/// Splits the whole cores of `group` that `allowed` (the game's affinity in
/// that group) holds every hardware thread of, and that no other process
/// has reserved: the worker takes half of them, rounded down, the slowest
/// first (by efficiency class, then the highest core index); the rest is
/// left for the game. `None` when fewer than two such cores remain: no
/// subset leaves the game a core.
pub fn split(cpus: &[Cpu], group: u16, allowed: usize) -> Option<Placement> {
    let mut cores: BTreeMap<u8, (u8, Vec<&Cpu>)> = BTreeMap::new();
    for cpu in cpus.iter().filter(|c| c.group == group) {
        let entry = cores
            .entry(cpu.core)
            .or_insert_with(|| (cpu.efficiency, Vec::new()));
        entry.0 = entry.0.min(cpu.efficiency);
        entry.1.push(cpu);
    }
    let usable =
        |c: &&Cpu| c.free && u32::from(c.index) < usize::BITS && allowed >> c.index & 1 == 1;
    let mut whole: Vec<(u8, u8, Vec<&Cpu>)> = cores
        .into_iter()
        .filter(|(_, (_, threads))| threads.iter().all(usable))
        .map(|(core, (efficiency, threads))| (efficiency, core, threads))
        .collect();
    if whole.len() < 2 {
        return None;
    }
    // The slowest first, then the highest index: the worker's.
    whole.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let worker_cores = whole.len() / 2;
    let (worker, game) = whole.split_at(worker_cores);
    let mask = |side: &[(u8, u8, Vec<&Cpu>)]| -> usize {
        side.iter()
            .flat_map(|(_, _, threads)| threads)
            .map(|c| 1_usize << c.index)
            .fold(0, |m, b| m | b)
    };
    Some(Placement {
        group,
        worker: mask(worker),
        game: mask(game),
        worker_cores,
        game_cores: game.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cores` cores of `threads` hardware threads each in group `group`,
    /// numbered as Windows numbers them (a core's threads adjacent), ids
    /// from `first_id`, all of efficiency class `efficiency`.
    fn cpus(group: u16, cores: u8, threads: u8, efficiency: u8, first_id: u32) -> Vec<Cpu> {
        (0..cores)
            .flat_map(|core| {
                (0..threads).map(move |t| Cpu {
                    id: first_id + u32::from(core * threads + t),
                    group,
                    index: core * threads + t,
                    core,
                    efficiency,
                    free: true,
                })
            })
            .collect()
    }

    // Covers: T02
    #[test]
    fn whole_cores_split_with_both_threads_on_one_side() {
        // Four cores of two threads: the worker the upper two cores, the
        // game the lower two, no core shared.
        let p = split(&cpus(0, 4, 2, 0, 256), 0, 0xFF).unwrap();
        assert_eq!((p.worker, p.game), (0xF0, 0x0F));
        assert_eq!((p.worker_cores, p.game_cores), (2, 2));
        // Five cores: the worker two, the game three.
        let p = split(&cpus(0, 5, 2, 0, 0), 0, 0x3FF).unwrap();
        assert_eq!((p.worker, p.game), (0x3C0, 0x03F));
        // A game allowed one thread of a core leaves that core out.
        let p = split(&cpus(0, 4, 2, 0, 0), 0, 0b0111_1111).unwrap();
        assert_eq!((p.worker, p.game), (0b0011_0000, 0b0000_1111));
        assert_eq!(p.worker & p.game, 0);
    }

    // Covers: T02
    #[test]
    fn no_reserve_gives_no_placement() {
        // One core, with its two threads: the game cannot keep one.
        assert_eq!(split(&cpus(0, 1, 2, 0, 0), 0, 0b11), None);
        // Two cores, the game allowed only one whole.
        assert_eq!(split(&cpus(0, 2, 2, 0, 0), 0, 0b0111), None);
        // Two cores, one reserved for another process.
        let mut reserved = cpus(0, 2, 1, 0, 0);
        reserved[1].free = false;
        assert_eq!(split(&reserved, 0, 0b11), None);
        // A single CPU.
        assert_eq!(split(&cpus(0, 1, 1, 0, 0), 0, 0b1), None);
    }

    // Covers: T02
    #[test]
    fn the_worker_takes_the_slowest_cores_of_the_primary_group() {
        // Two fast cores of two threads, then four efficient cores of one:
        // the worker the three slowest, the game the rest.
        let mut hybrid = cpus(0, 2, 2, 1, 0);
        hybrid.extend((0..4).map(|i| Cpu {
            id: 4 + i,
            group: 0,
            index: u8::try_from(4 + i).unwrap(),
            core: u8::try_from(2 + i).unwrap(),
            efficiency: 0,
            free: true,
        }));
        let p = split(&hybrid, 0, 0xFF).unwrap();
        assert_eq!((p.worker_cores, p.game_cores), (3, 3));
        assert_eq!(p.worker, 0b1110_0000);
        assert_eq!(p.game, 0b0001_1111);
        // Two groups: only the primary group's cores are split.
        let mut two = cpus(0, 4, 2, 0, 0);
        two.extend(cpus(1, 4, 2, 0, 8));
        let p = split(&two, 1, 0xFF).unwrap();
        assert_eq!((p.group, p.worker, p.game), (1, 0xF0, 0x0F));
    }
}
