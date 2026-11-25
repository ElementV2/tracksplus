//! Import de fichiers audio sur la timeline du multipiste (glisser-déposer) : décodage
//! (WAV, AIFF, MP3, FLAC, Ogg, M4A…), conversion à la fréquence de la session, écriture
//! d'un WAV 32 bits flottant dans le dossier « Audio » de la session. L'original n'est
//! jamais modifié. Tout se fait par blocs : un morceau de plusieurs heures n'est jamais
//! chargé en entier en mémoire.

use anyhow::{Context, Result, anyhow, bail};
use rubato::{FftFixedIn, Resampler};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;

const CHUNK: usize = 8192;

/// Un fichier importé, prêt à être posé sur une nouvelle piste.
pub struct Imported {
    /// Nom de la piste (nom du fichier d'origine, sans extension).
    pub name: String,
    /// Fichier écrit dans le dossier « Audio » de la session.
    pub file: String,
    pub frames: u64,
    pub stereo: bool,
}

/// Import annulé par l'utilisateur.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Import annulé")
    }
}

impl std::error::Error for Cancelled {}

/// Résultat par fichier : (fichier d'origine, piste prête ou raison de l'échec).
pub type ImportResults = Vec<(PathBuf, Result<Imported>)>;

/// Import en arrière-plan de plusieurs fichiers.
pub struct ImportJob {
    /// Avancement global en millièmes.
    pub progress: Arc<AtomicU32>,
    pub cancel: Arc<AtomicBool>,
    /// Position de la timeline où poser les prises.
    pub at: u64,
    handle: Option<JoinHandle<ImportResults>>,
}

impl ImportJob {
    /// `targets` : (fichier d'origine, nom du fichier à écrire dans `audio_dir`).
    pub fn start(
        targets: Vec<(PathBuf, String)>,
        audio_dir: PathBuf,
        sample_rate: u32,
        at: u64,
    ) -> Result<Self> {
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (p2, c2) = (progress.clone(), cancel.clone());
        let handle = std::thread::Builder::new()
            .name("tracksplus-import".into())
            .spawn(move || {
                let n = targets.len().max(1) as f64;
                let mut out = Vec::new();
                for (k, (src, file)) in targets.into_iter().enumerate() {
                    if c2.load(Ordering::Relaxed) {
                        out.push((src, Err(Cancelled.into())));
                        continue;
                    }
                    let tick = |f: f64| {
                        p2.store(
                            ((k as f64 + f.clamp(0.0, 1.0)) / n * 1000.0) as u32,
                            Ordering::Relaxed,
                        );
                        !c2.load(Ordering::Relaxed)
                    };
                    // Un fichier qui fait planter le décodeur ne fait pas planter l'import.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        import_file(&src, &audio_dir, &file, sample_rate, &tick)
                    }))
                    .unwrap_or_else(|_| Err(anyhow!("fichier illisible (erreur du décodeur)")));
                    out.push((src, r));
                }
                // Annulé : les fichiers déjà convertis ne servent à rien, on les retire.
                if c2.load(Ordering::Relaxed) {
                    for (_, r) in &mut out {
                        if let Ok(i) = r {
                            let _ = std::fs::remove_file(audio_dir.join(&i.file));
                        }
                        *r = Err(Cancelled.into());
                    }
                }
                p2.store(1000, Ordering::Relaxed);
                out
            })
            .context("Impossible de démarrer l'import")?;
        Ok(Self {
            progress,
            cancel,
            at,
            handle: Some(handle),
        })
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    pub fn join(mut self) -> ImportResults {
        match self.handle.take().map(|h| h.join()) {
            Some(Ok(r)) => r,
            _ => Vec::new(),
        }
    }
}

/// Fichier en cours d'écriture : supprimé s'il n'a pas été validé (erreur, annulation).
struct Part(Option<PathBuf>);

