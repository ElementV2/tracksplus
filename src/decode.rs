//! Ouverture sûre des fichiers audio reçus (pads du soundboard, import sur la timeline).
//!
//! Un fichier abîmé ou fabriqué ne doit jamais faire tomber l'application. Une demande
//! d'allocation géante lue dans un en-tête n'est pas une panique : elle arrête le processus
//! d'un coup (`catch_unwind` n'y peut rien). D'où, avant de confier le fichier à symphonia :
//! - la structure du conteneur est contrôlée (CAF, MP4/M4A, WAV, AIFF, étiquette ID3v2) :
//!   aucun bloc ni aucune table ne peut annoncer plus de données que le fichier n'en a ;
//! - la fréquence est bornée avant de créer un rééchantillonneur, dont la taille des
//!   tampons est elle-même vérifiée ;
//! - le délai d'encodeur et le bourrage de fin (MP3, Ogg, AAC/M4A) sont retirés : un pad
//!   en boucle n'a pas de trou, un import reste calé sur l'original (« gapless »).
//!
//! Dans l'application, le décodage tourne en plus dans un processus fils (voir `isolate`) :
//! ce qui échapperait à ces contrôles (un codec piégé, par exemple) n'arrête que lui.

use anyhow::{Context, Result, anyhow, bail};
use rubato::{FftFixedIn, Resampler};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::ops::RangeInclusive;
use std::path::Path;
use symphonia::core::codecs::{CODEC_TYPE_AAC, CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSourceStream, ReadBytes};
use symphonia::core::meta::{MetadataLog, MetadataOptions};
use symphonia::core::probe::Instantiate;

/// Fréquences d'échantillonnage acceptées pour un fichier source. Au-delà, l'en-tête est
/// abîmé ou fabriqué : il ne doit pas dimensionner un rééchantillonneur.
pub const VALID_SOURCE_RATES: RangeInclusive<u32> = 1_000..=768_000;

/// Paquets annoncés au plus par la table d'un CAF (≈ 6 h d'AAC à 768 kHz) : chaque entrée
/// coûte 32 octets en mémoire, une table plus grande est forcément fabriquée.
const MAX_CAF_PACKETS: u64 = 1 << 24;

/// Taille maximale d'une FFT du rééchantillonneur (points) : quelques dizaines de Mo.
const MAX_FFT_LEN: usize = 1 << 22;

/// Trames lues par bloc pour la conversion de fréquence.
pub const CHUNK: usize = 8192;

/// Fichier ouvert, prêt à décoder.
pub struct Opened {
    pub format: Box<dyn FormatReader>,
    pub decoder: Box<dyn Decoder>,
    pub track_id: u32,
    /// Fréquence annoncée (déjà contrôlée), `None` si le fichier ne la donne pas.
    pub sample_rate: Option<u32>,
    pub channels: Option<usize>,
    /// Trames annoncées par l'en-tête (non fiable : seulement pour réserver et afficher
    /// l'avancement), délai d'encodeur et bourrage déjà retirés quand ils sont connus.
    pub n_frames: Option<u64>,
    /// Trames à retirer au début et longueur d'origine (AAC/M4A : iTunSMPB).
    pub trim: Trim,
    /// Taille du fichier (octets).
    pub file_len: u64,
}

