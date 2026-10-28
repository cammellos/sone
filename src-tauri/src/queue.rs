//! Play queue for the headless daemon: what plays now, what comes next,
//! shuffle and repeat. Pure state; the daemon drives the audio engine from it.
//!
//! Entries keep their insertion order in `entries`; `order` is the play order
//! as indices into `entries` (identity unless shuffled), and `pos` points into
//! `order`. Turning shuffle off restores insertion order around the current
//! entry.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueueTrack {
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_seconds: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Unique per queue entry, so the same track queued twice stays two
    /// entries (the audio engine's gapless slot is keyed by it).
    pub qid: u64,
    pub track: QueueTrack,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

/// What `previous` decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Previous {
    /// Seek the current entry back to the start.
    Restart,
    /// Play the previous entry.
    Moved,
}

/// Seconds into a track after which "previous" restarts it instead of going back.
const RESTART_THRESHOLD_SECS: f32 = 3.0;

#[derive(Default, Debug)]
pub struct Queue {
    entries: Vec<Entry>,
    order: Vec<usize>,
    pos: Option<usize>,
    next_qid: u64,
    shuffle: bool,
    repeat: Repeat,
}

impl Queue {
    pub fn current(&self) -> Option<&Entry> {
        self.pos.map(|p| &self.entries[self.order[p]])
    }

    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    pub fn set_repeat(&mut self, repeat: Repeat) {
        self.repeat = repeat;
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries after the current one, in play order (no repeat wrap-around).
    pub fn upcoming(&self) -> impl Iterator<Item = &Entry> {
        let start = self.pos.map_or(0, |p| p + 1);
        self.order[start.min(self.order.len())..]
            .iter()
            .map(|&i| &self.entries[i])
    }

    /// Replace the queue and make `start` the current entry. With shuffle on,
    /// the rest is shuffled with `start` kept first.
    pub fn replace(
        &mut self,
        tracks: Vec<QueueTrack>,
        start: usize,
        rand_below: &mut impl FnMut(usize) -> usize,
    ) -> Option<&Entry> {
        self.entries.clear();
        self.order.clear();
        self.pos = None;
        for t in tracks {
            self.push_entry(t);
        }
        self.order = (0..self.entries.len()).collect();
        if self.entries.is_empty() {
            return None;
        }
        let start = start.min(self.entries.len() - 1);
        self.pos = Some(start);
        if self.shuffle {
            self.shuffle_around_current(rand_below);
        }
        self.current()
    }

    /// Append to the end of the play order.
    pub fn append(&mut self, tracks: Vec<QueueTrack>) {
        for t in tracks {
            let i = self.push_entry(t);
            self.order.push(i);
        }
        if self.pos.is_none() && !self.order.is_empty() {
            self.pos = Some(0);
        }
    }

    /// Insert right after the current entry, keeping their order.
    pub fn insert_next(&mut self, tracks: Vec<QueueTrack>) {
        let mut at = self.pos.map_or(0, |p| p + 1);
        for t in tracks {
            let i = self.push_entry(t);
            self.order.insert(at, i);
            at += 1;
        }
        if self.pos.is_none() && !self.order.is_empty() {
            self.pos = Some(0);
        }
    }

    /// The entry that will follow the current one when it ends naturally.
    pub fn peek_next(&self) -> Option<&Entry> {
        let p = self.pos?;
        let next = match self.repeat {
            Repeat::One => p,
            Repeat::All => (p + 1) % self.order.len(),
            Repeat::Off if p + 1 < self.order.len() => p + 1,
            Repeat::Off => return None,
        };
        Some(&self.entries[self.order[next]])
    }

    /// The current entry ended on its own: move to what `peek_next` showed.
    /// Returns `None` (and leaves the last entry current) at the end of the queue.
    pub fn advance(&mut self) -> Option<&Entry> {
        let p = self.pos?;
        let next = match self.repeat {
            Repeat::One => p,
            Repeat::All => (p + 1) % self.order.len(),
            Repeat::Off if p + 1 < self.order.len() => p + 1,
            Repeat::Off => return None,
        };
        self.pos = Some(next);
        self.current()
    }

    /// The user skipped: like `advance`, but repeat-one moves on.
    pub fn skip_next(&mut self) -> Option<&Entry> {
        let p = self.pos?;
        let next = if p + 1 < self.order.len() {
            p + 1
        } else if self.repeat != Repeat::Off {
            0
        } else {
            return None;
        };
        self.pos = Some(next);
        self.current()
    }

    pub fn previous(&mut self, position_secs: f32) -> Option<Previous> {
        let p = self.pos?;
        if position_secs > RESTART_THRESHOLD_SECS {
            return Some(Previous::Restart);
        }
        if p > 0 {
            self.pos = Some(p - 1);
            Some(Previous::Moved)
        } else if self.repeat == Repeat::All {
            self.pos = Some(self.order.len() - 1);
            Some(Previous::Moved)
        } else {
            Some(Previous::Restart)
        }
    }

    /// Jump to the `n`th upcoming entry (0 = the next one).
    pub fn jump_upcoming(&mut self, n: usize) -> Option<&Entry> {
        let target = self.pos.map_or(0, |p| p + 1) + n;
        if target >= self.order.len() {
            return None;
        }
        self.pos = Some(target);
        self.current()
    }

    /// Remove the first upcoming entry with this track id. The current entry
    /// is never removed. Returns whether something was removed.
    pub fn remove_upcoming(&mut self, track_id: u64) -> bool {
        let start = self.pos.map_or(0, |p| p + 1);
        let Some(order_idx) = (start..self.order.len())
            .find(|&o| self.entries[self.order[o]].track.id == track_id)
        else {
            return false;
        };
        let entry_idx = self.order.remove(order_idx);
        self.entries.remove(entry_idx);
        for i in &mut self.order {
            if *i > entry_idx {
                *i -= 1;
            }
        }
        true
    }

    /// Drop everything except the current entry.
    pub fn clear_upcoming(&mut self) {
        match self.current().cloned() {
            Some(cur) => {
                self.entries = vec![cur];
                self.order = vec![0];
                self.pos = Some(0);
            }
            None => *self = Self {
                next_qid: self.next_qid,
                shuffle: self.shuffle,
                repeat: self.repeat,
                ..Self::default()
            },
        }
    }

    pub fn set_shuffle(&mut self, on: bool, rand_below: &mut impl FnMut(usize) -> usize) {
        if on == self.shuffle {
            return;
        }
        self.shuffle = on;
        if on {
            self.shuffle_around_current(rand_below);
        } else {
            let cur = self.pos.map(|p| self.order[p]);
            self.order = (0..self.entries.len()).collect();
            self.pos = cur;
        }
    }

    fn push_entry(&mut self, track: QueueTrack) -> usize {
        self.next_qid += 1;
        self.entries.push(Entry {
            qid: self.next_qid,
            track,
        });
        self.entries.len() - 1
    }

    /// Current entry first, everything else in random order after it.
    fn shuffle_around_current(&mut self, rand_below: &mut impl FnMut(usize) -> usize) {
        let cur = self.pos.map(|p| self.order[p]);
        let mut rest: Vec<usize> = (0..self.entries.len()).filter(|&i| Some(i) != cur).collect();
        for i in (1..rest.len()).rev() {
            rest.swap(i, rand_below(i + 1));
        }
        self.order = cur.into_iter().chain(rest).collect();
        self.pos = cur.map(|_| 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: u64) -> QueueTrack {
        QueueTrack {
            id,
            title: format!("t{id}"),
            artist: "a".into(),
            album: None,
            duration_seconds: 60,
        }
    }

    fn ids<'a>(it: impl Iterator<Item = &'a Entry>) -> Vec<u64> {
        it.map(|e| e.track.id).collect()
    }

    /// Deterministic "random": always picks index 0, i.e. reverses via swaps.
    fn first(_: usize) -> usize {
        0
    }

    fn queue_of(n: u64, start: usize) -> Queue {
        let mut q = Queue::default();
        q.replace((1..=n).map(t).collect(), start, &mut first);
        q
    }

    #[test]
    fn replace_starts_at_index() {
        let q = queue_of(4, 2);
        assert_eq!(q.current().unwrap().track.id, 3);
        assert_eq!(ids(q.upcoming()), vec![4]);
    }

    #[test]
    fn advance_stops_at_end_without_repeat() {
        let mut q = queue_of(2, 0);
        assert_eq!(q.advance().unwrap().track.id, 2);
        assert!(q.advance().is_none());
        assert_eq!(q.current().unwrap().track.id, 2);
    }

    #[test]
    fn repeat_all_wraps_and_repeat_one_stays() {
        let mut q = queue_of(2, 1);
        q.set_repeat(Repeat::All);
        assert_eq!(q.peek_next().unwrap().track.id, 1);
        assert_eq!(q.advance().unwrap().track.id, 1);
        q.set_repeat(Repeat::One);
        assert_eq!(q.advance().unwrap().track.id, 1);
        // a user skip moves on even with repeat-one
        assert_eq!(q.skip_next().unwrap().track.id, 2);
    }

    #[test]
    fn previous_restarts_after_threshold() {
        let mut q = queue_of(3, 1);
        assert_eq!(q.previous(10.0), Some(Previous::Restart));
        assert_eq!(q.current().unwrap().track.id, 2);
        assert_eq!(q.previous(1.0), Some(Previous::Moved));
        assert_eq!(q.current().unwrap().track.id, 1);
        assert_eq!(q.previous(1.0), Some(Previous::Restart));
    }

    #[test]
    fn insert_next_goes_after_current_in_order() {
        let mut q = queue_of(3, 0);
        q.insert_next(vec![t(10), t(11)]);
        assert_eq!(ids(q.upcoming()), vec![10, 11, 2, 3]);
        q.append(vec![t(12)]);
        assert_eq!(ids(q.upcoming()), vec![10, 11, 2, 3, 12]);
    }

    #[test]
    fn same_track_twice_gets_distinct_qids() {
        let mut q = queue_of(1, 0);
        q.append(vec![t(1)]);
        let next = q.peek_next().unwrap().qid;
        assert_ne!(q.current().unwrap().qid, next);
    }

    #[test]
    fn shuffle_keeps_current_first_and_unshuffle_restores_order() {
        let mut q = queue_of(5, 2);
        q.set_shuffle(true, &mut first);
        assert_eq!(q.current().unwrap().track.id, 3);
        let mut up = ids(q.upcoming());
        assert_eq!(up.len(), 4);
        up.sort();
        assert_eq!(up, vec![1, 2, 4, 5]);
        q.set_shuffle(false, &mut first);
        assert_eq!(q.current().unwrap().track.id, 3);
        assert_eq!(ids(q.upcoming()), vec![4, 5]);
    }

    #[test]
    fn remove_upcoming_never_removes_current() {
        let mut q = queue_of(3, 0);
        assert!(!q.remove_upcoming(1));
        assert!(q.remove_upcoming(2));
        assert_eq!(ids(q.upcoming()), vec![3]);
        assert_eq!(q.current().unwrap().track.id, 1);
    }

    #[test]
    fn clear_upcoming_keeps_current() {
        let mut q = queue_of(3, 1);
        q.clear_upcoming();
        assert_eq!(q.current().unwrap().track.id, 2);
        assert_eq!(q.upcoming().count(), 0);
    }

    #[test]
    fn jump_upcoming_skips_ahead() {
        let mut q = queue_of(5, 0);
        assert_eq!(q.jump_upcoming(2).unwrap().track.id, 4);
        assert!(q.jump_upcoming(5).is_none());
    }

    #[test]
    fn append_to_empty_queue_sets_current() {
        let mut q = Queue::default();
        q.append(vec![t(7)]);
        assert_eq!(q.current().unwrap().track.id, 7);
    }
}
