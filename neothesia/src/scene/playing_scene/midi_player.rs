use midi_file::midly::{num::u4, MidiMessage};

use crate::{
    output_manager::OutputConnection,
    song::{PlayerConfig, Song},
};
use neothesia_core::piano_layout;
use std::{
    collections::HashSet,
    time::Duration,
};

pub fn is_black_key(midi_note: u8) -> bool {
    let pitch = midi_note % 12;
    pitch == 1 || pitch == 3 || pitch == 6 || pitch == 8 || pitch == 10
}

pub struct MidiPlayer {
    playback: midi_file::PlaybackState,
    output: OutputConnection,
    song: Song,
    play_along: PlayAlong,
    separate_channels: bool,
    /// `note.start` của mọi nốt thuộc track Human, sắp xếp tăng dần. Cache 1 lần lúc khởi
    /// tạo (track player config chỉ chỉnh được ở menu, không đổi khi đang chơi) để
    /// `pull_in_chord_siblings` tra cứu bằng binary search thay vì quét lại toàn bộ bài
    /// mỗi lần có NoteOn — quét toàn bộ mỗi frame từng gây giật hình ở PlayAlong.
    human_note_starts: Vec<Duration>,
    human_note_targets: Vec<Duration>,
}

impl MidiPlayer {
    pub fn new(
        output: OutputConnection,
        song: Song,
        user_keyboard_range: piano_layout::KeyboardRange,
        separate_channels: bool,
    ) -> Self {
        Self::new_with_lead_in(
            output,
            song,
            user_keyboard_range,
            separate_channels,
            Duration::from_secs(3),
        )
    }

    pub fn new_with_lead_in(
        output: OutputConnection,
        song: Song,
        user_keyboard_range: piano_layout::KeyboardRange,
        separate_channels: bool,
        lead_in: Duration,
    ) -> Self {
        let mut human_note_starts: Vec<Duration> = song
            .file
            .tracks
            .iter()
            .filter(|track| song.config.tracks[track.track_id].player == PlayerConfig::Human)
            .flat_map(|track| track.notes.iter())
            .map(|note| note.start)
            .collect();
        human_note_starts.sort_unstable();

        let human_note_targets: Vec<Duration> = human_note_starts
            .iter()
            .map(|&start| start + lead_in)
            .collect();

        let mut player = Self {
            playback: midi_file::PlaybackState::new(lead_in, song.file.tracks.clone()),
            output,
            play_along: PlayAlong::new(user_keyboard_range),
            song,
            separate_channels,
            human_note_starts,
            human_note_targets,
        };
        // Let's reset programs,
        // for timestamp 0 most likely all programs will be 0, so this should clean any leftovers
        // from previous songs
        player.send_midi_programs_for_timestamp(&player.playback.time());
        player.update(Duration::ZERO);

        player
    }

    pub fn song(&self) -> &Song {
        &self.song
    }

    /// When playing: returns midi events
    ///
    /// When paused: returns None
    pub fn update(&mut self, mut delta: Duration) -> Vec<midi_file::MidiEvent> {
        self.play_along.update();

        // Clamp delta so playback stops EXACTLY at the next Human note target,
        // preventing frames from leaping over fast / repeated notes.
        let running = self.playback.time();

        let idx = self.human_note_targets.partition_point(|&target| target <= running);
        if let Some(&next_target) = self.human_note_targets.get(idx) {
            if next_target > running {
                let time_to_next = next_target - running;
                if time_to_next < delta {
                    delta = time_to_next;
                }
            }
        }

        let mut events: Vec<midi_file::MidiEvent> =
            self.playback.update(delta).into_iter().cloned().collect();

        self.pull_in_chord_siblings(&mut events);

        events.iter().for_each(|event| {
            let config = &self.song.config.tracks[event.track_id];

            let channel = if self.separate_channels {
                event.track_color_id as u8
            } else {
                event.channel
            };
            match config.player {
                PlayerConfig::Auto => {
                    self.output
                        .midi_event(u4::new(channel), event.message);
                }
                PlayerConfig::Human => {
                    let needs_led = self.play_along
                        .midi_event(MidiEventSource::File, &event.message);

                    match event.message {
                        midi_file::midly::MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
                            let key_num = key.as_int();
                            log::info!("[WAIT_GATE] File NoteOn key={}, required={:?}", key_num, self.play_along.required_notes());
                            if needs_led {
                                let custom_vel = if is_black_key(key_num) { 120 } else { 80 };
                                self.output.midi_event(
                                    u4::new(channel),
                                    midi_file::midly::MidiMessage::NoteOn {
                                        key,
                                        vel: custom_vel.into(),
                                    },
                                );
                            }
                        }
                        midi_file::midly::MidiMessage::NoteOff { .. } => {
                            // Do NOT send NoteOff to LED or remove from required_notes on Human tracks!
                            // The LED guide must stay ON until the USER physically strikes the key.
                            // Extinguishing LED on file NoteOff would prematurely kill LEDs on repeated/overlapping notes.
                        }
                        other => {
                            self.output.midi_event(u4::new(channel), other);
                        }
                    }
                }
                PlayerConfig::Mute => {}
            }
        });