/// Ouvre `path` : contrôle de la structure, détection du format, piste audio, décodeur.
pub fn open(path: &Path) -> Result<Opened> {
    let file = std::fs::File::open(path).context("ouverture impossible")?;
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    // Second accès au même fichier, pour lire les en-têtes sans déplacer le premier.
    let mut check = BufReader::with_capacity(
        4096,
        std::fs::File::open(path).context("ouverture impossible")?,
    );
    let mut mss = MediaSourceStream::new(Box::new(file), Default::default());
    let probe = symphonia::default::get_probe();
    let opts = FormatOptions {
        // Délai d'encodeur et bourrage de fin retirés (MP3 LAME/Xing, Ogg).
        enable_gapless: true,
        ..Default::default()
    };
    let mut probed_meta = MetadataLog::default();
    // Même parcours que `Probe::format`, avec le contrôle du conteneur juste avant que
    // son lecteur ne lise quoi que ce soit.
    let mut format = loop {
        let at = mss.pos();
        match probe
            .next(&mut mss)
            .map_err(|e| anyhow!("format non reconnu ({e})"))?
        {
            Instantiate::Format(make) => {
                check_container(&mut check, mss.pos(), file_len)?;
                break make(mss, &opts).map_err(|e| anyhow!("format non reconnu ({e})"))?;
            }
            Instantiate::Metadata(make) => {
                check_id3v2(&mut check, mss.pos(), file_len)?;
                let mut reader = make(&MetadataOptions::default());
                probed_meta.push(
                    reader
                        .read_all(&mut mss)
                        .map_err(|e| anyhow!("étiquette illisible ({e})"))?,
                );
                // Garde-fou : la recherche avance forcément.
                if mss.pos() <= at {
                    bail!("format non reconnu");
                }
            }
        }
    };
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("aucune piste audio dans le fichier"))?;
    let track_id = track.id;
    let params = track.codec_params.clone();
    if let Some(sr) = params.sample_rate
        && !VALID_SOURCE_RATES.contains(&sr)
    {
        bail!("fréquence d'échantillonnage invalide ({sr} Hz)");
    }
    let decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| anyhow!("codec non pris en charge ({e})"))?;
    // AAC : symphonia ne lit pas le délai d'encodeur des M4A ; il est dans l'étiquette
    // iTunSMPB (délai, bourrage, longueur d'origine).
    let mut trim = Trim::default();
    let mut n_frames = params.n_frames;
    if params.codec == CODEC_TYPE_AAC
        && let Some((delay, total)) = itunes_gapless(format.as_mut())
    {
        trim = Trim::new(delay, Some(total));
        n_frames = Some(total);
    }
    Ok(Opened {
        format,
        decoder,
        track_id,
        sample_rate: params.sample_rate,
        channels: params.channels.map(|c| c.count()),
        n_frames,
        trim,
        file_len,
    })
}

/// Étiquette iTunSMPB d'un M4A : (délai d'encodeur, longueur d'origine en trames).
fn itunes_gapless(format: &mut dyn FormatReader) -> Option<(u64, u64)> {
    let meta = format.metadata();
    let rev = meta.current()?;
    rev.tags()
        .iter()
        .filter(|t| t.key.to_ascii_lowercase().ends_with("itunsmpb"))
        .find_map(|t| parse_itunsmpb(&t.value.to_string()))
}

/// « 00000000 00000840 000001CA 00000000001E7E76 … » : réservé, délai, bourrage, longueur.
pub fn parse_itunsmpb(s: &str) -> Option<(u64, u64)> {
    let f: Vec<u64> = s
        .split_whitespace()
        .map(|w| u64::from_str_radix(w, 16))
        .collect::<Result<_, _>>()
        .ok()?;
    let (delay, total) = (*f.get(1)?, *f.get(3)?);
    // Valeurs invraisemblables : on n'y touche pas.
    (total > 0 && delay < 1 << 20).then_some((delay, total))
}

/// Retrait du délai d'encodeur et du bourrage de fin, au fil du décodage.
#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub struct Trim {
    skip: u64,
    limit: Option<u64>,
    kept: u64,
}

impl Trim {
    pub fn new(skip: u64, limit: Option<u64>) -> Self {
        Self {
            skip,
            limit,
            kept: 0,
        }
    }

    /// Partie à garder d'un bloc entrelacé sur `ch` canaux.
    pub fn apply<'a>(&mut self, samples: &'a [f32], ch: usize) -> &'a [f32] {
        let ch = ch.max(1);
        let n = (samples.len() / ch) as u64;
        let start = self.skip.min(n);
        self.skip -= start;
        let mut take = n - start;
        if let Some(l) = self.limit {
            take = take.min(l.saturating_sub(self.kept));
        }
        self.kept += take;
        &samples[start as usize * ch..(start + take) as usize * ch]
    }
}

// ------------------------------------------------------------------ conteneurs

fn read_at<const N: usize>(r: &mut BufReader<std::fs::File>, at: u64) -> Result<[u8; N]> {
    let mut b = [0u8; N];
    r.seek(SeekFrom::Start(at))?;
    r.read_exact(&mut b)
        .map_err(|_| anyhow!("fichier abîmé (en-tête tronqué)"))?;
    Ok(b)
}

