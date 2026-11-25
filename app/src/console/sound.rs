//! Console mode UI sounds, played through rodio's native audio output.
//!
//! Sound packages live beside the executable at `sounds/<style>/`.

use std::collections::HashMap;
use std::io::{BufReader, Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use heronite_data::console_theme::{ControlOptions, SoundStyle, SOUND_VOLUME};
use rodio::cpal::traits::{DeviceTrait, HostTrait};
use rodio::{buffer::SamplesBuffer, Decoder, OutputStream, OutputStreamHandle, Sink, Source};

use super::Pad;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Sound {
    Move,
    Confirm,
    Back,
    Tab,
    Launch,
    Notification,
    Restore,
}

impl Sound {
    const ALL: [Sound; 7] = [Sound::Move, Sound::Confirm, Sound::Back, Sound::Tab, Sound::Launch, Sound::Notification, Sound::Restore];

    fn name(self) -> &'static str {
        match self {
            Sound::Move => "move",
            Sound::Confirm => "confirm",
            Sound::Back => "back",
            Sound::Tab => "tab",
            Sound::Launch => "launch",
            Sound::Notification => "notification",
            Sound::Restore => "restore",
        }
    }

    /// The tones as (Hz, ms), played one after the other, and the level
    /// (0..=1, at full volume).
    fn tones(self, style: SoundStyle) -> (&'static [(f32, f32)], f32) {
        match (style, self) {
            (SoundStyle::Classic, Sound::Move) => (&[(1150., 26.)], 0.22),
            (SoundStyle::Classic, Sound::Confirm | Sound::Launch | Sound::Notification | Sound::Restore) => {
                (&[(660., 45.), (990., 80.)], 0.3)
            }
            (SoundStyle::Classic, Sound::Back) => (&[(720., 45.), (480., 80.)], 0.3),
            (SoundStyle::Classic, Sound::Tab) => (&[(880., 34.), (1180., 52.)], 0.26),
            // Low and round: a tick that dips a little, then rising (G4 to D5)
            // and falling (A4 to E4) fifths for yes and back.
            (SoundStyle::Soft, Sound::Move) => (&[(520., 60.)], 0.26),
            (SoundStyle::Soft, Sound::Confirm | Sound::Launch | Sound::Notification | Sound::Restore) => {
                (&[(392., 50.), (587., 110.)], 0.34)
            }
            (SoundStyle::Soft, Sound::Back) => (&[(440., 50.), (330., 100.)], 0.34),
            (SoundStyle::Soft, Sound::Tab) => (&[(494., 38.), (587., 64.)], 0.3),
        }
    }

    fn gap(self) -> Duration {
        match self {
            Sound::Move => Duration::from_millis(35),
            _ => Duration::from_millis(40),
        }
    }
}

/// What a press sounds like (`None`: silent, like the right stick).
pub fn for_pad(pad: Pad) -> Option<Sound> {
    match pad {
        Pad::Up | Pad::Down | Pad::Left | Pad::Right => Some(Sound::Move),
        Pad::A | Pad::X | Pad::Y | Pad::Start | Pad::Select | Pad::LT | Pad::RT => Some(Sound::Confirm),
        Pad::B => Some(Sound::Back),
        Pad::LB | Pad::RB => Some(Sound::Tab),
        Pad::ScrollUp | Pad::ScrollDown => None,
    }
}

const RATE: u32 = 22_050;
const MAX_SOUND_SECONDS: usize = 5;
const MAX_SAMPLE_RATE: u32 = 192_000;

/// How a built-in tone is shaped.
struct Voice {
    attack: f32,
    decay: f32,
    harmonic: f32,
    glide: f32,
}

impl Voice {
    fn of(style: SoundStyle) -> Self {
        match style {
            SoundStyle::Classic => Voice { attack: 0.004, decay: 2., harmonic: 0., glide: 1. },
            SoundStyle::Soft => Voice { attack: 0.008, decay: 2.5, harmonic: 0.3, glide: 0.97 },
        }
    }
}

/// What the sounds are played with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Settings {
    pub style: String,
    /// Percent, [`SOUND_VOLUME`].
    pub volume: u8,
    pub dynamic_pitch: bool,
    pub output_device: String,
}

impl Settings {
    pub fn of(c: &ControlOptions, output_device: String) -> Self {
        Self {
            style: c.sound_style.clone(),
            volume: c.sound_volume.clamp(SOUND_VOLUME.0, SOUND_VOLUME.1),
            dynamic_pitch: c.dynamic_pitch,
            output_device,
        }
    }