        events
    }

    pub fn clear(&mut self) {
        self.output.stop_all();
        self.play_along.clear();
    }

    /// Humanized MIDI files often have chord notes that start a few ms apart
    /// instead of at the exact same timestamp. The Wait-For-Me gate in
    /// `PlayingScene::update_midi_player` freezes the timeline as soon as the
    /// *first* dispatched note of such a chord becomes required, so any
    /// sibling note that hasn't reached its own timestamp yet never gets a
    /// chance to join `required_notes` — the player can get stuck holding
    /// the first note forever while waiting on a note that will never arrive.
    ///
    /// To fix this, once we've just dispatched a Human NoteOn, look ahead a
    /// fixed CHORD_CLUSTER_WINDOW from that note's timestamp and eagerly pull
    /// in (dispatch) any sibling notes inside that window, so the whole
    /// chord becomes required — and lit up — together.
    fn pull_in_chord_siblings(&mut self, events: &mut Vec<midi_file::MidiEvent>) {
        const CHORD_CLUSTER_WINDOW: Duration = Duration::from_millis(20);

        let anchor_start = events
            .iter()
            .filter(|event| {
                matches!(event.message, MidiMessage::NoteOn { vel, .. } if vel.as_int() > 0)
                    && self.song.config.tracks[event.track_id].player == PlayerConfig::Human
            })
            .map(|event| event.timestamp)
            .min();

        let Some(anchor_start) = anchor_start else {
            return;
        };
        let deadline = anchor_start + CHORD_CLUSTER_WINDOW;

        let mut existing_keys: HashSet<u8> = events
            .iter()
            .filter_map(|event| match event.message {
                MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => Some(key.as_int()),
                _ => None,
            })
            .collect();

        loop {
            let leed_in = *self.playback.leed_in();
            let running = self.playback.time();

            let search_from = running.saturating_sub(leed_in);
            let idx = self.human_note_starts.partition_point(|&start| start <= search_from);
            let next_pending_start = self
                .human_note_starts
                .get(idx)
                .copied()
                .filter(|&start| start <= deadline);

            let Some(next_start) = next_pending_start else {
                break;
            };

            // If any note at next_start is a repeated note of an already required key,
            // DO NOT pull it in (it is a sequential repeated strike, not a simultaneous chord).
            let has_duplicate_key = self.song.file.tracks.iter()
                .filter(|track| self.song.config.tracks[track.track_id].player == PlayerConfig::Human)
                .flat_map(|track| track.notes.iter())
                .any(|note| note.start == next_start && existing_keys.contains(&note.note));

            if has_duplicate_key {
                break;
            }

            let extra_delta = (next_start + leed_in).saturating_sub(running);
            let new_events = self.playback.update(extra_delta);
            if new_events.is_empty() {
                break;
            }
            for ev in &new_events {
                if let MidiMessage::NoteOn { key, vel } = ev.message {
                    if vel.as_int() > 0 {
                        existing_keys.insert(key.as_int());
                    }
                }
            }
            events.extend(new_events.into_iter().cloned());
        }
    }
}

impl Drop for MidiPlayer {
    fn drop(&mut self) {
        self.clear();
    }
}

impl MidiPlayer {
    pub fn pause_resume(&mut self) {
        if self.playback.is_paused() {
            self.resume();
        } else {
            self.pause();
        }
    }

    pub fn pause(&mut self) {
        self.clear();
        self.playback.pause();
    }

