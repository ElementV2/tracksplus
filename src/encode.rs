//! Écriture des fichiers d'export : WAV et BWF (bascule automatique en RF64 au-delà de
//! 4 Go), AIFF, FLAC (flacenc, pur Rust), MP3 (LAME) et Ogg Vorbis (libvorbis aoTuV).
//!
//! Chaque format reçoit des blocs d'échantillons flottants entrelacés (±1) ; la
//! quantification (et le dither TPDF en 16 bits) est faite ici.

use crate::export::{BitDepth, ExportFormat, Mp3Bitrate, OggQuality};
use anyhow::{Context, Result, anyhow, bail};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::num::{NonZeroU8, NonZeroU32};
use std::path::Path;

/// Nom écrit dans les métadonnées des fichiers.
const ENCODER: &str = concat!("TracksPlus ", env!("CARGO_PKG_VERSION"));

/// Informations écrites dans les métadonnées (bext, ID3, commentaires Vorbis/FLAC).
#[derive(Clone, Debug, Default)]
pub struct FileMeta {
    /// Titre : nom de la piste, ou « Mix ».
    pub title: String,
    /// Album : nom de la session.
    pub album: String,
    /// Description BWF (« Session - Piste »).
    pub description: String,
    /// Début de la plage exportée, en échantillons à la fréquence de sortie
    /// (TimeReference du BWF).
    pub time_reference: u64,
}

/// Paramètres d'un fichier à écrire.
pub struct SinkSpec<'a> {
    pub format: ExportFormat,
    /// Résolution (formats PCM et FLAC ; ignorée pour MP3 et Ogg).
    pub depth: BitDepth,
    pub dither: bool,
    pub channels: usize,
    pub sample_rate: u32,
    pub mp3_bitrate: Mp3Bitrate,
    pub ogg_quality: OggQuality,
    pub meta: &'a FileMeta,
    /// Graine du dither : différente pour chaque fichier d'un même export (sinon le bruit
    /// des pistes séparées s'additionnerait de façon cohérente au remixage).
    pub seed: u64,
}

/// Destination des échantillons d'un fichier exporté.
pub trait Sink {
    /// Échantillons flottants entrelacés (±1), un nombre entier de trames.
    fn write(&mut self, samples: &[f32]) -> Result<()>;
    /// Termine le fichier (tailles, en-têtes, étiquettes).
    fn finish(self: Box<Self>) -> Result<()>;
}

/// Ouvre le fichier `path` et prépare l'encodeur du format demandé.
pub fn create(path: &Path, spec: &SinkSpec) -> Result<Box<dyn Sink>> {
    let file = File::create(path).with_context(|| format!("Création de {}", path.display()))?;
    let w = BufWriter::with_capacity(1 << 20, file);
    Ok(match spec.format {
        ExportFormat::Wav => Box::new(WavSink::new(w, spec, None)?),
        ExportFormat::Bwf => {
            let bext = bext_chunk(spec, chrono::Local::now().naive_local());
            Box::new(WavSink::new(w, spec, Some(bext))?)
        }
        ExportFormat::Aiff => Box::new(AiffSink::new(w, spec)?),
        ExportFormat::Flac => Box::new(FlacSink::new(w, spec)?),
        ExportFormat::Mp3 => Box::new(Mp3Sink::new(w, spec)?),
        ExportFormat::Ogg => Box::new(OggSink::new(w, spec)?),
    })
}

// ---------------------------------------------------------------------------
// Quantification
// ---------------------------------------------------------------------------

/// Conversion flottant → entier, avec dither TPDF optionnel en 16 bits.
struct Quantizer {
    bits: u16,
    dither: bool,
    rng: u64,
}

impl Quantizer {
    fn new(bits: u16, dither: bool, seed: u64) -> Self {
        Self {
            bits,
            dither: dither && bits == 16,
            // Jamais nulle (xorshift).
            rng: 0x9E37_79B9_7F4A_7C15 ^ seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1,
        }
    }

