//! Import de fichiers audio sur la timeline du multipiste (glisser-déposer) : décodage
//! (WAV, AIFF, MP3, FLAC, Ogg, M4A…), conversion à la fréquence de la session, écriture
//! d'un WAV 32 bits flottant dans le dossier « Audio » de la session. L'original n'est
//! jamais modifié. Tout se fait par blocs : un morceau de plusieurs heures n'est jamais
//! chargé en entier en mémoire. Comme une prise, un import long est découpé en fichiers
//! de 2 Go au plus (`nom.wav`, `nom_p2.wav`…) : un WAV ne peut pas dépasser 4 Gio.
//!
//! Dans l'application, chaque fichier est décodé dans un processus fils (voir `isolate`) :
//! un fichier piégé n'arrête que lui.

use crate::decode::Converter;
use crate::recorder::{MAX_SEGMENT_BYTES, segment_name};
use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;

/// Un fichier importé, prêt à être posé sur une nouvelle piste.
pub struct Imported {
    /// Nom de la piste (nom du fichier d'origine, sans extension).
    pub name: String,
    /// Fichiers écrits dans le dossier « Audio » de la session, dans l'ordre, avec leur
    /// longueur (trames) : un par tranche de 2 Go, posés bout à bout sur la piste.
    pub segments: Vec<(String, u64)>,
    pub stereo: bool,
}

impl Imported {
    /// Longueur totale (trames).
    #[cfg(test)]
    pub fn frames(&self) -> u64 {
        self.segments.iter().map(|(_, n)| n).sum()
    }
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
                    // Dans un processus fils quand c'est possible : un fichier piégé n'arrête
                    // que lui. Sinon ici, et un fichier qui fait planter le décodeur ne fait
                    // pas planter l'import.
                    let r =
                        crate::isolate::import(&src, &audio_dir, &file, sample_rate, &tick, &c2)
                            .unwrap_or_else(|| {
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    import_file(
                                        &src,
                                        &audio_dir,
                                        &file,
                                        sample_rate,
                                        &tick,
                                        &|_| {},
                                    )
                                }))
                                .unwrap_or_else(|_| Err(anyhow!("fichier illisible")))
                            });
                    out.push((src, r));
                }
                // Annulé : les fichiers déjà convertis ne servent à rien, on les retire.
                if c2.load(Ordering::Relaxed) {
                    for (_, r) in &mut out {
                        if let Ok(i) = r {
                            for (f, _) in &i.segments {
                                let _ = std::fs::remove_file(audio_dir.join(f));
                            }
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

/// Retire les fichiers d'un import interrompu : segments terminés et fichiers « .part »
/// (celui en cours d'écriture compris).
pub fn remove_segments(audio_dir: &Path, file: &str, done: &[(String, u64)]) {
    for (f, _) in done {
        let _ = std::fs::remove_file(audio_dir.join(f));
    }
    for k in 1..=done.len() + 1 {
        let _ = std::fs::remove_file(audio_dir.join(format!("{}.part", segment_name(file, k))));
    }
}

/// Étape d'un fichier de l'import (voir `import_file`).
pub enum SegmentEvent<'a> {
    /// Nom définitif encore libre : le fichier terminé va y être renommé.
    Naming(&'a str),
    /// Fichier terminé sous son nom définitif (nom, trames).
    Done(&'a str, u64),
}

/// Écriture découpée en fichiers de `max_bytes` au plus. Tout fichier non validé
/// (erreur, annulation) est retiré, segments déjà terminés compris.
struct Segments<'a> {
    dir: &'a Path,
    base: &'a str,
    spec: hound::WavSpec,
    channels: usize,
    /// Trames par fichier au plus.
    max_frames: u64,
    writer: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>,
    frames: u64,
    done: Vec<(String, u64)>,
    on_segment: &'a dyn Fn(SegmentEvent<'_>),
    committed: bool,
}

impl<'a> Segments<'a> {
    fn new(
        dir: &'a Path,
        base: &'a str,
        spec: hound::WavSpec,
        max_bytes: u64,
        on_segment: &'a dyn Fn(SegmentEvent<'_>),
    ) -> Self {
        let channels = spec.channels as usize;
        // En-tête WAV (44 octets) compris ; 4 octets par échantillon.
        let max_frames = (max_bytes.saturating_sub(44) / (4 * channels as u64)).max(1);
        Self {
            dir,
            base,
            spec,
            channels,
            max_frames,
            writer: None,
            frames: 0,
            done: Vec::new(),
            on_segment,
            committed: false,
        }
    }

    fn name(&self) -> String {
        segment_name(self.base, self.done.len() + 1)
    }

    fn part(&self) -> PathBuf {
        self.dir.join(format!("{}.part", self.name()))
    }

    fn write(&mut self, mut block: &[f32]) -> Result<()> {
        while !block.is_empty() {
            if self.writer.is_none() {
                let part = self.part();
                let _ = std::fs::remove_file(&part);
                self.writer = Some(
                    hound::WavWriter::create(&part, self.spec)
                        .context("écriture dans le dossier Audio")?,
                );
                self.frames = 0;
            }
            let room = (self.max_frames - self.frames) as usize;
            let n = (block.len() / self.channels).min(room);
            let w = self.writer.as_mut().unwrap();
            for &x in &block[..n * self.channels] {
                w.write_sample(if x.is_finite() { x } else { 0.0 })
                    .map_err(write_err)?;
            }
            self.frames += n as u64;
            block = &block[n * self.channels..];
            if self.frames >= self.max_frames {
                self.close()?;
            }
        }
        Ok(())
    }

    /// Termine le fichier en cours : en-tête, écriture forcée sur le disque, nom définitif.
    fn close(&mut self) -> Result<()> {
        let Some(w) = self.writer.take() else {
            return Ok(());
        };
        let part = self.part();
        let name = self.name();
        let dst = self.dir.join(&name);
        w.finalize().map_err(write_err)?;
        // Écriture forcée sur le disque (Windows : le fichier doit être ouvert en écriture).
        std::fs::OpenOptions::new()
            .write(true)
            .open(&part)
            .and_then(|f| f.sync_all())
            .context("écriture sur le disque")?;
        // Annoncé avant le renommage : un processus fils arrêté juste après laisse un
        // fichier que l'application sait être le sien (et retire).
        (self.on_segment)(SegmentEvent::Naming(&name));
        // Jamais par-dessus une prise existante (renommage qui refuse d'écraser).
        match crate::project::rename_no_replace(&part, &dst) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("« {name} » existe déjà dans le dossier Audio")
            }
            r => r.context("écriture dans le dossier Audio")?,
        }
        self.done.push((name.clone(), self.frames));
        (self.on_segment)(SegmentEvent::Done(&name, self.frames));
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<(String, u64)>> {
        self.close()?;
        self.committed = true;
        Ok(std::mem::take(&mut self.done))
    }
}