fn fourcc(t: &[u8]) -> String {
    t.iter()
        .map(|&c| {
            if c.is_ascii_graphic() || c == b' ' {
                c as char
            } else {
                '?'
            }
        })
        .collect()
}

fn too_big(kind: &[u8]) -> anyhow::Error {
    anyhow!(
        "fichier abîmé : le bloc « {} » annonce plus de données que le fichier n'en contient",
        fourcc(kind)
    )
}

/// Contrôle du conteneur qui commence à `pos` (marqueur trouvé par la détection).
fn check_container(r: &mut BufReader<std::fs::File>, pos: u64, len: u64) -> Result<()> {
    if pos + 16 > len {
        return Ok(());
    }
    let h = read_at::<16>(r, pos)?;
    match &h[..4] {
        b"caff" => check_caf(r, pos, len),
        // Marqueur MP4 : « ftyp », 4 octets après le début du premier bloc.
        b"ftyp" => check_mp4(r, pos.saturating_sub(4), len),
        b"RIFF" => check_iff(r, pos, len, false),
        b"FORM" => check_iff(r, pos, len, true),
        _ => Ok(()),
    }
}

/// CAF : blocs (type, taille i64). La table des paquets (« pakt ») ne peut pas annoncer
/// plus de paquets que d'octets (au moins un octet par paquet).
fn check_caf(r: &mut BufReader<std::fs::File>, pos: u64, len: u64) -> Result<()> {
    let mut at = pos + 8;
    for _ in 0..100_000 {
        if at + 12 > len {
            return Ok(());
        }
        let h = read_at::<12>(r, at)?;
        let kind = &h[..4];
        let size = i64::from_be_bytes(h[4..12].try_into().unwrap());
        let avail = len - (at + 12);
        if kind == b"data" {
            // Données audio (taille -1 = jusqu'à la fin) : rien d'autre n'est lu avant.
            return Ok(());
        }
        if size < 0 || size as u64 > avail {
            return Err(too_big(kind));
        }
        if kind == b"pakt" {
            if size < 24 {
                bail!("fichier abîmé : table des paquets trop courte");
            }
            let t = read_at::<16>(r, at + 12)?;
            let packets = i64::from_be_bytes(t[..8].try_into().unwrap());
            let frames = i64::from_be_bytes(t[8..].try_into().unwrap());
            if packets < 0
                || frames < 0
                || packets as u64 > (size as u64 - 24)
                || packets as u64 > MAX_CAF_PACKETS
            {
                bail!("fichier abîmé : table des paquets invraisemblable");
            }
        }
        at += 12 + size as u64;
    }
    bail!("fichier abîmé : trop de blocs")
}

/// WAV (RIFF, petit-boutiste) et AIFF (FORM, gros-boutiste) : aucun bloc lu avant les
/// données ne dépasse la fin du fichier.
fn check_iff(r: &mut BufReader<std::fs::File>, pos: u64, len: u64, big: bool) -> Result<()> {
    let mut at = pos + 12;
    for _ in 0..100_000 {
        if at + 8 > len {
            return Ok(());
        }
        let h = read_at::<8>(r, at)?;
        let kind = &h[..4];
        let b: [u8; 4] = h[4..8].try_into().unwrap();
        let size = if big {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        } as u64;
        // Données audio : lues au fil de l'eau, jamais réservées d'après l'en-tête (et un
        // enregistrement interrompu peut annoncer plus que ce qui a été écrit).
        if kind == b"data" || kind == b"SSND" {
            return Ok(());
        }
        if size > len - (at + 8) {
            return Err(too_big(kind));
        }
        at += 8 + size + (size & 1);
    }
    bail!("fichier abîmé : trop de blocs")
}

/// Contexte d'un bloc MP4 pendant le parcours.
#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Normal,
    /// Liste des étiquettes (`ilst`) : chaque enfant est une étiquette.
    Ilst,
    /// Étiquette : ses enfants sont `data`, `mean`, `name`.
    Tag,
}

/// MP4/M4A : chaque bloc tient dans son parent, et chaque table (tailles, positions,
/// durées des échantillons) tient dans son bloc.
fn check_mp4(r: &mut BufReader<std::fs::File>, start: u64, len: u64) -> Result<()> {
    let mut count = 0u32;
    walk_atoms(r, start, len, 0, Ctx::Normal, &mut count)
}