impl Drop for Part {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Décode `src`, le convertit à `sr` et l'écrit dans `audio_dir/file` (WAV 32 bits
/// flottant, mono ou stéréo comme l'original ; au-delà de 2 canaux, les deux premiers).
/// `on_progress(f)` renvoie `false` pour annuler.
pub fn import_file(
    src: &Path,
    audio_dir: &Path,
    file: &str,
    sr: u32,
    on_progress: &dyn Fn(f64) -> bool,
) -> Result<Imported> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
    use symphonia::core::errors::Error as SymError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let f = std::fs::File::open(src).context("ouverture impossible")?;
    let mss = MediaSourceStream::new(Box::new(f), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = src.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| anyhow!("format non reconnu ({e})"))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("aucune piste audio dans le fichier"))?;
    let track_id = track.id;
    let src_sr = track
        .codec_params
        .sample_rate
        .ok_or_else(|| anyhow!("fréquence d'échantillonnage inconnue"))?;
    let channels = track
        .codec_params
        .channels
        .map_or(2, |c| c.count())
        .clamp(1, 2);
    let total = track.codec_params.n_frames;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| anyhow!("codec non pris en charge ({e})"))?;

    std::fs::create_dir_all(audio_dir).context("création du dossier Audio")?;
    let dst = audio_dir.join(file);
    let part_path = audio_dir.join(format!("{file}.part"));
    let mut part = Part(Some(part_path.clone()));
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate: sr,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer =
        hound::WavWriter::create(&part_path, spec).context("écriture dans le dossier Audio")?;
    let mut conv = Converter::new(channels, src_sr, sr)?;
    let mut buf: Option<SampleBuffer<f32>> = None;
    let mut read = 0u64;
    let mut written = 0u64;
    let mut write = |block: &[f32]| -> Result<()> {
        for &x in block {
            writer
                .write_sample(if x.is_finite() { x } else { 0.0 })
                .map_err(write_err)?;
        }
        written += (block.len() / channels) as u64;
        Ok(())
    };
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => bail!("lecture impossible ({e})"),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // Paquet abîmé isolé (fréquent en MP3) : on continue.
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => bail!("décodage impossible ({e})"),
        };
        let dspec = *decoded.spec();
        let ch = dspec.channels.count().max(1);
        let b = buf.get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, dspec));
        if b.capacity() < decoded.capacity() * ch {
            *b = SampleBuffer::new(decoded.capacity() as u64, dspec);
        }
        b.copy_interleaved_ref(decoded);
        let frames = b.samples().len() / ch;
        read += frames as u64;
        conv.push(b.samples(), ch, &mut write)?;
        let f = total.map_or(0.0, |t| read as f64 / t.max(1) as f64);
        if !on_progress(f) {
            return Err(Cancelled.into());
        }
    }
    conv.finish(&mut write)?;
    if written == 0 {
        bail!("le fichier ne contient pas de son");
    }
    writer.finalize().map_err(write_err)?;
    // Écriture forcée sur le disque (Windows : le fichier doit être ouvert en écriture).
    std::fs::OpenOptions::new()
        .write(true)
        .open(&part_path)
        .and_then(|f| f.sync_all())
        .context("écriture sur le disque")?;
    // Jamais par-dessus un fichier existant (autre prise du même nom) : il serait perdu.
    match crate::project::rename_no_replace(&part_path, &dst) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("un fichier « {file} » existe déjà dans le dossier Audio")
        }
        r => r.context("écriture dans le dossier Audio")?,
    }
    part.0 = None;
    let name = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Import".into());
    Ok(Imported {
        name,
        file: file.to_string(),
        frames: written,
        stereo: channels == 2,
    })
}

fn write_err(e: hound::Error) -> anyhow::Error {
    match e {
        hound::Error::IoError(io) if io.kind() == std::io::ErrorKind::StorageFull => {
            anyhow!("disque plein : libérez de la place puis recommencez")
        }
        e => anyhow!("écriture dans le dossier Audio ({e})"),
    }
}

/// Conversion par blocs : canaux (1 ou 2) puis fréquence, avec la longueur exacte.
struct Converter {
    channels: usize,
    rs: Option<FftFixedIn<f32>>,
    /// Échantillons en attente d'un bloc complet, par canal.
    pending: Vec<Vec<f32>>,
    out: Vec<Vec<f32>>,
    skip: usize,
    read: u64,
    written: u64,
    ratio: (u64, u64),
    inter: Vec<f32>,
}

impl Converter {
    fn new(channels: usize, from: u32, to: u32) -> Result<Self> {
        let rs = if from != to {
            Some(FftFixedIn::<f32>::new(
                from as usize,
                to as usize,
                CHUNK,
                2,
                channels,
            )?)
        } else {
            None
        };
        Ok(Self {
            channels,
            skip: rs.as_ref().map_or(0, |r| r.output_delay()),
            out: rs
                .as_ref()
                .map_or_else(Vec::new, |r| r.output_buffer_allocate(true)),
            rs,
            pending: vec![Vec::with_capacity(CHUNK * 2); channels],
            read: 0,
            written: 0,
            ratio: (to as u64, from as u64),
            inter: Vec::new(),
        })
    }

    /// Ajoute des échantillons entrelacés sur `ch` canaux.
    fn push(
        &mut self,
        samples: &[f32],
        ch: usize,
        sink: &mut impl FnMut(&[f32]) -> Result<()>,
    ) -> Result<()> {
        for frame in samples.chunks_exact(ch) {
            for (j, p) in self.pending.iter_mut().enumerate() {
                p.push(frame[j.min(ch - 1)]);
            }
        }
        self.read += (samples.len() / ch) as u64;
        if self.rs.is_none() {
            return self.drain_direct(sink);
        }
        while self.pending[0].len() >= CHUNK {
            self.process(CHUNK, false, sink)?;
        }
        Ok(())
    }