impl Drop for Segments<'_> {
    fn drop(&mut self) {
        if !self.committed {
            drop(self.writer.take());
            remove_segments(self.dir, self.base, &self.done);
        }
    }
}

/// Décode `src`, le convertit à `sr` et l'écrit dans `audio_dir/file` (WAV 32 bits
/// flottant, mono ou stéréo comme l'original ; au-delà de 2 canaux, les deux premiers),
/// découpé en fichiers de 2 Go au plus. `on_progress(f)` renvoie `false` pour annuler ;
/// `on_segment` suit chaque fichier : son nom juste avant qu'il n'y soit renommé, puis
/// le fichier terminé.
pub fn import_file(
    src: &Path,
    audio_dir: &Path,
    file: &str,
    sr: u32,
    on_progress: &dyn Fn(f64) -> bool,
    on_segment: &dyn Fn(SegmentEvent<'_>),
) -> Result<Imported> {
    import_file_limited(
        src,
        audio_dir,
        file,
        sr,
        on_progress,
        on_segment,
        MAX_SEGMENT_BYTES,
    )
}

fn import_file_limited(
    src: &Path,
    audio_dir: &Path,
    file: &str,
    sr: u32,
    on_progress: &dyn Fn(f64) -> bool,
    on_segment: &dyn Fn(SegmentEvent<'_>),
    max_bytes: u64,
) -> Result<Imported> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::errors::Error as SymError;

    if !crate::decode::VALID_SOURCE_RATES.contains(&sr) {
        bail!("fréquence de la session invalide ({sr} Hz)");
    }
    let crate::decode::Opened {
        mut format,
        mut decoder,
        track_id,
        sample_rate,
        channels,
        n_frames: total,
        mut trim,
        ..
    } = crate::decode::open(src)?;
    // Fréquence déjà bornée par `decode::open` : un en-tête fantaisiste ne dimensionne
    // jamais le rééchantillonneur.
    let src_sr = sample_rate.ok_or_else(|| anyhow!("fréquence d'échantillonnage inconnue"))?;
    let channels = channels.unwrap_or(2).clamp(1, 2);
    let mut conv = Converter::new(channels, src_sr, sr)?;

    std::fs::create_dir_all(audio_dir).context("création du dossier Audio")?;
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate: sr,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut out = Segments::new(audio_dir, file, spec, max_bytes, on_segment);
    let mut buf: Option<SampleBuffer<f32>> = None;
    let mut read = 0u64;
    let mut write = |block: &[f32]| -> Result<()> { out.write(block) };
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => bail!("lecture impossible : {e}"),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // Paquet abîmé isolé (fréquent en MP3) : on continue.
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => bail!("décodage impossible : {e}"),
        };
        let dspec = *decoded.spec();
        let ch = dspec.channels.count().max(1);
        let b = buf.get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, dspec));
        if b.capacity() < decoded.capacity() * ch {
            *b = SampleBuffer::new(decoded.capacity() as u64, dspec);
        }
        b.copy_interleaved_ref(decoded);
        // Délai d'encodeur et bourrage retirés : l'import reste calé sur l'original.
        let kept = trim.apply(b.samples(), ch);
        read += (kept.len() / ch) as u64;
        conv.push(kept, ch, &mut write)?;
        let f = total.map_or(0.0, |t| read as f64 / t.max(1) as f64);
        if !on_progress(f) {
            return Err(Cancelled.into());
        }
    }
    conv.finish(&mut write)?;
    drop(write);
    if out.done.is_empty() && out.frames == 0 {
        bail!("aucun son dans le fichier");
    }
    let segments = out.finish()?;
    let name = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Import".into());
    Ok(Imported {
        name,
        segments,
        stereo: channels == 2,
    })
}

