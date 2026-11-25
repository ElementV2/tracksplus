//! Journal des incidents d'une session (« Journal des incidents.txt » dans son dossier) :
//! carte son perdue ou reconnectée, signal d'entrée interrompu, trou comblé, prise coupée
//! ou relancée… une ligne horodatée par événement, comme le rapport son des enregistreurs
//! de terrain. Écrit par un thread à part : un disque lent ou bloqué ne retarde jamais
//! l'interface ni la prise (au pire, des lignes sont perdues, jamais du son).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crossbeam_channel::{Sender, TrySendError, bounded};

/// Nom du fichier dans le dossier de la session.
pub const FILE: &str = "Journal des incidents.txt";

/// Lignes en attente d'écriture au plus (au-delà, les plus récentes sont perdues).
const QUEUE: usize = 512;

static WRITER: OnceLock<Option<Sender<(PathBuf, String)>>> = OnceLock::new();

fn writer() -> Option<&'static Sender<(PathBuf, String)>> {
    WRITER
        .get_or_init(|| {
            let (tx, rx) = bounded::<(PathBuf, String)>(QUEUE);
            std::thread::Builder::new()
                .name("tracksplus-incidents".into())
                .spawn(move || {
                    for (path, line) in rx {
                        let _ = append(&path, &line);
                    }
                })
                .ok()
                .map(|_| tx)
        })
        .as_ref()
}

fn append(path: &Path, line: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())?;
    // Une coupure de courant juste après ne doit pas emporter la ligne.
    f.sync_data()
}

/// Ligne du journal : date et heure locales, puis le texte (sur une seule ligne).
pub fn format_line(at: chrono::NaiveDateTime, text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    format!("{}  {}\r\n", at.format("%Y-%m-%d %H:%M:%S"), flat.trim())
}

/// Note un incident dans le journal de la session `dir` (sans jamais bloquer).
pub fn log(dir: &Path, text: &str) {
    let Some(tx) = writer() else {
        return;
    };
    let line = format_line(chrono::Local::now().naive_local(), text);
    match tx.try_send((dir.join(FILE), line)) {
        Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_dated_and_flat() {
        let at = chrono::NaiveDate::from_ymd_opt(2026, 11, 25)
            .unwrap()
            .and_hms_opt(21, 43, 12)
            .unwrap();
        assert_eq!(
            format_line(at, " Carte son perdue\npendant la prise "),
            "2026-11-25 21:43:12  Carte son perdue pendant la prise\r\n"
        );
    }

    #[test]
    fn lines_reach_the_file() {
        let dir = std::env::temp_dir().join(format!("tp-incidents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        log(&dir, "premier");
        log(&dir, "second");
        let path = dir.join(FILE);
        let t0 = std::time::Instant::now();
        loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if text.lines().count() == 2 {
                assert!(text.lines().next().unwrap().ends_with("premier"));
                break;
            }
            assert!(t0.elapsed().as_secs() < 5, "lignes écrites");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