    /// The factor on a sound's level: steeper than linear, as loudness is.
    fn gain(&self) -> f32 {
        (f32::from(self.volume) / 100.).powf(1.5)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyleOption {
    pub id: String,
    pub label: String,
}

struct StyleCatalog {
    root: Option<PathBuf>,
    styles: Vec<StyleOption>,
}

const LEGACY_SOUND_README: &str = "External UI sound packages\n\
\n\
Create one subfolder per style inside sounds/, for example sounds/PS5/.\n\
Use these WAV file names:\n\
  move.wav     Navigation between items\n\
  confirm.wav  Confirming a selection\n\
  back.wav     Going back\n\
  tab.wav      Changing tabs\n\
  launch.wav   Launching a game\n\
\n\
Each WAV file must be no longer than 5 seconds. A style appears when it has\n\
at least one valid sound file. Missing sounds use a built-in fallback.\n";

const SOUND_README: &str = "External UI sound packages\n\
\n\
Create one subfolder per style inside sounds/, for example sounds/PS5/.\n\
Use .wav, .ogg, .mp3, or .flac files with these names:\n\
  move        Navigation between items\n\
  confirm     Confirming a selection\n\
  back        Going back\n\
  tab         Changing tabs\n\
  launch      Game launch begins (falls back to confirm.wav)\n\
  notification Toast/banner appears (falls back to confirm.wav)\n\
  restore     Heronite window is restored (falls back to confirm.wav)\n\
\n\
Each sound must be no longer than 5 seconds. A style appears when it has at\n\
least one valid sound file. Missing navigation sounds use built-in sounds;\n\
launch, notification and restore use confirm if their file is absent.\n\
Select Audio Output Device in Console Settings. System Default follows the\n\
current system output; unavailable selected devices fall back to that default.\n";

fn sound_root() -> Option<PathBuf> {
    match std::env::current_exe() {
        Ok(exe) => exe.parent().map(|dir| dir.join("sounds")),
        Err(e) => {
            log::warn!("console sounds: cannot locate executable for sound packs: {e}");
            None
        }
    }
}

fn prepare_sound_folder(root: &Path) {
    if let Err(e) = std::fs::create_dir_all(root) {
        log::warn!("console sounds: cannot create {}: {e}", root.display());
        return;
    }
    let readme = root.join("README.txt");
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&readme) {
        Ok(mut file) => {
            if let Err(e) = file.write_all(SOUND_README.as_bytes()) {
                log::warn!("console sounds: cannot write {}: {e}", readme.display());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match std::fs::read_to_string(&readme) {
            Ok(contents)
                if contents == LEGACY_SOUND_README || contents.contains("Game process successfully starts (optional; no fallback)") =>
            {
                if let Err(e) = std::fs::write(&readme, SOUND_README) {
                    log::warn!("console sounds: cannot update {}: {e}", readme.display());
                }
            }
            Ok(_) => {}
            Err(e) => log::warn!("console sounds: cannot inspect {}: {e}", readme.display()),
        },
        Err(e) => log::warn!("console sounds: cannot create {}: {e}", readme.display()),
    }
}

fn catalog() -> &'static StyleCatalog {
    static CATALOG: OnceLock<StyleCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let root = sound_root();
        if let Some(root) = &root {
            prepare_sound_folder(root);
        }
        let styles = root.as_deref().map(discover_styles).unwrap_or_default();
        StyleCatalog { root, styles }
    })
}

/// Creates the user sound-pack folder and its guide without starting audio.
pub fn prepare() {
    let _ = catalog();
}

/// Built-in styles followed by valid external folders in alphabetical order.
pub fn available_styles() -> Vec<StyleOption> {
    catalog().styles.clone()
}

fn resolve_style(style: &str) -> String {
    let styles = &catalog().styles;
    styles.iter().find(|candidate| candidate.id == style).map_or_else(|| "classic".to_owned(), |candidate| candidate.id.clone())
}

struct AudioSample {
    channels: u16,
    sample_rate: u32,
    samples: Vec<f32>,
}