    fn rand(&mut self) -> f32 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let v = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 40) as f32 / (1u64 << 24) as f32
    }

    fn int(&mut self, x: f32) -> i32 {
        let x = x.clamp(-1.0, 1.0);
        match self.bits {
            16 => {
                let mut v = x * 32767.0;
                if self.dither {
                    v += self.rand() - self.rand();
                }
                v.round().clamp(-32768.0, 32767.0) as i32
            }
            24 => (x * 8_388_607.0).round() as i32,
            _ => (x as f64 * 2_147_483_647.0).round() as i32,
        }
    }
}

/// Codage PCM d'un fichier WAV/AIFF.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Pcm {
    Int(u16),
    Float,
}

impl Pcm {
    fn from_depth(d: BitDepth) -> Self {
        match d {
            BitDepth::I16 => Pcm::Int(16),
            BitDepth::I24 => Pcm::Int(24),
            BitDepth::I32 => Pcm::Int(32),
            BitDepth::F32 => Pcm::Float,
        }
    }

    fn bits(self) -> u16 {
        match self {
            Pcm::Int(b) => b,
            Pcm::Float => 32,
        }
    }

    fn bytes(self) -> usize {
        self.bits() as usize / 8
    }
}

// ---------------------------------------------------------------------------
// WAV / BWF / RF64
// ---------------------------------------------------------------------------

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// Corps du chunk ds64 sans table : riffSize, dataSize, sampleCount (u64) + tableLength.
const DS64_BODY: usize = 28;

fn put_chunk(h: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    h.extend_from_slice(id);
    h.extend_from_slice(&(body.len() as u32).to_le_bytes());
    h.extend_from_slice(body);
    if body.len() % 2 == 1 {
        h.push(0);
    }
}

/// En-tête WAV complet (jusqu'à l'en-tête du chunk `data` inclus) pour `data_bytes`
/// octets d'échantillons. Tant que tout tient sur 32 bits : RIFF avec un chunk JUNK
/// réservé ; au-delà : RF64 avec le chunk ds64 à la place du JUNK. Les deux variantes
/// ont exactement la même longueur, ce qui permet de réécrire l'en-tête en fin d'export.
pub(crate) fn wav_header(
    channels: u16,
    sample_rate: u32,
    pcm: Pcm,
    bext: Option<&[u8]>,
    data_bytes: u64,
) -> Vec<u8> {
    let block_align = channels * pcm.bytes() as u16;
    let frames = data_bytes / block_align.max(1) as u64;
    let float = pcm == Pcm::Float;

    let mut fmt = Vec::with_capacity(18);
    let tag = if float {
        WAVE_FORMAT_IEEE_FLOAT
    } else {
        WAVE_FORMAT_PCM
    };
    fmt.extend_from_slice(&tag.to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&sample_rate.to_le_bytes());
    fmt.extend_from_slice(&(sample_rate * block_align as u32).to_le_bytes());
    fmt.extend_from_slice(&block_align.to_le_bytes());
    fmt.extend_from_slice(&pcm.bits().to_le_bytes());
    if float {
        // cbSize : obligatoire hors WAVE_FORMAT_PCM.
        fmt.extend_from_slice(&0u16.to_le_bytes());
    }

    let mut h = Vec::with_capacity(128 + bext.map_or(0, <[u8]>::len));
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&[0; 4]);
    h.extend_from_slice(b"WAVE");
    let ds64_pos = h.len();
    put_chunk(&mut h, b"JUNK", &[0; DS64_BODY]);
    put_chunk(&mut h, b"fmt ", &fmt);
    let fact_pos = if float {
        put_chunk(&mut h, b"fact", &[0; 4]);
        Some(h.len() - 4)
    } else {
        None
    };
    if let Some(b) = bext {
        put_chunk(&mut h, b"bext", b);
    }
    h.extend_from_slice(b"data");
    let data_size_pos = h.len();
    h.extend_from_slice(&[0; 4]);

    let riff_size = (h.len() as u64 - 8) + data_bytes + (data_bytes & 1);
    let rf64 = riff_size > u32::MAX as u64;
    let (riff32, data32, frames32) = if rf64 {
        h[0..4].copy_from_slice(b"RF64");
        h[ds64_pos..ds64_pos + 4].copy_from_slice(b"ds64");
        let mut body = Vec::with_capacity(DS64_BODY);
        body.extend_from_slice(&riff_size.to_le_bytes());
        body.extend_from_slice(&data_bytes.to_le_bytes());
        body.extend_from_slice(&frames.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        h[ds64_pos + 8..ds64_pos + 8 + DS64_BODY].copy_from_slice(&body);
        (u32::MAX, u32::MAX, u32::MAX)
    } else {
        (riff_size as u32, data_bytes as u32, frames as u32)
    };
    h[4..8].copy_from_slice(&riff32.to_le_bytes());
    h[data_size_pos..data_size_pos + 4].copy_from_slice(&data32.to_le_bytes());
    if let Some(p) = fact_pos {
        h[p..p + 4].copy_from_slice(&frames32.to_le_bytes());
    }
    h
}