fn walk_atoms(
    r: &mut BufReader<std::fs::File>,
    begin: u64,
    end: u64,
    depth: u32,
    ctx: Ctx,
    count: &mut u32,
) -> Result<()> {
    if depth > 16 {
        bail!("fichier abîmé : structure trop profonde");
    }
    let mut at = begin;
    while at + 8 <= end {
        *count += 1;
        if *count > 200_000 {
            bail!("fichier abîmé : trop de blocs");
        }
        let h = read_at::<8>(r, at)?;
        let kind: [u8; 4] = h[4..8].try_into().unwrap();
        let (hdr, size) = match u32::from_be_bytes(h[..4].try_into().unwrap()) {
            0 => (8, end - at),
            1 => {
                if at + 16 > end {
                    return Err(too_big(&kind));
                }
                (16, u64::from_be_bytes(read_at::<8>(r, at + 8)?))
            }
            s => (8, s as u64),
        };
        if size < hdr {
            bail!(
                "fichier abîmé : bloc « {} » de taille invalide",
                fourcc(&kind)
            );
        }
        if size > end - at {
            // Fichier tronqué pendant l'écriture des données : rien n'est lu au-delà.
            if depth == 0 && &kind == b"mdat" {
                return Ok(());
            }
            return Err(too_big(&kind));
        }
        let body = at + hdr;
        let body_len = size - hdr;
        let body_end = at + size;
        let need = |n: u64| {
            if body_len < n {
                Err(anyhow!(
                    "fichier abîmé : bloc « {} » trop court",
                    fourcc(&kind)
                ))
            } else {
                Ok(())
            }
        };
        match (&kind, ctx) {
            (
                b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"edts" | b"udta" | b"mvex"
                | b"moof" | b"traf" | b"dinf",
                Ctx::Normal,
            ) => walk_atoms(r, body, body_end, depth + 1, Ctx::Normal, count)?,
            (b"meta", Ctx::Normal) => {
                need(4)?;
                // Forme ISO (version + options avant les enfants) ou QuickTime (sans).
                let quicktime = body_len >= 8 && &read_at::<4>(r, body + 4)? == b"hdlr";
                let first = if quicktime { body } else { body + 4 };
                walk_atoms(r, first, body_end, depth + 1, Ctx::Normal, count)?
            }
            (b"ilst", Ctx::Normal) => walk_atoms(r, body, body_end, depth + 1, Ctx::Ilst, count)?,
            (_, Ctx::Ilst) => walk_atoms(r, body, body_end, depth + 1, Ctx::Tag, count)?,
            (b"data", Ctx::Tag) => need(8)?,
            (b"mean" | b"name", Ctx::Tag) => need(4)?,
            (b"hdlr", _) => need(24)?,
            (b"stsz", _) => {
                need(12)?;
                let t = read_at::<8>(r, body + 4)?;
                let sample_size = u32::from_be_bytes(t[..4].try_into().unwrap());
                let n = u32::from_be_bytes(t[4..].try_into().unwrap()) as u64;
                if sample_size == 0 && n * 4 > body_len - 12 {
                    return Err(too_big(&kind));
                }
            }
            (b"stco" | b"stss", _) => table(r, &kind, body, body_len, 4)?,
            (b"co64" | b"stts" | b"ctts", _) => table(r, &kind, body, body_len, 8)?,
            (b"stsc", _) => table(r, &kind, body, body_len, 12)?,
            _ => {}
        }
        at = body_end;
    }
    Ok(())
}

/// Table MP4 (version, options, nombre d'entrées, entrées de `entry` octets).
fn table(
    r: &mut BufReader<std::fs::File>,
    kind: &[u8],
    body: u64,
    body_len: u64,
    entry: u64,
) -> Result<()> {
    if body_len < 8 {
        return Err(too_big(kind));
    }
    let n = u32::from_be_bytes(read_at::<4>(r, body + 4)?) as u64;
    if n * entry > body_len - 8 {
        return Err(too_big(kind));
    }
    Ok(())
}