impl AudioSample {
    fn from_audio(bytes: Vec<u8>) -> Result<Self> {
        let decoder = Decoder::new(Cursor::new(bytes))?;
        let channels = decoder.channels();
        let sample_rate = decoder.sample_rate();
        if channels == 0 || channels > 2 || sample_rate == 0 || sample_rate > MAX_SAMPLE_RATE {
            return Err(anyhow!("audio has an invalid channel count or sample rate"));
        }
        let max_samples = sample_rate as usize * usize::from(channels) * MAX_SOUND_SECONDS;
        let samples: Vec<_> = decoder.take(max_samples.saturating_add(1)).map(|sample| f32::from(sample) / f32::from(i16::MAX)).collect();
        if samples.is_empty() {
            return Err(anyhow!("audio contains no samples"));
        }
        if samples.len() > max_samples {
            return Err(anyhow!("sound effects must be no longer than {MAX_SOUND_SECONDS} seconds"));
        }
        Ok(Self { channels, sample_rate, samples })
    }
}

/// A mono 16-bit WAV of the tones, none louder than `level` (0..=1).
fn wav(tones: &[(f32, f32)], level: f32, voice: &Voice) -> Vec<u8> {
    let mut samples: Vec<i16> = Vec::new();
    for &(hz, ms) in tones {
        let n = (RATE as f32 * ms / 1000.) as usize;
        let mut phase = 0f32;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let progress = i as f32 / n as f32;
            let attack = 0.5 - 0.5 * (std::f32::consts::PI * (t / voice.attack).min(1.)).cos();
            let envelope = attack * (1. - progress).powf(voice.decay);
            let now = hz * (1. + (voice.glide - 1.) * progress);
            phase += std::f32::consts::TAU * now / RATE as f32;
            let tone = (phase.sin() + voice.harmonic * (2. * phase).sin()) / (1. + voice.harmonic);
            samples.push((tone * envelope * level * f32::from(i16::MAX)) as i16);
        }
    }
    let data = samples.len() as u32 * 2;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

fn built_in(style: SoundStyle, sound: Sound) -> AudioSample {
    let (tones, level) = sound.tones(style);
    AudioSample::from_audio(wav(tones, level, &Voice::of(style))).expect("built-in WAV is valid")
}

fn load_external(path: &std::path::Path) -> Option<AudioSample> {
    match std::fs::read(path) {
        Ok(bytes) => match AudioSample::from_audio(bytes) {
            Ok(sample) => Some(sample),
            Err(e) => {
                log::warn!("console sounds: cannot load {}: {e}; using built-in sound", path.display());
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            log::warn!("console sounds: cannot read {}: {e}; using built-in sound", path.display());
            None
        }
    }
}

fn validate_external(path: &Path) -> Result<()> {
    let file = std::fs::File::open(path)?;
    let decoder = Decoder::new(BufReader::new(file))?;
    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();
    if channels == 0 || channels > 2 || sample_rate == 0 || sample_rate > MAX_SAMPLE_RATE {
        return Err(anyhow!("audio has an invalid channel count or sample rate"));
    }
    let max_samples = sample_rate as usize * usize::from(channels) * MAX_SOUND_SECONDS;
    let samples = decoder.take(max_samples.saturating_add(1)).count();
    if samples == 0 {
        return Err(anyhow!("audio contains no samples"));
    }
    if samples > max_samples {
        return Err(anyhow!("sound effects must be no longer than {MAX_SOUND_SECONDS} seconds"));
    }
    Ok(())
}

#[cfg(test)]
fn load_or_builtin(path: Option<&std::path::Path>, fallback: AudioSample) -> AudioSample {
    path.and_then(load_external).unwrap_or(fallback)
}

fn custom_pack_valid(root: &Path, style: &str) -> bool {
    let dir = root.join(style);
    if !dir.is_dir() {
        return false;
    }
    let mut has_valid_sound = false;
    for sound in Sound::ALL {
        for extension in ["wav", "ogg", "mp3", "flac"] {
            let path = dir.join(format!("{}.{}", sound.name(), extension));
            match std::fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => {
                    if let Err(e) = validate_external(&path) {
                        log::warn!("console sounds: invalid {}: {e}; using Classic", path.display());
                        return false;
                    }
                    has_valid_sound = true;
                }
                Ok(_) => return false,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    log::warn!("console sounds: cannot inspect {}: {e}", path.display());
                    return false;
                }
            }
        }
    }
    has_valid_sound
}

