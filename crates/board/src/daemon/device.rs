//! The device-wide limit on the runs the dispatcher has at once (`docs/DESIGN.md`, "Daemon
//! dispatch", Device limit).
//!
//! A board's own limits (`parallel_tasks` workers, [`super::judges::JUDGES_AT_ONCE`]
//! judges) say nothing of the device: each run is an agent session of some hundred MiB,
//! and its builds take more, so the boards a device dispatches together must share one
//! limit. Workers and judges count alike. One slot is kept for judges, since a judge ends
//! work already in progress, and a board that was refused a slot gets the next free one
//! before a board that asked later ([`Turns`]).

use std::collections::BTreeMap;

use board_proto::{BoardSlug, Timestamp};

/// Host memory that allows one run, when `runs_at_once` is not set.
pub const MEMORY_PER_RUN: u64 = 4 << 30;

/// The least default limit: one worker and one judge.
pub const MIN_RUNS: usize = 2;

/// The limit of a device whose memory could not be read.
pub const FALLBACK_RUNS: usize = 4;

/// The limit a device with `host_memory` bytes gets when `runs_at_once` is not set. The
/// memory is rounded to the nearest run: a host reports somewhat less than it has.
pub fn default_run_limit(host_memory: Option<u64>) -> usize {
    host_memory.map_or(FALLBACK_RUNS, |bytes| {
        usize::try_from(bytes.saturating_add(MEMORY_PER_RUN / 2) / MEMORY_PER_RUN)
            .unwrap_or(usize::MAX)
            .max(MIN_RUNS)
    })
}

/// How many of `limit` runs workers may take: all but the judges' slot.
pub fn worker_limit(limit: usize) -> usize {
    limit.saturating_sub(1).max(1)
}

/// The memory of this host in bytes, when it can be read.
pub fn host_memory() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8(out.stdout).ok()?.trim().parse().ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        mem_total(&std::fs::read_to_string("/proc/meminfo").ok()?)
    }
}

/// The `MemTotal` of a `/proc/meminfo` text, in bytes.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn mem_total(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
    let kib: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
    kib.checked_mul(1024)
}

/// The boards that were refused a slot of one kind, oldest first served. Kept in memory:
/// after a restart the boards ask again in the order they are read.
#[derive(Debug, Default)]
pub struct Turns {
    waiting: BTreeMap<BoardSlug, Waiting>,
}

#[derive(Debug, Clone, Copy)]
struct Waiting {
    since: Timestamp,
    asked: Timestamp,
}

impl Turns {
    /// Takes `slug` out of the queue and gives the time it has waited since, if it
    /// waited. A board that still wants a slot and gets none goes back with [`Self::wait`].
    pub fn take(&mut self, slug: &BoardSlug) -> Option<Timestamp> {
        self.waiting.remove(slug).map(|w| w.since)
    }

    /// Whether a board other than `slug` has waited for a slot since before `since`,
    /// `slug`'s own wait, if it waited.
    pub fn another_first(&self, slug: &BoardSlug, since: Option<Timestamp>) -> bool {
        self.waiting
            .iter()
            .any(|(other, w)| other != slug && since.is_none_or(|mine| w.since < mine))
    }

    /// Drops the boards that have not asked since `last`, the reconcile before this one:
    /// a board that no longer wants a slot, or that the device no longer dispatches, must
    /// not hold the others back.
    pub fn keep_asked_since(&mut self, last: Timestamp) {
        self.waiting.retain(|_, w| w.asked >= last);
    }

    /// Records that `slug` was refused a slot at `now`, having waited since `since` if it
    /// waited before. True when its wait starts now.
    pub fn wait(&mut self, slug: &BoardSlug, since: Option<Timestamp>, now: Timestamp) -> bool {
        self.waiting.insert(
            slug.clone(),
            Waiting {
                since: since.unwrap_or(now),
                asked: now,
            },
        );
        since.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slug(s: &str) -> BoardSlug {
        s.parse().unwrap()
    }

    fn at(minute: i64) -> Timestamp {
        Timestamp::from_timestamp(1_800_000_000 + minute * 60, 0).unwrap()
    }

    #[test]
    fn the_default_limit_is_one_run_per_four_gib_and_at_least_two() {
        assert_eq!(default_run_limit(Some(16 << 30)), 4);
        // A 16 GiB Linux host reports about 15.5 GiB.
        assert_eq!(default_run_limit(Some(16_303_412 * 1024)), 4);
        assert_eq!(default_run_limit(Some(36 << 30)), 9);
        assert_eq!(default_run_limit(Some(4 << 30)), MIN_RUNS);
        assert_eq!(default_run_limit(Some(0)), MIN_RUNS);
        assert_eq!(default_run_limit(None), FALLBACK_RUNS);
    }

    #[test]
    fn workers_leave_one_slot_to_the_judges() {
        assert_eq!(worker_limit(4), 3);
        assert_eq!(worker_limit(2), 1);
        assert_eq!(worker_limit(1), 1);
    }

    #[test]
    fn mem_total_reads_proc_meminfo() {
        let text = "MemTotal:       16303412 kB\nMemFree:          812340 kB\n";
        assert_eq!(mem_total(text), Some(16_303_412 * 1024));
        assert_eq!(mem_total("MemFree: 1 kB\n"), None);
    }

    #[test]
    fn the_board_that_waited_longest_goes_first() {
        let (a, b) = (slug("alpha"), slug("beta"));
        let mut turns = Turns::default();
        assert!(!turns.another_first(&a, None));
        assert!(turns.wait(&b, None, at(0)));
        // `a` never waited, so `b` goes first; `b` itself is held back by nobody.
        assert!(turns.another_first(&a, None));
        assert!(turns.wait(&a, None, at(1)));
        let since = turns.take(&b);
        assert_eq!(since, Some(at(0)));
        assert!(!turns.another_first(&b, since));
        // Refused again without a start, `b` keeps its place.
        assert!(!turns.wait(&b, since, at(2)));
        let since = turns.take(&a);
        assert!(turns.another_first(&a, since));
        // Once `b` is served, `a` is next.
        turns.take(&b);
        assert!(!turns.another_first(&a, since));
    }

    #[test]
    fn a_board_that_was_served_waits_behind_the_boards_that_asked() {
        let (a, b) = (slug("alpha"), slug("beta"));
        let mut turns = Turns::default();
        turns.wait(&a, None, at(0));
        turns.wait(&b, None, at(1));
        // `a` gets a slot, then is refused for its next task: its wait starts anew.
        assert!(turns.take(&a).is_some());
        assert!(turns.wait(&a, None, at(2)));
        let since = turns.take(&b);
        assert!(!turns.another_first(&b, since));
        turns.wait(&b, since, at(3));
        let since = turns.take(&a);
        assert!(turns.another_first(&a, since));
    }

    #[test]
    fn a_board_that_stopped_asking_loses_its_place() {
        let (a, b) = (slug("alpha"), slug("beta"));
        let mut turns = Turns::default();
        turns.wait(&b, None, at(0));
        // It asked in the last pass: kept.
        turns.keep_asked_since(at(0));
        assert!(turns.another_first(&a, None));
        // It did not ask in the pass after: dropped, with its place.
        turns.keep_asked_since(at(1));
        assert!(!turns.another_first(&a, None));
        assert_eq!(turns.take(&b), None);
    }
}