/// Étiquette ID3v2 (avant un MP3, parfois un WAV) : aucune trame ne dépasse l'étiquette,
/// qui ne dépasse pas le fichier.
fn check_id3v2(r: &mut BufReader<std::fs::File>, pos: u64, len: u64) -> Result<()> {
    if pos + 10 > len {
        return Ok(());
    }
    let h = read_at::<10>(r, pos)?;
    if &h[..3] != b"ID3" {
        return Ok(());
    }
    let (major, flags) = (h[3], h[5]);
    let syncsafe = |b: &[u8]| {
        b.iter()
            .fold(0u64, |acc, &x| (acc << 7) | (x as u64 & 0x7f))
    };
    let tag_end = pos + 10 + syncsafe(&h[6..10]);
    if tag_end > len {
        bail!("fichier abîmé : étiquette ID3 plus grande que le fichier");
    }
    // ID3v2.2 : tailles sur 3 octets (16 Mo au plus). Désynchronisation de toute
    // l'étiquette : les tailles brutes ne sont pas lisibles telles quelles.
    if !(3..=4).contains(&major) || flags & 0x80 != 0 {
        return Ok(());
    }
    let mut at = pos + 10;
    if flags & 0x40 != 0 {
        // En-tête étendu : sa taille (v2.4 : comprise, v2.3 : sans les 4 octets).
        let e = read_at::<4>(r, at)?;
        let n = if major == 4 {
            syncsafe(&e)
        } else {
            u32::from_be_bytes(e) as u64 + 4
        };
        at += n;
    }
    for _ in 0..100_000 {
        if at + 10 > tag_end {
            return Ok(());
        }
        let f = read_at::<10>(r, at)?;
        if f[0] == 0 {
            // Remplissage : fin des trames.
            return Ok(());
        }
        let size = if major == 4 {
            syncsafe(&f[4..8])
        } else {
            u32::from_be_bytes(f[4..8].try_into().unwrap()) as u64
        };
        if size > tag_end - (at + 10) {
            return Err(too_big(&f[..4]));
        }
        at += 10 + size;
    }
    bail!("fichier abîmé : trop de trames ID3")
}

// ------------------------------------------------------------------ conversion

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

/// Le rééchantillonneur (FFT de `rubato`) reste de taille raisonnable pour cette paire de
/// fréquences : ses tampons ont la taille du rapport réduit des fréquences, qu'une
/// fréquence première rend énorme.
pub fn resampler_fits(from: u32, to: u32) -> bool {
    if from == 0 || to == 0 {
        return false;
    }
    let (from, to) = (from as usize, to as usize);
    let g = gcd(from, to);
    // Même calcul que `FftFixedIn::new` (deux sous-blocs).
    let fft_chunks = (CHUNK / 2).div_ceil(from / g);
    fft_chunks.saturating_mul(from / g) <= MAX_FFT_LEN
        && fft_chunks.saturating_mul(to / g) <= MAX_FFT_LEN
}