/// Texte limité à l'ASCII (champs bext) : accents retirés, autres caractères en « _ ».
fn ascii_fold(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => 'A',
            'ç' => 'c',
            'Ç' => 'C',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'È' | 'É' | 'Ê' | 'Ë' => 'E',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'Ì' | 'Í' | 'Î' | 'Ï' => 'I',
            'ñ' => 'n',
            'Ñ' => 'N',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => 'o',
            'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' => 'O',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'Ù' | 'Ú' | 'Û' | 'Ü' => 'U',
            'ý' | 'ÿ' => 'y',
            'Ý' | 'Ÿ' => 'Y',
            c if c.is_ascii() && !c.is_ascii_control() => c,
            _ => '_',
        })
        .collect()
}

/// Champ texte de taille fixe, complété par des zéros.
fn put_fixed(b: &mut Vec<u8>, s: &str, n: usize) {
    let bytes = s.as_bytes();
    let k = bytes.len().min(n);
    b.extend_from_slice(&bytes[..k]);
    b.resize(b.len() + n - k, 0);
}

/// Corps du chunk `bext` (EBU Tech 3285, version 1).
pub(crate) fn bext_chunk(spec: &SinkSpec, now: chrono::NaiveDateTime) -> Vec<u8> {
    let m = spec.meta;
    let mut b = Vec::with_capacity(700);
    put_fixed(&mut b, &ascii_fold(&m.description), 256);
    put_fixed(&mut b, "TracksPlus", 32);
    // OriginatorReference au format USID : pays, organisation, numéro de série,
    // heure d'origine, nombre aléatoire (32 caractères).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let usid = format!(
        "FRTPL{:012}{}{:09}",
        (nanos / 1_000_000) % 1_000_000_000_000,
        now.format("%H%M%S"),
        (nanos as u64 ^ m.time_reference.rotate_left(17)) % 1_000_000_000
    );
    put_fixed(&mut b, &usid, 32);
    put_fixed(&mut b, &now.format("%Y-%m-%d").to_string(), 10);
    put_fixed(&mut b, &now.format("%H:%M:%S").to_string(), 8);
    b.extend_from_slice(&(m.time_reference as u32).to_le_bytes());
    b.extend_from_slice(&((m.time_reference >> 32) as u32).to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // Version
    b.resize(b.len() + 64, 0); // UMID (non renseigné)
    b.resize(b.len() + 190, 0); // Réservé (version 1)
    debug_assert_eq!(b.len(), 602);
    let pcm = Pcm::from_depth(spec.depth);
    let history = format!(
        "A=PCM,F={},W={},M={},T={}\r\n",
        spec.sample_rate,
        pcm.bits(),
        if spec.channels == 1 { "mono" } else { "stereo" },
        ascii_fold(ENCODER)
    );
    b.extend_from_slice(history.as_bytes());
    if b.len() % 2 == 1 {
        b.push(0);
    }
    b
}

struct WavSink {
    w: BufWriter<File>,
    channels: u16,
    sample_rate: u32,
    pcm: Pcm,
    bext: Option<Vec<u8>>,
    q: Quantizer,
    data_bytes: u64,
    buf: Vec<u8>,
}