fn discover_styles(root: &Path) -> Vec<StyleOption> {
    let mut styles =
        vec![StyleOption { id: "soft".into(), label: "Soft".into() }, StyleOption { id: "classic".into(), label: "Classic".into() }];
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return styles,
        Err(e) => {
            log::warn!("console sounds: cannot scan {}: {e}", root.display());
            return styles;
        }
    };
    let mut custom = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let Ok(file_type) = entry.file_type() else { continue };
        if !file_type.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if id.eq_ignore_ascii_case("soft") || id.eq_ignore_ascii_case("classic") {
            continue;
        }
        if custom_pack_valid(root, &id) {
            custom.push(StyleOption { label: id.clone(), id });
        }
    }
    custom.sort_by(|a, b| a.id.to_lowercase().cmp(&b.id.to_lowercase()).then_with(|| a.id.cmp(&b.id)));
    styles.extend(custom);
    styles
}

fn builtin_style(style: &str) -> SoundStyle {
    match style {
        "soft" => SoundStyle::Soft,
        _ => SoundStyle::Classic,
    }
}

fn load_style(root: Option<&Path>, style: &str) -> HashMap<Sound, AudioSample> {
    let is_custom = style != "soft" && style != "classic";
    if is_custom && !root.is_some_and(|root| custom_pack_valid(root, style)) {
        log::warn!("console sounds: sound pack {style:?} is missing or invalid; using Classic");
        return load_style(root, "classic");
    }

    let fallback_style = builtin_style(style);
    let mut sounds = HashMap::new();
    for sound in Sound::ALL {
        let sample = root.and_then(|root| {
            ["wav", "ogg", "mp3", "flac"]
                .into_iter()
                .map(|extension| root.join(style).join(format!("{}.{}", sound.name(), extension)))
                .find(|path| path.is_file())
                .and_then(|path| load_external(&path))
        });
        if let Some(sample) = sample {
            sounds.insert(sound, sample);
        } else if !matches!(sound, Sound::Launch | Sound::Notification | Sound::Restore) {
            sounds.insert(sound, built_in(fallback_style, sound));
        }
    }
    sounds
}

fn sample_for(sounds: &HashMap<Sound, AudioSample>, sound: Sound) -> Option<&AudioSample> {
    if matches!(sound, Sound::Launch | Sound::Notification | Sound::Restore) && !sounds.contains_key(&sound) {
        sounds.get(&Sound::Confirm)
    } else {
        sounds.get(&sound)
    }
}

struct Request {
    sound: Sound,
    settings: Settings,
    pitch: f32,
    replaces_press: bool,
}

static OUTPUT_DEVICE_NAMES: OnceLock<Arc<RwLock<Vec<String>>>> = OnceLock::new();

fn output_device_names() -> Arc<RwLock<Vec<String>>> {
    OUTPUT_DEVICE_NAMES
        .get_or_init(|| {
            let names = Arc::new(RwLock::new(Vec::new()));
            let worker_names = names.clone();
            if let Err(e) = std::thread::Builder::new().name("audio-devices".into()).spawn(move || loop {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(scan_output_devices));
                match result {
                    Ok(Ok(found)) => {
                        if let Ok(mut current) = worker_names.write() {
                            *current = found;
                        } else {
                            log::warn!("console sounds: output device cache lock was poisoned");
                        }
                    }
                    Ok(Err(e)) => log::warn!("console sounds: cannot enumerate output devices: {e}"),
                    Err(_) => log::warn!("console sounds: output device enumeration panicked; will retry"),
                }
                std::thread::sleep(Duration::from_secs(10));
            }) {
                log::warn!("console sounds: could not start output device scanner: {e}");
            }
            names
        })
        .clone()
}

/// Starts the device-list worker without waiting for its first scan.
pub fn start_output_device_scan() {
    let _ = output_device_names();
}

fn scan_output_devices() -> Result<Vec<String>> {
    let host = rodio::cpal::default_host();
    let devices = host.output_devices()?;
    let mut names = Vec::new();
    for device in devices {
        match device.name() {
            Ok(name) if !names.contains(&name) => names.push(name),
            Ok(_) => {}
            Err(e) => log::warn!("console sounds: cannot read output device name: {e}"),
        }
    }
    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    Ok(names)
}

/// Returns the last asynchronously scanned device list without touching CPAL.
pub fn available_output_devices() -> Vec<String> {
    output_device_names().read().map(|names| names.clone()).unwrap_or_else(|_| {
        log::warn!("console sounds: output device cache lock was poisoned");
        Vec::new()
    })
}

fn default_output_name() -> String {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rodio::cpal::default_host().default_output_device().and_then(|device| device.name().ok()).unwrap_or_default()
    }))
    .unwrap_or_else(|_| {
        log::warn!("console sounds: default output device query panicked");
        String::new()
    })
}

