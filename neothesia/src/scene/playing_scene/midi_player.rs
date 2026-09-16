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
        let mut player = Self {
            playback: midi_file::PlaybackState::new(lead_in, song.file.tracks.clone()),
            output,
            play_along: PlayAlong::new(user_keyboard_range),
            song,
            separate_channels,
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
    pub fn update(&mut self, delta: Duration) -> Vec<&midi_file::MidiEvent> {
        self.play_along.update();

        let events = self.playback.update(delta);

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
                    self.play_along
                        .midi_event(MidiEventSource::File, &event.message);

                    // For Human PlayAlong, customize velocity for visual distinction on piano LED
                    let msg = match event.message {
                        midi_file::midly::MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
                            let key_num = key.as_int();
                            let custom_vel = if is_black_key(key_num) { 120 } else { 80 };
                            midi_file::midly::MidiMessage::NoteOn {
                                key,
                                vel: custom_vel.into(),
                            }
                        }
                        other => other,
                    };
                    self.output.midi_event(u4::new(channel), msg);
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
                        if note.start >= next_start && note.start <= next_start + Duration::from_millis(40) {
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
                let note_id = key.as_int();
                if vel.as_int() > 0 {
                    self.play_along.midi_event(MidiEventSource::User, message);
                    // When user hits a required note, extinguish its LED guide on the piano
                    if self.play_along.required_notes().contains(&note_id) {
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
                    // If user releases a key while chord is not fully complete, re-light it
                    if self.play_along.required_notes().contains(&note_id) {
                        let custom_vel = if is_black_key(note_id) { 120 } else { 80 };
                        self.output.midi_event(
                            u4::new(0),
                            midi_file::midly::MidiMessage::NoteOn {
                                key: *key,
                                vel: custom_vel.into(),
                            },
                        );
                    }
                }
            }
            MidiMessage::NoteOff { key, .. } => {
                let note_id = key.as_int();
                self.play_along.midi_event(MidiEventSource::User, message);
                // If user releases a key while chord is not fully complete, re-light it
                if self.play_along.required_notes().contains(&note_id) {
                    let custom_vel = if is_black_key(note_id) { 120 } else { 80 };
                    self.output.midi_event(
                        u4::new(0),
                        midi_file::midly::MidiMessage::NoteOn {
                            key: *key,
                            vel: custom_vel.into(),
                        },
                    );
                }
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
    user_held_keys: HashSet<NoteId>,
    stats: PlayerStats,
}

impl PlayAlong {
    fn new(user_keyboard_range: piano_layout::KeyboardRange) -> Self {
        Self {
            user_keyboard_range,
            required_notes: Default::default(),
            user_held_keys: Default::default(),
            stats: PlayerStats::default(),
        }
    }

    fn update(&mut self) {}

    pub fn required_notes(&self) -> &HashSet<NoteId> {
        &self.required_notes
    }

    fn user_press_key(&mut self, note_id: u8, active: bool) {
        if active {
            self.user_held_keys.insert(note_id);
        } else {
            self.user_held_keys.remove(&note_id);
        }
    }

    fn file_press_key(&mut self, note_id: u8, active: bool) {
        if active {
            self.required_notes.insert(note_id);
        } else {
            self.required_notes.remove(&note_id);
        }
    }

    fn press_key(&mut self, src: MidiEventSource, note_id: u8, active: bool) {
        if !self.user_keyboard_range.contains(note_id) {
            return;
        }

        match src {
            MidiEventSource::User => self.user_press_key(note_id, active),
            MidiEventSource::File => self.file_press_key(note_id, active),
        }
    }

    pub fn midi_event(&mut self, source: MidiEventSource, message: &MidiMessage) {
        match message {
            MidiMessage::NoteOn { key, vel } => {
                let active = vel.as_int() > 0;
                self.press_key(source, key.as_int(), active);
            }
            MidiMessage::NoteOff { key, .. } => {
                self.press_key(source, key.as_int(), false);
            }
            _ => {}
        }
    }

    pub fn clear(&mut self) {
        self.required_notes.clear();
        self.user_held_keys.clear();
    }

    pub fn are_required_keys_pressed(&self) -> bool {
        if self.required_notes.is_empty() {
            return true;
        }
        // All currently required notes (e.g. chord) must be held simultaneously
        self.required_notes.iter().all(|note| self.user_held_keys.contains(note))
    }
}
