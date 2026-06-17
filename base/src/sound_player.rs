use anyhow::{ensure, Context};
use camino::Utf8Path;
use reaper_high::Reaper;
use reaper_low::raw;
use reaper_medium::{
    FlexibleOwnedPcmSource, Handle, MeasureAlignment, MidiImportBehavior, OwnedPreviewRegister,
    PositionInSeconds, ReaperMutex, ReaperMutexGuard, ReaperVolumeValue,
};
use std::cell::Cell;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct SoundPlayer {
    preview_register: Arc<ReaperMutex<OwnedPreviewRegister>>,
    play_handle: Cell<Option<Handle<raw::preview_register_t>>>,
    /// Length of the currently loaded source in seconds, captured at load time (0.0 if
    /// unknown). Used to clamp seeks and to drive the transport/waveform position readout.
    length_secs: Cell<f64>,
}

unsafe impl Send for SoundPlayer {}

impl Default for SoundPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl SoundPlayer {
    pub fn new() -> Self {
        let mut register = OwnedPreviewRegister::new();
        register.set_volume(ReaperVolumeValue::ZERO_DB);
        let preview_register = Arc::new(ReaperMutex::new(register));
        Self {
            preview_register,
            play_handle: Cell::new(None),
            length_secs: Cell::new(0.0),
        }
    }

    pub fn load_file(&mut self, path_to_file: &Utf8Path) -> anyhow::Result<()> {
        ensure!(path_to_file.exists(), "sound file doesn't exist");
        let source = Reaper::get()
            .medium_reaper()
            .pcm_source_create_from_file_ex(path_to_file, MidiImportBehavior::UsePreference)?;
        // Capture the length now, while we still hold the typed source, for the transport.
        self.length_secs
            .set(source.get_length().map(|d| d.get()).unwrap_or(0.0));
        self.load_pcm_source(FlexibleOwnedPcmSource::Reaper(source))
    }

    pub fn load_pcm_source(&mut self, source: FlexibleOwnedPcmSource) -> anyhow::Result<()> {
        let mut preview_register = self.lock_preview_register()?;
        preview_register.set_src(Some(source));
        Ok(())
    }

    pub fn volume(&self) -> anyhow::Result<ReaperVolumeValue> {
        let preview_register = self.lock_preview_register()?;
        Ok(preview_register.volume())
    }

    pub fn set_volume(&self, volume: ReaperVolumeValue) -> anyhow::Result<()> {
        let mut preview_register = self.lock_preview_register()?;
        preview_register.set_volume(volume);
        Ok(())
    }

    pub fn play(&self) -> anyhow::Result<()> {
        if self.play_handle.get().is_some() {
            // Is playing already. Simply rewind.
            let mut preview_register = self.lock_preview_register()?;
            preview_register.set_cur_pos(PositionInSeconds::ZERO);
        } else {
            // Is not yet playing. Start playing.
            let handle = Reaper::get().medium_session().play_preview_ex(
                self.preview_register.clone(),
                Default::default(),
                MeasureAlignment::PlayImmediately,
            )?;
            self.play_handle.set(Some(handle));
        }
        Ok(())
    }

    pub fn stop(&self) -> anyhow::Result<()> {
        let play_handle = self.play_handle.take().context("not playing")?;
        Reaper::get().medium_session().stop_preview(play_handle)?;
        self.lock_preview_register()?
            .set_cur_pos(PositionInSeconds::ZERO);
        Ok(())
    }

    /// Whether a preview is currently playing (note: this reflects our play handle, so it
    /// stays `true` after playback naturally reaches the end of a non-looped source until
    /// [`Self::stop`] or [`Self::pause`] is called).
    pub fn is_playing(&self) -> bool {
        self.play_handle.get().is_some()
    }

    /// Pause playback, keeping the current position so [`Self::resume`] continues from there.
    /// Unlike [`Self::stop`], this does not rewind.
    pub fn pause(&self) -> anyhow::Result<()> {
        if let Some(handle) = self.play_handle.take() {
            Reaper::get().medium_session().stop_preview(handle)?;
        }
        Ok(())
    }

    /// Resume playback from the current position (no-op if already playing).
    pub fn resume(&self) -> anyhow::Result<()> {
        if self.play_handle.get().is_none() {
            let handle = Reaper::get().medium_session().play_preview_ex(
                self.preview_register.clone(),
                Default::default(),
                MeasureAlignment::PlayImmediately,
            )?;
            self.play_handle.set(Some(handle));
        }
        Ok(())
    }

    /// Current playback position in seconds.
    pub fn position(&self) -> anyhow::Result<f64> {
        Ok(self.lock_preview_register()?.cur_pos().get())
    }

    /// Length of the loaded source in seconds (0.0 if unknown).
    pub fn length(&self) -> f64 {
        self.length_secs.get()
    }

    /// Seek to the given position in seconds, clamped to `[0, length]`.
    pub fn seek(&self, pos_secs: f64) -> anyhow::Result<()> {
        let len = self.length_secs.get();
        let clamped = if len > 0.0 {
            pos_secs.clamp(0.0, len)
        } else {
            pos_secs.max(0.0)
        };
        self.lock_preview_register()?
            .set_cur_pos(PositionInSeconds::new_panic(clamped));
        Ok(())
    }

    pub fn is_looped(&self) -> anyhow::Result<bool> {
        Ok(self.lock_preview_register()?.is_looped())
    }

    pub fn set_looped(&self, looped: bool) -> anyhow::Result<()> {
        self.lock_preview_register()?.set_looped(looped);
        Ok(())
    }

    fn lock_preview_register(&self) -> anyhow::Result<ReaperMutexGuard<OwnedPreviewRegister>> {
        self.preview_register
            .lock()
            .context("couldn't acquire preview register lock in sound player")
    }
}