    pub fn resume(&mut self) {
        self.playback.resume();
        self.play_along.clear();
    }

    fn send_midi_programs_for_timestamp(&self, time: &Duration) {
        for (&channel, &p) in self.song.file.program_track.program_for_timestamp(time) {
            self.output.midi_event(
                u4::new(channel),
                midi_file::midly::MidiMessage::ProgramChange {
                    program: midi_file::midly::num::u7::new(p),
                },
            );
        }
    }

    pub fn sync_state_at_current_time(&mut self) {
        self.clear();
        self.play_along.clear();
        let time = self.playback.time();
        self.send_midi_programs_for_timestamp(&time);

        let leed_in = *self.playback.leed_in();
        let song_time = if time >= leed_in {
            time - leed_in
        } else {
            Duration::ZERO
        };

        // 1. Collect notes active at song_time OR find the earliest upcoming chord
        let mut active_or_next_notes = Vec::new();
        let mut min_upcoming_start: Option<Duration> = None;

        for track in self.song.file.tracks.iter() {
            let config = &self.song.config.tracks[track.track_id];
            if config.player == PlayerConfig::Mute {
                continue;
            }

            for note in track.notes.iter() {
                if note.start <= song_time && song_time < (note.start + note.duration) {
                    active_or_next_notes.push((track.track_id, note.clone()));
                } else if note.start >= song_time {
                    match min_upcoming_start {
                        None => min_upcoming_start = Some(note.start),
                        Some(min_t) => {
                            if note.start < min_t {
                                min_upcoming_start = Some(note.start);
                            }
                        }
                    }
                }
            }
        }

        // If no active note is playing right now, snap to the nearest upcoming notes
        if active_or_next_notes.is_empty() {
            if let Some(next_start) = min_upcoming_start {
                self.set_time_silent(next_start + leed_in);

                for track in self.song.file.tracks.iter() {
                    let config = &self.song.config.tracks[track.track_id];
                    if config.player == PlayerConfig::Mute {
                        continue;
                    }

                    for note in track.notes.iter() {
                        if note.start >= next_start && note.start <= next_start + Duration::from_millis(30) {
                            active_or_next_notes.push((track.track_id, note.clone()));
                        }
                    }
                }
            }
        }

        // Light up LEDs on piano and register required notes
        for (track_id, note) in active_or_next_notes {
            let config = &self.song.config.tracks[track_id];
            let channel = if self.separate_channels {
                note.track_color_id as u8
            } else {
                note.channel
            };

            let custom_vel = if is_black_key(note.note) { 120 } else { 80 };

            let msg = midi_file::midly::MidiMessage::NoteOn {
                key: note.note.into(),
                vel: custom_vel.into(),
            };

            match config.player {
                PlayerConfig::Auto => {
                    self.output.midi_event(u4::new(channel), msg);
                }
                PlayerConfig::Human => {
                    self.play_along.midi_event(MidiEventSource::File, &msg);
                    self.output.midi_event(u4::new(channel), msg);
                }
                PlayerConfig::Mute => {}
            }
        }
    }

    pub fn set_time_silent(&mut self, time: Duration) {
        self.playback.set_time(time);
        let events = self.playback.update(Duration::ZERO);
        std::mem::drop(events);
    }

    pub fn set_percentage_time_silent(&mut self, p: f32) {
        self.set_time_silent(self.percentage_to_time(p));
    }

    pub fn rewind_silent(&mut self, delta: i64) {
        let mut time = self.playback.time();

        if delta < 0 {
            let delta = Duration::from_millis((-delta) as u64);
            time = time.saturating_sub(delta);
        } else {
            let delta = Duration::from_millis(delta as u64);
            time = time.saturating_add(delta);
        }

        self.set_time_silent(time);
    }

    pub fn set_time(&mut self, time: Duration) {
        self.set_time_silent(time);
        self.sync_state_at_current_time();
    }

    pub fn rewind(&mut self, delta: i64) {
        self.rewind_silent(delta);
        self.sync_state_at_current_time();
    }

    pub fn percentage_to_time(&self, p: f32) -> Duration {
        Duration::from_secs_f32((p * self.playback.length().as_secs_f32()).max(0.0))
    }