    fn drain_direct(&mut self, sink: &mut impl FnMut(&[f32]) -> Result<()>) -> Result<()> {
        let n = self.pending[0].len();
        self.inter.clear();
        for f in 0..n {
            for p in &self.pending {
                self.inter.push(p[f]);
            }
        }
        for p in &mut self.pending {
            p.clear();
        }
        self.written += n as u64;
        sink(&self.inter)
    }

    fn process(
        &mut self,
        n: usize,
        partial: bool,
        sink: &mut impl FnMut(&[f32]) -> Result<()>,
    ) -> Result<()> {
        let Some(rs) = &mut self.rs else {
            return Ok(());
        };
        let input: Vec<&[f32]> = self.pending.iter().map(|p| &p[..n]).collect();
        let (_, got) = if partial {
            rs.process_partial_into_buffer(Some(&input), &mut self.out, None)?
        } else {
            rs.process_into_buffer(&input, &mut self.out, None)?
        };
        for p in &mut self.pending {
            p.drain(..n);
        }
        self.emit(got, sink)
    }

    fn emit(&mut self, got: usize, sink: &mut impl FnMut(&[f32]) -> Result<()>) -> Result<()> {
        let expected =
            (self.read as u128 * self.ratio.0 as u128).div_ceil(self.ratio.1 as u128) as u64;
        let start = self.skip.min(got);
        self.skip -= start;
        let take = ((got - start) as u64).min(expected.saturating_sub(self.written)) as usize;
        if take == 0 {
            return Ok(());
        }
        self.inter.clear();
        for f in start..start + take {
            for c in &self.out[..self.channels] {
                self.inter.push(c[f]);
            }
        }
        self.written += take as u64;
        sink(&self.inter)
    }

    /// Fin du fichier : dernier bloc et latence du filtre.
    fn finish(&mut self, sink: &mut impl FnMut(&[f32]) -> Result<()>) -> Result<()> {
        if self.rs.is_none() {
            return self.drain_direct(sink);
        }
        let n = self.pending[0].len();
        if n > 0 {
            self.process(n, true, sink)?;
        }
        let expected =
            (self.read as u128 * self.ratio.0 as u128).div_ceil(self.ratio.1 as u128) as u64;
        let mut guard = 0;
        while self.written < expected && guard < 64 {
            let Some(rs) = &mut self.rs else { break };
            let (_, got) =
                rs.process_partial_into_buffer::<&[f32], _>(None, &mut self.out, None)?;
            self.emit(got, sink)?;
            guard += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tp-import-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_wav(path: &Path, sr: u32, ch: u16, frames: usize) {
        let spec = hound::WavSpec {
            channels: ch,
            sample_rate: sr,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..frames {
            let v = (0.5 * (i as f32 * 0.05).sin() * 32767.0) as i16;
            for _ in 0..ch {
                w.write_sample(v).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    #[test]
    fn imports_and_converts() {
        let d = tmp("conv");
        let audio = d.join("Audio");
        // Stéréo 44,1 kHz → 48 kHz : longueur exacte, stéréo conservée.
        write_wav(&d.join("chanson.wav"), 44100, 2, 44100 * 3 + 17);
        let i = import_file(
            &d.join("chanson.wav"),
            &audio,
            "chanson_001.wav",
            48000,
            &|_| true,
        )
        .unwrap();
        assert!(i.stereo);
        assert_eq!(i.name, "chanson");
        let want = ((44100u64 * 3 + 17) * 48000).div_ceil(44100);
        assert_eq!(i.frames, want);
        let r = hound::WavReader::open(audio.join("chanson_001.wav")).unwrap();
        assert_eq!(r.spec().sample_rate, 48000);
        assert_eq!(r.spec().channels, 2);
        assert_eq!(r.duration() as u64, want);
        // Mono à la même fréquence : copie exacte de la longueur.
        write_wav(&d.join("voix.wav"), 48000, 1, 12345);
        let i = import_file(&d.join("voix.wav"), &audio, "voix_001.wav", 48000, &|_| {
            true
        })
        .unwrap();
        assert!(!i.stereo);
        assert_eq!(i.frames, 12345);
        // Aucun fichier temporaire laissé.
        let parts = std::fs::read_dir(&audio)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
            .count();
        assert_eq!(parts, 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_file_and_cancel_leave_nothing() {
        let d = tmp("bad");
        let audio = d.join("Audio");
        std::fs::write(d.join("faux.mp3"), b"pas du son").unwrap();
        let e = import_file(&d.join("faux.mp3"), &audio, "faux_001.wav", 48000, &|_| {
            true
        })
        .err()
        .unwrap();
        assert!(e.to_string().contains("format"), "{e:#}");
        write_wav(&d.join("long.wav"), 48000, 1, 48000 * 5);
        let e = import_file(&d.join("long.wav"), &audio, "long_001.wav", 48000, &|_| {
            false
        })
        .err()
        .unwrap();
        assert!(e.is::<Cancelled>());
        let left = std::fs::read_dir(&audio).map(|r| r.count()).unwrap_or(0);
        assert_eq!(left, 0);
        let _ = std::fs::remove_dir_all(&d);
    }
}