fn write_err(e: hound::Error) -> anyhow::Error {
    match e {
        hound::Error::IoError(io) if io.kind() == std::io::ErrorKind::StorageFull => {
            anyhow!("disque plein")
        }
        e => anyhow!("écriture dans Audio : {e}"),
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

    fn no_part_left(audio: &Path) -> bool {
        std::fs::read_dir(audio)
            .map(|rd| {
                rd.flatten()
                    .all(|e| !e.file_name().to_string_lossy().ends_with(".part"))
            })
            .unwrap_or(true)
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
            &|_| {},
        )
        .unwrap();
        assert!(i.stereo);
        assert_eq!(i.name, "chanson");
        let want = ((44100u64 * 3 + 17) * 48000).div_ceil(44100);
        assert_eq!(i.frames(), want);
        assert_eq!(i.segments, vec![("chanson_001.wav".to_string(), want)]);
        let r = hound::WavReader::open(audio.join("chanson_001.wav")).unwrap();
        assert_eq!(r.spec().sample_rate, 48000);
        assert_eq!(r.spec().channels, 2);
        assert_eq!(r.duration() as u64, want);
        // Mono à la même fréquence : copie exacte de la longueur.
        write_wav(&d.join("voix.wav"), 48000, 1, 12345);
        let i = import_file(
            &d.join("voix.wav"),
            &audio,
            "voix_001.wav",
            48000,
            &|_| true,
            &|_| {},
        )
        .unwrap();
        assert!(!i.stereo);
        assert_eq!(i.frames(), 12345);
        // Aucun fichier temporaire laissé.
        assert!(no_part_left(&audio));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Import long : découpé en fichiers bornés (comme une prise), nommés `_p2`, `_p3`…,
    /// chacun avec un en-tête juste. Sans cela, au-delà de 4 Gio, la taille 32 bits du WAV
    /// déborde et la fin de l'import devient muette.
    #[test]
    fn long_import_is_split_into_segments() {
        let d = tmp("split");
        let audio = d.join("Audio");
        write_wav(&d.join("concert.wav"), 48000, 2, 10_000);
        let seen = std::cell::RefCell::new(Vec::new());
        let named = std::cell::RefCell::new(Vec::<String>::new());
        // 1 000 trames stéréo flottantes par fichier (8 000 octets + en-tête).
        let i = import_file_limited(
            &d.join("concert.wav"),
            &audio,
            "concert_001.wav",
            48000,
            &|_| true,
            &|e| match e {
                // Chaque nom est annoncé avant que le fichier n'y soit renommé.
                SegmentEvent::Naming(n) => {
                    assert!(!audio.join(n).exists());
                    named.borrow_mut().push(n.to_string());
                }
                SegmentEvent::Done(n, f) => {
                    assert_eq!(named.borrow().last().map(String::as_str), Some(n));
                    seen.borrow_mut().push((n.to_string(), f));
                }
            },
            8_044,
        )
        .unwrap();
        assert_eq!(i.frames(), 10_000);
        assert_eq!(i.segments.len(), 10);
        assert_eq!(i.segments[0].0, "concert_001.wav");
        assert_eq!(i.segments[1].0, "concert_001_p2.wav");
        assert_eq!(*seen.borrow(), i.segments);
        assert_eq!(named.borrow().len(), 10);
        for (f, n) in &i.segments {
            let r = hound::WavReader::open(audio.join(f)).unwrap();
            assert_eq!(r.duration() as u64, *n);
        }
        // Annulé au milieu : rien ne reste, segments terminés compris.
        let calls = std::cell::Cell::new(0);
        let e = import_file_limited(
            &d.join("concert.wav"),
            &audio,
            "autre_001.wav",
            48000,
            &|_| {
                calls.set(calls.get() + 1);
                calls.get() < 3
            },
            &|_| {},
            8_044,
        )
        .err()
        .unwrap();
        assert!(e.is::<Cancelled>());
        let left: Vec<_> = std::fs::read_dir(&audio)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("autre"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        assert!(no_part_left(&audio));
        // Un segment ne remplace jamais une prise existante.
        std::fs::write(audio.join("x_001_p2.wav"), b"prise").unwrap();
        assert!(
            import_file_limited(
                &d.join("concert.wav"),
                &audio,
                "x_001.wav",
                48000,
                &|_| true,
                &|_| {},
                8_044,
            )
            .is_err()
        );
        assert_eq!(std::fs::read(audio.join("x_001_p2.wav")).unwrap(), b"prise");
        assert!(!audio.join("x_001.wav").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_file_and_cancel_leave_nothing() {
        let d = tmp("bad");
        let audio = d.join("Audio");
        std::fs::write(d.join("faux.mp3"), b"pas du son").unwrap();
        let e = import_file(
            &d.join("faux.mp3"),
            &audio,
            "faux_001.wav",
            48000,
            &|_| true,
            &|_| {},
        )
        .err()
        .unwrap();
        assert!(e.to_string().contains("format"), "{e:#}");
        write_wav(&d.join("long.wav"), 48000, 1, 48000 * 5);
        let e = import_file(
            &d.join("long.wav"),
            &audio,
            "long_001.wav",
            48000,
            &|_| false,
            &|_| {},
        )
        .err()
        .unwrap();
        assert!(e.is::<Cancelled>());
        let left = std::fs::read_dir(&audio).map(|r| r.count()).unwrap_or(0);
        assert_eq!(left, 0);
        // Fréquence fantaisiste (nombre premier de 4 GHz) : refusée avant de créer le
        // rééchantillonneur (qui réserverait des dizaines de Go).
        write_wav(&d.join("piege.wav"), 48000, 1, 100);
        let mut b = std::fs::read(d.join("piege.wav")).unwrap();
        b[24..28].copy_from_slice(&4_294_967_291u32.to_le_bytes());
        std::fs::write(d.join("piege.wav"), &b).unwrap();
        let e = import_file(
            &d.join("piege.wav"),
            &audio,
            "piege_001.wav",
            48000,
            &|_| true,
            &|_| {},
        )
        .err()
        .unwrap();
        assert!(e.to_string().contains("fréquence"), "{e:#}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