const OUTPUT_RETRY: Duration = Duration::from_secs(2);
const OUTPUT_REFRESH: Duration = Duration::from_secs(5);
const DEVICE_POLL: Duration = Duration::from_secs(1);

struct AudioOutput {
    _stream: OutputStream,
    handle: OutputStreamHandle,
    selected_device: String,
    default_device: String,
    connected_at: Instant,
}

fn open_output(selected_device: &str) -> Result<AudioOutput> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open_output_inner(selected_device)))
        .unwrap_or_else(|_| Err(anyhow!("audio output initialization panicked")))
}

fn open_output_inner(selected_device: &str) -> Result<AudioOutput> {
    let host = rodio::cpal::default_host();
    let default_device = host.default_output_device().and_then(|device| device.name().ok()).unwrap_or_default();
    if !selected_device.is_empty() {
        match host.output_devices() {
            Ok(devices) => {
                if let Some(device) = devices.into_iter().find(|device| device.name().is_ok_and(|name| name == selected_device)) {
                    match OutputStream::try_from_device(&device) {
                        Ok((stream, handle)) => {
                            return Ok(AudioOutput {
                                _stream: stream,
                                handle,
                                selected_device: selected_device.to_owned(),
                                default_device,
                                connected_at: Instant::now(),
                            });
                        }
                        Err(e) => log::warn!("console sounds: cannot open selected output {selected_device:?}: {e}; trying system default"),
                    }
                } else {
                    log::warn!("console sounds: selected output {selected_device:?} is unavailable; trying system default");
                }
            }
            Err(e) => log::warn!("console sounds: cannot enumerate outputs: {e}; trying system default"),
        }
    }
    let (stream, handle) = OutputStream::try_default()?;
    Ok(AudioOutput { _stream: stream, handle, selected_device: selected_device.to_owned(), default_device, connected_at: Instant::now() })
}

const PRESS_WINDOW: Duration = Duration::from_millis(100);
const MOVE_PITCH_WINDOW: Duration = Duration::from_millis(180);
const MOVE_SCALE_SEMITONES: [u32; 5] = [0, 2, 4, 7, 9];

fn move_pitch(last_move: &mut Option<Instant>, step: &mut usize, now: Instant) -> f32 {
    if last_move.is_some_and(|last| now.duration_since(last) < MOVE_PITCH_WINDOW) {
        *step = (*step + 1) % MOVE_SCALE_SEMITONES.len();
    } else {
        *step = 0;
    }
    *last_move = Some(now);
    2f32.powf(MOVE_SCALE_SEMITONES[*step] as f32 / 12.)
}

fn pitch_for(sound: Sound, dynamic_pitch: bool, preview: bool, last_move: &mut Option<Instant>, step: &mut usize, now: Instant) -> f32 {
    if sound == Sound::Move && dynamic_pitch && !preview {
        move_pitch(last_move, step, now)
    } else {
        if !preview {
            *last_move = None;
            *step = 0;
        }
        1.
    }
}

/// Plays the sounds from one worker thread; it owns rodio's output stream and
/// keeps only the active package's decoded sounds resident.
struct Player {
    tx: Sender<Request>,
    last: [Option<Instant>; 7],
    last_preview: Option<Instant>,
    last_move: Option<Instant>,
    move_step: usize,
}

static PLAYER: Mutex<Option<Player>> = Mutex::new(None);

impl Player {
    fn new(initial: &Settings) -> Self {
        let (tx, rx) = channel();
        let initial_style = initial.style.clone();
        let initial_device = initial.output_device.clone();
        if let Err(e) = std::thread::Builder::new().name("console-sounds".into()).spawn(move || work(rx, initial_style, initial_device)) {
            log::warn!("console sounds: could not start audio worker: {e}");
        }
        Self { tx, last: [None; 7], last_preview: None, last_move: None, move_step: 0 }
    }

    fn send(&mut self, sound: Sound, settings: Settings, preview: bool, replaces_press: bool) {
        let now = Instant::now();
        let last = if preview { &mut self.last_preview } else { &mut self.last[sound as usize] };
        if last.is_some_and(|t| now.duration_since(t) < sound.gap()) {
            return;
        }
        *last = Some(now);
        let pitch = pitch_for(sound, settings.dynamic_pitch, preview, &mut self.last_move, &mut self.move_step, now);
        if self.tx.send(Request { sound, settings, pitch, replaces_press }).is_err() {
            log::warn!("console sounds: audio worker has stopped");
        }
    }
}