    pub fn time_to_percentage(&self, time: &Duration) -> f32 {
        time.as_secs_f32() / self.playback.length().as_secs_f32()
    }

    pub fn set_percentage_time(&mut self, p: f32) {
        self.set_time(self.percentage_to_time(p));
    }

    pub fn leed_in(&self) -> &Duration {
        self.playback.leed_in()
    }

    pub fn length(&self) -> Duration {
        self.playback.length()
    }

    pub fn percentage(&self) -> f32 {
        self.playback.percentage()
    }

    pub fn is_finished(&self) -> bool {
        self.playback.is_finished()
    }

    pub fn time(&self) -> Duration {
        self.playback.time()
    }

    pub fn time_without_lead_in(&self) -> f32 {
        self.playback.time().as_secs_f32() - self.playback.leed_in().as_secs_f32()
    }

    pub fn is_paused(&self) -> bool {
        self.playback.is_paused()
    }
}

impl MidiPlayer {
    pub fn play_along(&self) -> &PlayAlong {
        &self.play_along
    }

    pub fn user_midi_event(&mut self, _channel: u8, message: &MidiMessage) {
        match message {
            MidiMessage::NoteOn { key, vel } => {
                let key_num = key.as_int();
                if vel.as_int() > 0 {
                    let satisfied = self.play_along.midi_event(MidiEventSource::User, message);
                    log::info!("[USER_INPUT] NoteOn key={}, vel={}, satisfied={}, remaining_required={:?}", key_num, vel.as_int(), satisfied, self.play_along.required_notes());
                    // When user hits a required note, extinguish its LED guide on the piano
                    if satisfied {
                        self.output.midi_event(
                            u4::new(0),
                            midi_file::midly::MidiMessage::NoteOff {
                                key: *key,
                                vel: 0.into(),
                            },
                        );
                    }
                } else {
                    self.play_along.midi_event(MidiEventSource::User, message);
                }
            }
            MidiMessage::NoteOff { .. } => {
                self.play_along.midi_event(MidiEventSource::User, message);
            }
            _ => {}
        }
    }
}

pub enum MidiEventSource {
    File,
    User,
}

type NoteId = u8;

#[derive(Debug, Default)]
struct PlayerStats {
    wrong_notes: usize,
    played_early: Vec<Duration>,
    played_late: Vec<Duration>,
}

#[derive(Debug)]
pub struct PlayAlong {
    user_keyboard_range: piano_layout::KeyboardRange,
    required_notes: HashSet<NoteId>,
    stats: PlayerStats,
}

impl PlayAlong {
    fn new(user_keyboard_range: piano_layout::KeyboardRange) -> Self {
        Self {
            user_keyboard_range,
            required_notes: Default::default(),
            stats: PlayerStats::default(),
        }
    }

    fn update(&mut self) {}

    pub fn required_notes(&self) -> &HashSet<NoteId> {
        &self.required_notes
    }

    fn user_press_key(&mut self, note_id: u8, active: bool) -> bool {
        if active {
            // Check if note has arrived from file and is waiting for user
            self.required_notes.remove(&note_id)
        } else {
            false
        }
    }

    fn file_press_key(&mut self, note_id: u8, active: bool) -> bool {
        if active {
            self.required_notes.insert(note_id);
            true
        } else {
            // NoteOff from file must NOT remove required notes;
            // only the user striking the key should satisfy/remove the note.
            false
        }
    }

    fn press_key(&mut self, src: MidiEventSource, note_id: u8, active: bool) -> bool {
        if !self.user_keyboard_range.contains(note_id) {
            return false;
        }

        match src {
            MidiEventSource::User => self.user_press_key(note_id, active),
            MidiEventSource::File => self.file_press_key(note_id, active),
        }
    }

    pub fn midi_event(&mut self, source: MidiEventSource, message: &MidiMessage) -> bool {
        match message {
            MidiMessage::NoteOn { key, vel } => {
                let active = vel.as_int() > 0;
                self.press_key(source, key.as_int(), active)
            }
            MidiMessage::NoteOff { key, .. } => {
                self.press_key(source, key.as_int(), false)
            }
            _ => false,
        }
    }

    pub fn clear(&mut self) {
        self.required_notes.clear();
    }

    pub fn are_required_keys_pressed(&self) -> bool {
        self.required_notes.is_empty()
    }
}