/// Conversion par blocs : canaux (1 ou 2) puis fréquence, avec la longueur exacte.
pub struct Converter {
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
    pub fn new(channels: usize, from: u32, to: u32) -> Result<Self> {
        let rs = if from != to {
            if !resampler_fits(from, to) {
                bail!("conversion de {from} Hz vers {to} Hz impossible");
            }
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

    /// Trames attendues en sortie pour `frames` trames d'entrée.
    pub fn output_frames(&self, frames: u64) -> u64 {
        (frames as u128 * self.ratio.0 as u128).div_ceil(self.ratio.1.max(1) as u128) as u64
    }

    /// Ajoute des échantillons entrelacés sur `ch` canaux.
    pub fn push(
        &mut self,
        samples: &[f32],
        ch: usize,
        sink: &mut impl FnMut(&[f32]) -> Result<()>,
    ) -> Result<()> {
        let ch = ch.max(1);
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
        let expected = self.output_frames(self.read);
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
    pub fn finish(&mut self, sink: &mut impl FnMut(&[f32]) -> Result<()>) -> Result<()> {
        if self.rs.is_none() {
            return self.drain_direct(sink);
        }
        let n = self.pending[0].len();
        if n > 0 {
            self.process(n, true, sink)?;
        }
        let expected = self.output_frames(self.read);
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

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tp-decode-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn expect_damaged(path: &Path) {
        let e = open(path).err().expect("fichier piégé refusé");
        assert!(format!("{e:#}").contains("abîmé"), "{e:#}");
    }

    /// CAF de quelques octets dont la table annonce 2^40 paquets : refusé avant que
    /// symphonia ne réserve la table (sinon : arrêt brutal du processus).
    #[test]
    fn forged_caf_is_refused_before_allocation() {
        let d = tmp("caf");
        let mut f = b"caff\x00\x01\x00\x00".to_vec();
        // desc : 32 octets (44,1 kHz, AAC).
        f.extend_from_slice(b"desc");
        f.extend_from_slice(&32i64.to_be_bytes());
        f.extend_from_slice(&44100f64.to_be_bytes());
        f.extend_from_slice(b"aac ");
        f.extend_from_slice(&[0; 4]);
        f.extend_from_slice(&0u32.to_be_bytes());
        f.extend_from_slice(&1024u32.to_be_bytes());
        f.extend_from_slice(&2u32.to_be_bytes());
        f.extend_from_slice(&0u32.to_be_bytes());
        f.extend_from_slice(b"pakt");
        f.extend_from_slice(&24i64.to_be_bytes());
        f.extend_from_slice(&(1i64 << 40).to_be_bytes());
        f.extend_from_slice(&0i64.to_be_bytes());
        f.extend_from_slice(&[0; 8]);
        let p = d.join("piege.caf");
        std::fs::write(&p, &f).unwrap();
        expect_damaged(&p);
        // Même fichier renommé : la détection se fait sur le contenu, pas l'extension.
        let p2 = d.join("piege.wav");
        std::fs::write(&p2, &f).unwrap();
        expect_damaged(&p2);
        // Et précédé de quelques octets parasites (la détection cherche le marqueur).
        let mut junk = vec![0u8; 7];
        junk.extend_from_slice(&f);
        let p3 = d.join("piege2.caf");
        std::fs::write(&p3, &junk).unwrap();
        expect_damaged(&p3);
        // Cookie de taille géante.
        let mut k = b"caff\x00\x01\x00\x00kuki".to_vec();
        k.extend_from_slice(&(1i64 << 50).to_be_bytes());
        k.extend_from_slice(&[0; 16]);
        std::fs::write(&p, &k).unwrap();
        expect_damaged(&p);
        let _ = std::fs::remove_dir_all(d);
    }

    /// M4A dont la table des tailles annonce 4 milliards d'entrées : refusé.
    #[test]
    fn forged_mp4_tables_are_refused() {
        let d = tmp("mp4");
        let ftyp = atom(b"ftyp", b"M4A \x00\x00\x00\x00M4A mp42isom");
        let mut stsz = vec![0u8; 4];
        stsz.extend_from_slice(&0u32.to_be_bytes());
        stsz.extend_from_slice(&u32::MAX.to_be_bytes());
        let stbl = atom(b"stbl", &atom(b"stsz", &stsz));
        let minf = atom(b"minf", &stbl);
        let mdia = atom(b"mdia", &minf);
        let trak = atom(b"trak", &mdia);
        let moov = atom(b"moov", &trak);
        let mut f = ftyp.clone();
        f.extend_from_slice(&moov);
        let p = d.join("piege.m4a");
        std::fs::write(&p, &f).unwrap();
        expect_damaged(&p);
        // Bloc plus grand que son parent.
        let mut big = ftyp.clone();
        let mut moov = atom(b"moov", &atom(b"trak", &[0; 16]));
        moov[8..12].copy_from_slice(&0x7fff_ffffu32.to_be_bytes());
        big.extend_from_slice(&moov);
        std::fs::write(&p, &big).unwrap();
        expect_damaged(&p);
        // Table des positions trop longue pour son bloc.
        let mut stco = vec![0u8; 4];
        stco.extend_from_slice(&1000u32.to_be_bytes());
        let mut f = ftyp.clone();
        f.extend_from_slice(&atom(
            b"moov",
            &atom(b"trak", &atom(b"stbl", &atom(b"stco", &stco))),
        ));
        std::fs::write(&p, &f).unwrap();
        // « stbl » directement dans « trak » : parcouru quand même (tout conteneur connu).
        expect_damaged(&p);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn forged_wav_chunks_and_id3_are_refused() {
        let d = tmp("riff");
        let mut f = b"RIFF\xff\xff\xff\xffWAVE".to_vec();
        f.extend_from_slice(b"LIST");
        f.extend_from_slice(&0xffff_fff0u32.to_le_bytes());
        f.extend_from_slice(b"INFOxxxx");
        let p = d.join("piege.wav");
        std::fs::write(&p, &f).unwrap();
        expect_damaged(&p);
        // ID3v2.3 dont une trame annonce 4 Go.
        let mut id3 = b"ID3\x03\x00\x00".to_vec();
        id3.extend_from_slice(&[0, 0, 0x01, 0x00]);
        id3.extend_from_slice(b"APIC");
        id3.extend_from_slice(&0xffff_ff00u32.to_be_bytes());
        id3.extend_from_slice(&[0, 0]);
        id3.resize(10 + 128, 0);
        id3.extend_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
        id3.resize(id3.len() + 512, 0);
        let p = d.join("piege.mp3");
        std::fs::write(&p, &id3).unwrap();
        expect_damaged(&p);
        let _ = std::fs::remove_dir_all(d);
    }

    /// Un WAV normal (et un WAV interrompu, dont l'en-tête annonce plus que le fichier)
    /// passent le contrôle.
    #[test]
    fn normal_and_truncated_wav_open() {
        let d = tmp("ok");
        let p = d.join("ok.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&p, spec).unwrap();
        for i in 0..4000 {
            w.write_sample(i as i16).unwrap();
        }
        w.finalize().unwrap();
        let o = open(&p).unwrap();
        assert_eq!(o.sample_rate, Some(44100));
        assert_eq!(o.channels, Some(2));
        // Tronqué : la taille annoncée des données dépasse le fichier.
        let mut b = std::fs::read(&p).unwrap();
        b.truncate(b.len() - 1000);
        std::fs::write(&p, &b).unwrap();
        assert!(open(&p).is_ok());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn invalid_rate_is_refused() {
        let d = tmp("rate");
        let p = d.join("r.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&p, spec).unwrap();
        for _ in 0..100 {
            w.write_sample(0i16).unwrap();
        }
        w.finalize().unwrap();
        let mut b = std::fs::read(&p).unwrap();
        // Fréquence (octets 24..28) : 4 294 967 291 Hz, un nombre premier.
        b[24..28].copy_from_slice(&4_294_967_291u32.to_le_bytes());
        std::fs::write(&p, &b).unwrap();
        let e = open(&p).err().unwrap();
        assert!(format!("{e:#}").contains("fréquence"), "{e:#}");
        let _ = std::fs::remove_dir_all(d);
    }

    /// Fréquences premières : le rééchantillonneur est refusé au-delà d'une taille
    /// raisonnable, jamais créé avec des gigaoctets de tampons.
    #[test]
    fn resampler_size_is_bounded() {
        assert!(resampler_fits(44100, 48000));
        assert!(resampler_fits(48000, 192000));
        assert!(resampler_fits(767_999, 48000));
        assert!(resampler_fits(1009, 768_000));
        assert!(!resampler_fits(4_294_967_291, 48000));
        assert!(!resampler_fits(999_999_937, 48000));
        assert!(!resampler_fits(1, 48000 * 100));
        assert!(!resampler_fits(0, 48000));
        assert!(Converter::new(2, 999_999_937, 48000).is_err());
        assert!(Converter::new(2, 44100, 48000).is_ok());
    }

    #[test]
    fn itunes_gapless_and_trim() {
        assert_eq!(
            parse_itunsmpb(" 00000000 00000840 000001CA 00000000001E7E76 00000000 00000000"),
            Some((0x840, 0x1E7E76))
        );
        assert_eq!(parse_itunsmpb("n'importe quoi"), None);
        assert_eq!(parse_itunsmpb(" 00000000 00000840"), None);
        let mut t = Trim::new(3, Some(4));
        let a = [1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 4.0, 4.0];
        // 4 trames stéréo : les 3 premières retirées.
        assert_eq!(t.apply(&a, 2), &[4.0, 4.0]);
        // Puis 3 trames gardées au plus (longueur d'origine 4).
        assert_eq!(t.apply(&a, 2), &a[..6]);
        assert!(t.apply(&a, 2).is_empty());
        let mut none = Trim::default();
        assert_eq!(none.apply(&a, 2), &a);
    }
}