fn player(settings: &Settings) {
    let mut player = PLAYER.lock().unwrap_or_else(|e| e.into_inner());
    player.get_or_insert_with(|| Player::new(settings));
}

/// Starts the worker with only the currently selected package resident.
pub fn initialize(settings: Settings) {
    player(&settings);
}

fn send(sound: Sound, settings: Settings, preview: bool, replaces_press: bool) {
    let mut player = PLAYER.lock().unwrap_or_else(|e| e.into_inner());
    player.get_or_insert_with(|| Player::new(&settings)).send(sound, settings, preview, replaces_press);
}

/// Plays `sound` for a controller press.
pub fn play(sound: Sound, settings: Settings) {
    send(sound, settings, false, false);
}

/// Plays the game-launch sound and replaces the immediately preceding confirm press.
pub fn launch(settings: Settings) {
    send(Sound::Launch, settings, false, true);
}

/// Plays the sound for a toast/banner.
pub fn notification(settings: Settings) {
    send(Sound::Notification, settings, false, false);
}

/// Plays the sound when Heronite's window is restored after a game.
pub fn restore(settings: Settings) {
    send(Sound::Restore, settings, false, false);
}

/// Plays `sound` as `settings` make it, for the user to hear a change.
pub fn preview(sound: Sound, settings: Settings) {
    send(sound, settings, true, true);
}

fn work(rx: Receiver<Request>, initial_style: String, initial_output_device: String) {
    let catalog = catalog();
    let style = resolve_style(&initial_style);
    let sounds = load_style(catalog.root.as_deref(), &style);
    play_requests(rx, initial_output_device, &catalog, style, sounds);
}