impl WavSink {
    fn new(mut w: BufWriter<File>, spec: &SinkSpec, bext: Option<Vec<u8>>) -> Result<Self> {
        let pcm = Pcm::from_depth(spec.depth);
        let channels = spec.channels as u16;
        w.write_all(&wav_header(
            channels,
            spec.sample_rate,
            pcm,
            bext.as_deref(),
            0,
        ))?;
        Ok(Self {
            w,
            channels,
            sample_rate: spec.sample_rate,
            pcm,
            bext,
            q: Quantizer::new(pcm.bits(), spec.dither, spec.seed),
            data_bytes: 0,
            buf: Vec::new(),
        })
    }
}

impl Sink for WavSink {
    fn write(&mut self, samples: &[f32]) -> Result<()> {
        self.buf.clear();
        match self.pcm {
            Pcm::Int(16) => {
                for &x in samples {
                    let v = self.q.int(x) as i16;
                    self.buf.extend_from_slice(&v.to_le_bytes());
                }
            }
            Pcm::Int(24) => {
                for &x in samples {
                    self.buf
                        .extend_from_slice(&self.q.int(x).to_le_bytes()[..3]);
                }
            }
            Pcm::Int(_) => {
                for &x in samples {
                    self.buf.extend_from_slice(&self.q.int(x).to_le_bytes());
                }
            }
            Pcm::Float => {
                for &x in samples {
                    self.buf.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
        self.w.write_all(&self.buf)?;
        self.data_bytes += self.buf.len() as u64;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        if self.data_bytes % 2 == 1 {
            self.w.write_all(&[0])?;
        }
        let h = wav_header(
            self.channels,
            self.sample_rate,
            self.pcm,
            self.bext.as_deref(),
            self.data_bytes,
        );
        self.w.seek(SeekFrom::Start(0))?;
        self.w.write_all(&h)?;
        self.w.flush()?;
        self.w.get_ref().sync_all()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AIFF
// ---------------------------------------------------------------------------

/// Taille maximale des échantillons d'un AIFF (tailles de chunks sur 32 bits).
pub const MAX_AIFF_DATA: u64 = u32::MAX as u64 - 1024;

/// Fréquence au format flottant étendu 80 bits (IEEE 754, big-endian) du chunk COMM.
pub(crate) fn ieee_extended(rate: u32) -> [u8; 10] {
    let mut out = [0u8; 10];
    if rate == 0 {
        return out;
    }
    let e = 31 - rate.leading_zeros();
    let exponent = 16383 + e as u16;
    let mantissa = (rate as u64) << (63 - e);
    out[..2].copy_from_slice(&exponent.to_be_bytes());
    out[2..].copy_from_slice(&mantissa.to_be_bytes());
    out
}

/// En-tête AIFF (FORM, COMM, en-tête SSND) pour `frames` trames.
pub(crate) fn aiff_header(channels: u16, sample_rate: u32, bits: u16, frames: u32) -> Vec<u8> {
    let data = frames as u64 * channels as u64 * (bits as u64 / 8);
    let form_size = 4 + (8 + 18) + (8 + 8 + data) + (data & 1);
    let mut h = Vec::with_capacity(54);
    h.extend_from_slice(b"FORM");
    h.extend_from_slice(&(form_size.min(u32::MAX as u64) as u32).to_be_bytes());
    h.extend_from_slice(b"AIFF");
    h.extend_from_slice(b"COMM");
    h.extend_from_slice(&18u32.to_be_bytes());
    h.extend_from_slice(&channels.to_be_bytes());
    h.extend_from_slice(&frames.to_be_bytes());
    h.extend_from_slice(&bits.to_be_bytes());
    h.extend_from_slice(&ieee_extended(sample_rate));
    h.extend_from_slice(b"SSND");
    h.extend_from_slice(&((8 + data).min(u32::MAX as u64) as u32).to_be_bytes());
    h.extend_from_slice(&0u32.to_be_bytes()); // offset
    h.extend_from_slice(&0u32.to_be_bytes()); // blockSize
    h
}

struct AiffSink {
    w: BufWriter<File>,
    channels: u16,
    sample_rate: u32,
    bits: u16,
    q: Quantizer,
    data_bytes: u64,
    buf: Vec<u8>,
}

impl AiffSink {
    fn new(mut w: BufWriter<File>, spec: &SinkSpec) -> Result<Self> {
        // L'AIFF classique ne stocke que des entiers : le 32 bits flottant devient 32 bits.
        let bits = match spec.depth {
            BitDepth::I16 => 16,
            BitDepth::I24 => 24,
            BitDepth::I32 | BitDepth::F32 => 32,
        };
        let channels = spec.channels as u16;
        w.write_all(&aiff_header(channels, spec.sample_rate, bits, 0))?;
        Ok(Self {
            w,
            channels,
            sample_rate: spec.sample_rate,
            bits,
            q: Quantizer::new(bits, spec.dither, spec.seed),
            data_bytes: 0,
            buf: Vec::new(),
        })
    }
}

impl Sink for AiffSink {
    fn write(&mut self, samples: &[f32]) -> Result<()> {
        self.buf.clear();
        for &x in samples {
            let v = self.q.int(x);
            match self.bits {
                16 => self.buf.extend_from_slice(&(v as i16).to_be_bytes()),
                24 => self.buf.extend_from_slice(&v.to_be_bytes()[1..]),
                _ => self.buf.extend_from_slice(&v.to_be_bytes()),
            }
        }
        self.data_bytes += self.buf.len() as u64;
        if self.data_bytes > MAX_AIFF_DATA {
            bail!("Fichier AIFF trop volumineux (limite de 4 Go du format)");
        }
        self.w.write_all(&self.buf)?;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        if self.data_bytes % 2 == 1 {
            self.w.write_all(&[0])?;
        }
        let frame_bytes = self.channels as u64 * (self.bits as u64 / 8);
        let frames = (self.data_bytes / frame_bytes) as u32;
        let h = aiff_header(self.channels, self.sample_rate, self.bits, frames);
        self.w.seek(SeekFrom::Start(0))?;
        self.w.write_all(&h)?;
        self.w.flush()?;
        self.w.get_ref().sync_all()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FLAC
// ---------------------------------------------------------------------------

/// Taille des blocs FLAC (valeur par défaut de l'encodeur de référence).
const FLAC_BLOCK: usize = 4096;

/// Corps d'un bloc VORBIS_COMMENT (aussi utilisé par FLAC), en little-endian.
fn vorbis_comment(tags: &[(&str, &str)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(ENCODER.len() as u32).to_le_bytes());
    b.extend_from_slice(ENCODER.as_bytes());
    let tags: Vec<String> = tags
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    b.extend_from_slice(&(tags.len() as u32).to_le_bytes());
    for t in &tags {
        b.extend_from_slice(&(t.len() as u32).to_le_bytes());
        b.extend_from_slice(t.as_bytes());
    }
    b
}

/// FLAC écrit au fil de l'eau : en-têtes provisoires, trames encodées une par une, puis
/// STREAMINFO (nombre d'échantillons, tailles, MD5) réécrit à la fin.
struct FlacSink {
    w: BufWriter<File>,
    config: flacenc::error::Verified<flacenc::config::Encoder>,
    stream: flacenc::component::Stream,
    fb: flacenc::source::FrameBuf,
    ctx: flacenc::source::Context,
    pending: Vec<i32>,
    q: Quantizer,
    channels: usize,
    frame_no: usize,
}

fn flac_header(stream: &flacenc::component::Stream) -> Result<Vec<u8>> {
    use flacenc::component::BitRepr;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| anyhow!("En-tête FLAC : {e}"))?;
    Ok(sink.as_slice().to_vec())
}

impl FlacSink {
    fn new(mut w: BufWriter<File>, spec: &SinkSpec) -> Result<Self> {
        use flacenc::error::Verify;
        let bits = match spec.depth {
            BitDepth::I16 => 16,
            _ => 24,
        };
        let mut config = flacenc::config::Encoder::default();
        config.block_size = FLAC_BLOCK;
        let config = config
            .into_verified()
            .map_err(|(_, e)| anyhow!("Configuration FLAC : {e}"))?;
        let mut stream =
            flacenc::component::Stream::new(spec.sample_rate as usize, spec.channels, bits)
                .map_err(|e| anyhow!("FLAC : {e}"))?;
        let m = spec.meta;
        let vc = vorbis_comment(&[
            ("TITLE", &m.title),
            ("ALBUM", &m.album),
            ("ENCODER", ENCODER),
        ]);
        stream.add_metadata_block(
            flacenc::component::MetadataBlockData::new_unknown(4, &vc)
                .map_err(|e| anyhow!("FLAC : {e}"))?,
        );
        w.write_all(&flac_header(&stream)?)?;
        Ok(Self {
            w,
            config,
            stream,
            fb: flacenc::source::FrameBuf::with_size(spec.channels, FLAC_BLOCK)
                .map_err(|e| anyhow!("FLAC : {e}"))?,
            ctx: flacenc::source::Context::new(bits, spec.channels),
            pending: Vec::with_capacity(FLAC_BLOCK * 2 * spec.channels),
            q: Quantizer::new(bits as u16, spec.dither, spec.seed),
            channels: spec.channels,
            frame_no: 0,
        })
    }

    /// Encode les `n` premiers échantillons en attente en une trame FLAC.
    fn encode_frame(&mut self, n: usize) -> Result<()> {
        use flacenc::component::BitRepr;
        use flacenc::source::Fill;
        (&mut self.fb, &mut self.ctx)
            .fill_interleaved(&self.pending[..n])
            .map_err(|e| anyhow!("FLAC : {e}"))?;
        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.fb,
            self.frame_no,
            self.stream.stream_info(),
        )
        .map_err(|e| anyhow!("FLAC : {e}"))?;
        self.stream.stream_info_mut().update_frame_info(&frame);
        let mut sink = flacenc::bitsink::ByteSink::new();
        frame.write(&mut sink).map_err(|e| anyhow!("FLAC : {e}"))?;
        self.w.write_all(sink.as_slice())?;
        self.pending.drain(..n);
        self.frame_no += 1;
        Ok(())
    }
}

impl Sink for FlacSink {
    fn write(&mut self, samples: &[f32]) -> Result<()> {
        for &x in samples {
            let v = self.q.int(x);
            self.pending.push(v);
        }
        let n = FLAC_BLOCK * self.channels;
        while self.pending.len() >= n {
            self.encode_frame(n)?;
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        if !self.pending.is_empty() {
            let n = self.pending.len();
            self.encode_frame(n)?;
        }
        let md5 = self.ctx.md5_digest();
        let info = self.stream.stream_info_mut();
        info.set_md5_digest(&md5);
        // Flux à taille de bloc fixe : seule la dernière trame peut être plus courte.
        info.set_block_sizes(FLAC_BLOCK, FLAC_BLOCK)
            .map_err(|e| anyhow!("FLAC : {e}"))?;
        let h = flac_header(&self.stream)?;
        self.w.seek(SeekFrom::Start(0))?;
        self.w.write_all(&h)?;
        self.w.flush()?;
        self.w.get_ref().sync_all()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MP3 (LAME)
// ---------------------------------------------------------------------------

struct Mp3Sink {
    w: BufWriter<File>,
    enc: mp3lame_encoder::Encoder,
    channels: usize,
    /// Position de la première trame MP3 (après l'étiquette ID3v2) : la trame
    /// Xing/LAME y est réécrite à la fin (durée exacte, recherche en VBR).
    audio_start: u64,
    out: Vec<u8>,
    tmp: Vec<f32>,
}

impl Mp3Sink {
    fn new(mut w: BufWriter<File>, spec: &SinkSpec) -> Result<Self> {
        use mp3lame_encoder::{Builder, Mode, Quality, VbrMode};
        let lame = |e: mp3lame_encoder::BuildError| anyhow!("Encodeur MP3 : {e}");
        let mut b = Builder::new().context("Encodeur MP3 indisponible")?;
        b.set_num_channels(spec.channels as u8).map_err(lame)?;
        b.set_sample_rate(spec.sample_rate).map_err(lame)?;
        b.set_output_sample_rate(NonZeroU32::new(spec.sample_rate))
            .map_err(lame)?;
        b.set_mode(if spec.channels == 1 {
            Mode::Mono
        } else {
            Mode::JointStereo
        })
        .map_err(lame)?;
        // Qualité d'algorithme 2 (« -h ») : quasi identique à 0, nettement plus rapide.
        b.set_quality(Quality::NearBest).map_err(lame)?;
        match spec.mp3_bitrate.kbps() {
            None => {
                b.set_vbr_mode(VbrMode::Mtrh).map_err(lame)?;
                b.set_vbr_quality(Quality::Best).map_err(lame)?;
            }
            Some(k) => {
                use mp3lame_encoder::Bitrate;
                let br = match k {
                    128 => Bitrate::Kbps128,
                    192 => Bitrate::Kbps192,
                    256 => Bitrate::Kbps256,
                    _ => Bitrate::Kbps320,
                };
                b.set_vbr_mode(VbrMode::Off).map_err(lame)?;
                b.set_brate(br).map_err(lame)?;
            }
        }
        b.set_to_write_vbr_tag(true).map_err(lame)?;
        let enc = b.build().map_err(lame)?;

        // Étiquette ID3v2.4 (UTF-8) écrite avant le flux MP3.
        use id3::TagLike;
        let m = spec.meta;
        let mut tag = id3::Tag::new();
        if !m.title.is_empty() {
            tag.set_title(m.title.as_str());
        }
        if !m.album.is_empty() {
            tag.set_album(m.album.as_str());
        }
        tag.set_text("TSSE", ENCODER);
        tag.write_to(&mut w, id3::Version::Id3v24)
            .map_err(|e| anyhow!("Étiquette ID3 : {e}"))?;
        let audio_start = w.stream_position()?;
        Ok(Self {
            w,
            enc,
            channels: spec.channels,
            audio_start,
            out: Vec::new(),
            tmp: Vec::new(),
        })
    }
}

impl Sink for Mp3Sink {
    fn write(&mut self, samples: &[f32]) -> Result<()> {
        let frames = samples.len() / self.channels;
        self.out.clear();
        // Taille maximale d'après la documentation de LAME : 1,25 × n + 7200.
        self.out.reserve(frames * 5 / 4 + 7200);
        self.tmp.clear();
        self.tmp.extend(samples.iter().map(|x| x.clamp(-1.0, 1.0)));
        let r = if self.channels == 1 {
            self.enc
                .encode_to_vec(mp3lame_encoder::MonoPcm(&self.tmp), &mut self.out)
        } else {
            self.enc
                .encode_to_vec(mp3lame_encoder::InterleavedPcm(&self.tmp), &mut self.out)
        };
        r.map_err(|e| anyhow!("Encodage MP3 : {e}"))?;
        self.w.write_all(&self.out)?;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.out.clear();
        self.out.reserve(7200);
        self.enc
            .flush_to_vec::<mp3lame_encoder::FlushGap>(&mut self.out)
            .map_err(|e| anyhow!("Encodage MP3 : {e}"))?;
        self.w.write_all(&self.out)?;
        let mut tag = Vec::with_capacity(self.enc.lame_tag_size());
        if self.enc.lame_tag_encode_to_vec(&mut tag).is_some() {
            self.w.seek(SeekFrom::Start(self.audio_start))?;
            self.w.write_all(&tag)?;
        }
        self.w.flush()?;
        self.w.get_ref().sync_all()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Ogg Vorbis
// ---------------------------------------------------------------------------

struct OggSink {
    enc: vorbis_rs::VorbisEncoder<BufWriter<File>>,
    channels: usize,
    planar: Vec<Vec<f32>>,
}

impl OggSink {
    fn new(w: BufWriter<File>, spec: &SinkSpec) -> Result<Self> {
        let vorbis = |e: vorbis_rs::VorbisError| anyhow!("Encodeur Vorbis : {e}");
        let sr = NonZeroU32::new(spec.sample_rate).context("Fréquence nulle")?;
        let ch = NonZeroU8::new(spec.channels as u8).context("Aucun canal")?;
        // Numéro de flux Ogg : doit seulement être différent d'un fichier à l'autre.
        let serial = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x5452_4B50, |d| {
                d.subsec_nanos() as i32 ^ d.as_secs() as i32
            });
        let mut b = vorbis_rs::VorbisEncoderBuilder::new_with_serial(sr, ch, w, serial);
        b.bitrate_management_strategy(vorbis_rs::VorbisBitrateManagementStrategy::QualityVbr {
            target_quality: spec.ogg_quality.quality(),
        });
        let m = spec.meta;
        for (k, v) in [
            ("TITLE", m.title.as_str()),
            ("ALBUM", m.album.as_str()),
            ("ENCODER", ENCODER),
        ] {
            if !v.is_empty() {
                b.comment_tag(k, v).map_err(vorbis)?;
            }
        }
        let enc = b.build().map_err(vorbis)?;
        Ok(Self {
            enc,
            channels: spec.channels,
            planar: vec![Vec::new(); spec.channels],
        })
    }
}

impl Sink for OggSink {
    fn write(&mut self, samples: &[f32]) -> Result<()> {
        for (c, p) in self.planar.iter_mut().enumerate() {
            p.clear();
            p.extend(
                samples
                    .iter()
                    .skip(c)
                    .step_by(self.channels)
                    .map(|x| x.clamp(-1.0, 1.0)),
            );
        }
        self.enc
            .encode_audio_block(&self.planar)
            .map_err(|e| anyhow!("Encodage Vorbis : {e}"))?;
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<()> {
        let mut w = self
            .enc
            .finish()
            .map_err(|e| anyhow!("Encodage Vorbis : {e}"))?;
        w.flush()?;
        w.get_ref().sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rf64_header_switches_above_4gb() {
        let small = wav_header(2, 48000, Pcm::Int(24), None, 6000);
        assert_eq!(&small[0..4], b"RIFF");
        assert_eq!(&small[12..16], b"JUNK");
        let riff = u32::from_le_bytes(small[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff, small.len() - 8 + 6000);

        // 5 Go de données en flottant stéréo : RF64, tailles 32 bits à 0xFFFFFFFF.
        let big_data = 5_000_000_000u64;
        let big = wav_header(2, 48000, Pcm::Float, None, big_data);
        assert_eq!(big.len(), wav_header(2, 48000, Pcm::Float, None, 0).len());
        assert_eq!(&big[0..4], b"RF64");
        assert_eq!(&big[4..8], &[0xFF; 4]);
        assert_eq!(&big[12..16], b"ds64");
        assert_eq!(u32::from_le_bytes(big[16..20].try_into().unwrap()), 28);
        let riff = u64::from_le_bytes(big[20..28].try_into().unwrap());
        let data = u64::from_le_bytes(big[28..36].try_into().unwrap());
        let frames = u64::from_le_bytes(big[36..44].try_into().unwrap());
        assert_eq!(riff, big.len() as u64 - 8 + big_data);
        assert_eq!(data, big_data);
        assert_eq!(frames, big_data / 8);
        let n = big.len();
        assert_eq!(&big[n - 8..n - 4], b"data");
        assert_eq!(&big[n - 4..], &[0xFF; 4]);
        // Le chunk fact (flottant) indique aussi 0xFFFFFFFF : la vraie valeur est dans ds64.
        let fact = big.windows(4).position(|w| w == b"fact").unwrap();
        assert_eq!(&big[fact + 8..fact + 12], &[0xFF; 4]);
    }

    #[test]
    fn aiff_extended_rate() {
        assert_eq!(
            ieee_extended(44100),
            [0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            ieee_extended(48000),
            [0x40, 0x0E, 0xBB, 0x80, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            ieee_extended(96000),
            [0x40, 0x0F, 0xBB, 0x80, 0, 0, 0, 0, 0, 0]
        );
        let h = aiff_header(2, 48000, 24, 1000);
        assert_eq!(h.len(), 54);
        assert_eq!(&h[0..4], b"FORM");
        assert_eq!(&h[8..12], b"AIFF");
        assert_eq!(u32::from_be_bytes(h[4..8].try_into().unwrap()), 46 + 6000);
        assert_eq!(u32::from_be_bytes(h[22..26].try_into().unwrap()), 1000);
        assert_eq!(&h[38..42], b"SSND");
    }

    #[test]
    fn ascii_fold_keeps_names_readable() {
        assert_eq!(ascii_fold("Synthé à cordes"), "Synthe a cordes");
        assert_eq!(ascii_fold("Voix 🎤"), "Voix _");
    }
}