fn play_requests(
    rx: Receiver<Request>,
    initial_output_device: String,
    catalog: &StyleCatalog,
    mut style: String,
    mut sounds: HashMap<Sound, AudioSample>,
) {
    let mut running: Vec<(Sink, Instant)> = Vec::new();
    let mut selected_device = initial_output_device;
    let mut output: Option<AudioOutput> = None;
    let mut last_attempt = Instant::now() - OUTPUT_RETRY;
    let mut next_device_poll = Instant::now();
    let mut default_device = String::new();
    loop {
        let request = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => Some(request),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        running.retain(|(sink, _)| !sink.empty());
        let mut requested_device_change = false;
        if let Some(request) = &request {
            requested_device_change = request.settings.output_device != selected_device;
            selected_device.clone_from(&request.settings.output_device);
        }
        if Instant::now() >= next_device_poll {
            default_device = default_output_name();
            next_device_poll = Instant::now() + DEVICE_POLL;
        }
        let default_changed = output.as_ref().is_some_and(|current| current.default_device != default_device);
        let reconnect = output.as_ref().map_or(true, |current| {
            current.selected_device != selected_device
                || current.default_device != default_device
                || current.connected_at.elapsed() >= OUTPUT_REFRESH
        });
        if reconnect && (requested_device_change || default_changed || last_attempt.elapsed() >= OUTPUT_RETRY) {
            last_attempt = Instant::now();
            for (sink, _) in &running {
                sink.stop();
            }
            running.clear();
            match open_output(&selected_device) {
                Ok(next) => output = Some(next),
                Err(e) => {
                    log::warn!("console sounds: cannot open audio output: {e}; will retry");
                    output = None;
                }
            }
        }
        let Some(Request { sound, settings, pitch, replaces_press }) = request else { continue };
        let next_style = resolve_style(&settings.style);
        if next_style != style {
            sounds = load_style(catalog.root.as_deref(), &next_style);
            style = next_style;
        }
        if replaces_press {
            for (sink, started) in &running {
                if started.elapsed() < PRESS_WINDOW {
                    sink.stop();
                }
            }
        }
        if running.len() >= 4 {
            continue;
        }
        let Some(sample) = sample_for(&sounds, sound) else { continue };
        let Some(output) = output.as_ref() else { continue };
        let sink = match Sink::try_new(&output.handle) {
            Ok(sink) => sink,
            Err(e) => {
                log::warn!("console sounds: cannot create playback sink: {e}");
                continue;
            }
        };
        sink.set_volume(settings.gain());
        sink.append(SamplesBuffer::new(sample.channels, sample.sample_rate, sample.samples.clone()).speed(pitch));
        running.push((sink, Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classic() -> Voice {
        Voice::of(SoundStyle::Classic)
    }

    fn peak(bytes: &[u8]) -> u16 {
        bytes[44..].chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]]).unsigned_abs()).max().unwrap()
    }

    fn make(style: SoundStyle, sound: Sound) -> Vec<u8> {
        let (tones, level) = sound.tones(style);
        wav(tones, level, &Voice::of(style))
    }

    #[test]
    fn wav_has_a_valid_header_and_the_right_length() {
        let bytes = wav(&[(440., 100.)], 0.5, &classic());
        let samples = (RATE as f32 * 0.1) as usize;
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(bytes.len(), 44 + samples * 2);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize, bytes.len() - 8);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize, samples * 2);
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), RATE);
    }

    #[test]
    fn every_sound_fades_out_and_stays_below_its_level() {
        for style in SoundStyle::ALL {
            for sound in Sound::ALL {
                let (tones, level) = sound.tones(*style);
                let bytes = make(*style, sound);
                let n = (bytes.len() - 44) / 2;
                let last = i16::from_le_bytes([bytes[44 + (n - 1) * 2], bytes[45 + (n - 1) * 2]]);
                let peak = f32::from(peak(&bytes));
                assert!(peak > 0.5 * level * f32::from(i16::MAX), "{style:?} {sound:?} is too quiet: {peak}");
                assert!(peak <= level * f32::from(i16::MAX), "{style:?} {sound:?} is above its level: {peak}");
                assert!(last.unsigned_abs() < 50, "{style:?} {sound:?} ends with a click");
                let first = i16::from_le_bytes([bytes[44], bytes[45]]);
                assert!(first.unsigned_abs() < 50, "{style:?} {sound:?} starts with a click");
                assert!(!tones.is_empty());
            }
        }
    }

    #[test]
    fn decoded_builtin_wavs_are_preloaded_as_samples() {
        let sample = AudioSample::from_audio(make(SoundStyle::Soft, Sound::Confirm)).unwrap();
        assert_eq!(sample.channels, 1);
        assert_eq!(sample.sample_rate, RATE);
        assert!(!sample.samples.is_empty());
    }

    #[test]
    fn external_wav_replaces_builtin_and_missing_file_uses_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("move.wav");
        let external = wav(&[(880., 100.)], 0.8, &classic());
        std::fs::write(&path, &external).unwrap();
        let loaded = load_or_builtin(Some(path.as_path()), built_in(SoundStyle::Classic, Sound::Move));
        let external_sample = AudioSample::from_audio(external).unwrap();
        assert_eq!(loaded.samples, external_sample.samples);

        let fallback = built_in(SoundStyle::Classic, Sound::Move);
        let missing = dir.path().join("missing.wav");
        let loaded = load_or_builtin(Some(missing.as_path()), built_in(SoundStyle::Classic, Sound::Move));
        assert_eq!(loaded.samples, fallback.samples);

        std::fs::write(&path, b"not a wav file").unwrap();
        let loaded = load_or_builtin(Some(path.as_path()), built_in(SoundStyle::Classic, Sound::Move));
        assert_eq!(loaded.samples, fallback.samples);
    }

    #[test]
    fn style_discovery_keeps_builtins_and_sorts_valid_external_folders() {
        let root = tempfile::tempdir().unwrap();
        let valid = wav(&[(880., 100.)], 0.8, &classic());
        for style in ["Switch 2", "PS5"] {
            let dir = root.path().join(style);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("move.wav"), &valid).unwrap();
        }
        let broken = root.path().join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("move.wav"), &valid).unwrap();
        std::fs::write(broken.join("back.wav"), b"truncated").unwrap();

        let ids: Vec<_> = discover_styles(root.path()).into_iter().map(|style| style.id).collect();
        assert_eq!(ids, ["soft", "classic", "PS5", "Switch 2"]);
    }

    #[test]
    fn notification_file_is_loaded_and_notification_and_restore_fall_back_to_confirm() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("custom");
        std::fs::create_dir_all(&dir).unwrap();
        let confirm = wav(&[(880., 100.)], 0.8, &classic());
        let notification = wav(&[(440., 120.)], 0.6, &classic());
        std::fs::write(dir.join("confirm.wav"), &confirm).unwrap();
        std::fs::write(dir.join("notification.wav"), &notification).unwrap();
        let sounds = load_style(Some(root.path()), "custom");
        let notification_sample = AudioSample::from_audio(notification).unwrap();
        assert_eq!(sample_for(&sounds, Sound::Notification).unwrap().samples, notification_sample.samples);
        for sound in [Sound::Launch, Sound::Restore] {
            assert_eq!(sample_for(&sounds, sound).unwrap().samples, sample_for(&sounds, Sound::Confirm).unwrap().samples);
        }
    }

    #[test]
    fn a_missing_or_invalid_custom_pack_falls_back_to_classic() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("custom");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("move.wav"), b"not wav").unwrap();
        let sounds = load_style(Some(root.path()), "custom");
        assert_eq!(sounds[&Sound::Move].samples, built_in(SoundStyle::Classic, Sound::Move).samples);
        let missing = load_style(Some(root.path()), "deleted");
        assert_eq!(missing[&Sound::Move].samples, built_in(SoundStyle::Classic, Sound::Move).samples);
    }

    #[test]
    fn sound_folder_readme_is_created_without_overwriting_user_content() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sounds");
        prepare_sound_folder(&root);
        let readme = root.join("README.txt");
        let text = std::fs::read_to_string(&readme).unwrap();
        for file in ["move", "confirm", "back", "tab", "launch", "notification", "restore"] {
            assert!(text.contains(file));
        }
        for extension in [".wav", ".ogg", ".mp3", ".flac"] {
            assert!(text.contains(extension));
        }
        assert!(text.contains("use confirm if their file is absent"));
        std::fs::write(&readme, LEGACY_SOUND_README).unwrap();
        prepare_sound_folder(&root);
        assert!(std::fs::read_to_string(&readme).unwrap().contains("notification"));
        assert!(text.contains("5 seconds"));
        std::fs::write(&readme, "user notes").unwrap();
        prepare_sound_folder(&root);
        assert_eq!(std::fs::read_to_string(readme).unwrap(), "user notes");
    }

    #[test]
    fn rapid_moves_step_up_a_pentatonic_scale_and_reset_after_a_pause() {
        let start = Instant::now();
        let mut last = None;
        let mut step = 0;
        let pitches: Vec<_> = (0..5).map(|i| move_pitch(&mut last, &mut step, start + Duration::from_millis(i * 80))).collect();
        assert!(pitches.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(move_pitch(&mut last, &mut step, start + Duration::from_secs(1)), 1.);
    }

    #[test]
    fn dynamic_pitch_only_changes_fast_navigation_when_enabled() {
        let start = Instant::now();
        let mut last = None;
        let mut step = 0;
        let first = pitch_for(Sound::Move, true, false, &mut last, &mut step, start);
        let second = pitch_for(Sound::Move, true, false, &mut last, &mut step, start + Duration::from_millis(80));
        assert_eq!(first, 1.);
        assert!(second > 1.);

        assert_eq!(pitch_for(Sound::Move, false, false, &mut last, &mut step, start + Duration::from_millis(120)), 1.);
        for sound in [Sound::Confirm, Sound::Back, Sound::Tab, Sound::Launch, Sound::Notification, Sound::Restore] {
            assert_eq!(pitch_for(sound, true, false, &mut last, &mut step, start + Duration::from_millis(150)), 1.);
        }
        assert_eq!(pitch_for(Sound::Move, true, true, &mut last, &mut step, start + Duration::from_millis(180)), 1.);
    }

    #[test]
    fn settings_clamp_volume_and_scale_gain() {
        let mut controls = ControlOptions::default();
        assert_eq!(
            Settings::of(&controls, String::new()),
            Settings { style: "soft".into(), volume: 100, dynamic_pitch: true, output_device: String::new() }
        );
        controls.sound_volume = 0;
        assert_eq!(Settings::of(&controls, String::new()).volume, SOUND_VOLUME.0);
        controls.sound_volume = 250;
        assert_eq!(Settings::of(&controls, String::new()).volume, SOUND_VOLUME.1);
        assert!(
            Settings { style: "soft".into(), volume: 50, dynamic_pitch: true, output_device: String::new() }.gain()
                < Settings { style: "soft".into(), volume: 100, dynamic_pitch: true, output_device: String::new() }.gain()
        );
    }

    #[test]
    fn presses_map_to_sounds() {
        assert_eq!(for_pad(Pad::Down), Some(Sound::Move));
        assert_eq!(for_pad(Pad::A), Some(Sound::Confirm));
        assert_eq!(for_pad(Pad::B), Some(Sound::Back));
        assert_eq!(for_pad(Pad::RB), Some(Sound::Tab));
        assert_eq!(for_pad(Pad::ScrollDown), None);
        assert_eq!(Sound::Launch.name(), "launch");
    }
}
