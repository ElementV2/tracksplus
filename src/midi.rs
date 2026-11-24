//! Contrôleurs MIDI USB (pads, grilles, claviers à pads…).
//!
//! Les messages déclenchent les pads directement depuis le thread MIDI vers le
//! moteur audio (latence minimale, même si la fenêtre est réduite), et sont aussi
//! transmis à l'interface pour l'apprentissage (MIDI learn) et l'affichage.

use crate::sampler::{PadCmd, PadParams, SampleData};
use crate::soundboard::{MidiKey, MidiKind};
use crossbeam_channel::{Receiver, Sender, unbounded};
use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub const ALL_PORTS: &str = "Tous les ports MIDI";

#[derive(Default)]
pub struct MidiShared {
    pub map: HashMap<MidiKey, (u16, Arc<SampleData>, PadParams)>,
    pub tx: Option<Sender<PadCmd>>,
    pub ctx: Option<eframe::egui::Context>,
}

#[derive(Clone, Copy, Debug)]
pub struct MidiEvent {
    pub key: MidiKey,
    pub press: bool,
}

pub struct Midi {
    pub shared: Arc<Mutex<MidiShared>>,
    pub events: Receiver<MidiEvent>,
    events_tx: Sender<MidiEvent>,
    conns: Vec<MidiInputConnection<()>>,
    pub connected: Vec<String>,
    pub error: Option<String>,
    /// Sorties MIDI utilisées pour allumer les LED du contrôleur.
    outs: Vec<(String, MidiOutputConnection)>,
    /// Dernier message LED envoyé par contrôle (pour n'envoyer que les changements).
    led_sent: HashMap<MidiKey, [u8; 3]>,
}

pub fn list_out_ports() -> Vec<String> {
    let Ok(m) = MidiOutput::new("TracksPlus") else {
        return Vec::new();
    };
    m.ports()
        .iter()
        .filter_map(|p| m.port_name(p).ok())
        .collect()
}

/// Nom de port simplifié pour associer l'entrée et la sortie d'un même contrôleur
/// (« MIDIIN2 (LPX MIDI) » ↔ « MIDIOUT2 (LPX MIDI) », « … MIDI In » ↔ « … MIDI Out »).
pub fn port_key(name: &str) -> String {
    let lower = name
        .to_lowercase()
        .replace("midiin", "midi")
        .replace("midiout", "midi");
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !matches!(*w, "in" | "out" | "input" | "output"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Façon d'allumer les LED du contrôleur.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LedMode {
    /// Détection d'après le nom du contrôleur.
    #[default]
    Auto,
    Off,
    /// Note On 127 quand le pad joue, 0 sinon (contrôleurs à LED simple).
    Generic,
    /// Novation Launchpad à palette de couleurs (MK2, X, Mini MK3, Pro) : couleur du pad
    /// faible = chargé, forte et fixe = en lecture.
    Launchpad,
    /// Akai APC mini (première génération) : jaune = chargé, vert fixe = en lecture.
    ApcMini,
    /// Akai APC mini mk2 (pads RVB) : couleur du pad à 25 % = chargé, 100 % = en lecture.
    ApcMiniMk2,
    /// Anciens Launchpad (S, Mini mk1 / mk2) : LED rouge / vert / ambre seulement.
    LaunchpadClassic,
    /// Ableton Push 2 (palette propre) : blanc = chargé, vert = en lecture.
    Push2,
}

impl LedMode {
    pub const ALL: [LedMode; 8] = [
        LedMode::Auto,
        LedMode::Off,
        LedMode::Generic,
        LedMode::Launchpad,
        LedMode::LaunchpadClassic,
        LedMode::ApcMini,
        LedMode::ApcMiniMk2,
        LedMode::Push2,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LedMode::Auto => "Automatique",
            LedMode::Off => "Désactivées",
            LedMode::Generic => "Générique (on/off)",
            LedMode::Launchpad => "Novation Launchpad (MK2, X, Mini MK3, Pro), Akai APC40 mkII",
            LedMode::LaunchpadClassic => "Novation Launchpad S / Mini (mk1, mk2)",
            LedMode::ApcMini => "Akai APC mini, APC Key 25",
            LedMode::ApcMiniMk2 => "Akai APC mini mk2, APC Key 25 mk2",
            LedMode::Push2 => "Ableton Push 2",
        }
    }

    /// Résout le mode automatique d'après le nom du port.
    pub fn resolve(self, port: &str) -> LedMode {
        if self != LedMode::Auto {
            return self;
        }
        let p = port.to_lowercase();
        if p.contains("lpx") || p.contains("lpmini") || p.contains("lppro") {
            // Noms des ports des Launchpad récents (X, Mini MK3, Pro MK3).
            LedMode::Launchpad
        } else if p.contains("launchpad") {
            // Launchpad Mini sans « MK3 », Launchpad S et premier Launchpad : ancien
            // protocole rouge / vert. Les autres (MK2, X, Pro, MK3) : palette.
            let modern = p.contains("mk3")
                || (!p.contains("mini")
                    && (p.contains("mk2") || p.contains("pro") || p.contains(" x")));
            if modern {
                LedMode::Launchpad
            } else {
                LedMode::LaunchpadClassic
            }
        } else if p.contains("apc40") && (p.contains("mkii") || p.contains("mk2")) {
            // APC40 mkII : même palette que le Launchpad, canal 1 = couleur fixe.
            LedMode::Launchpad
        } else if p.contains("apc") && p.contains("mk2") {
            // APC mini mk2, APC Key 25 mk2 : pads RVB, le canal règle la luminosité.
            LedMode::ApcMiniMk2
        } else if p.contains("apc") {
            LedMode::ApcMini
        } else if p.contains("push 2") {
            LedMode::Push2
        } else if [
            "fl studio fire",
            "mpd218",
            "minilab",
            "nanopad",
            "mpk mini",
            "launchkey",
            "lkmk",
        ]
        .iter()
        .any(|m| p.contains(m))
        {
            // Pads sans LED pilotables par de simples notes : rien à envoyer.
            LedMode::Off
        } else {
            LedMode::Generic
        }
    }
}

/// État lumineux voulu pour un pad.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LedState {
    Off,
    Loaded(u8),
    Playing(u8),
    /// Bouton de page du contrôleur : page affichée (`true`) ou autre page existante.
    Page(bool),
}

/// Couleurs de la palette Launchpad (vives / atténuées) pour les 12 couleurs de pads.
const LP_BRIGHT: [u8; 12] = [5, 9, 13, 17, 21, 29, 37, 45, 49, 53, 10, 3];
const LP_DIM: [u8; 12] = [7, 11, 15, 19, 23, 31, 39, 47, 51, 55, 11, 1];

/// Numérotation des pads d'un contrôleur (ligne `r` comptée depuis le haut, comme à
/// l'écran, colonne `c` depuis la gauche).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Note = `base` + `stride` × (ligne comptée depuis le bas) + colonne.
    BottomUp { base: u8, stride: u8 },
    /// Note = `base` + `stride` × (ligne comptée depuis le haut) + colonne.
    TopDown { base: u8, stride: u8 },
    /// Notes listées ligne par ligne depuis le haut.
    Table(&'static [u8]),
}

/// Boutons dédiés au changement de page (colonne de droite, boutons de scène…).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageButtons {
    first: u8,
    /// Écart de numéro entre deux boutons successifs (page 0 = `first`).
    step: i8,
    count: u8,
    cc: bool,
}

/// Disposition physique d'un contrôleur à pads : taille de la grille, note de chaque
/// pad et boutons servant à changer de page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridMap {
    /// Nom du modèle, affiché dans les réglages.
    pub name: &'static str,
    pub rows: usize,
    pub cols: usize,
    /// Canal MIDI des pads (0 = canal 1).
    channel: u8,
    layout: Layout,
    pages: Option<PageButtons>,
}

/// Nombre maximal de pages pilotables depuis le contrôleur.
const MAX_PAGE_BUTTONS: usize = 8;

impl GridMap {
    /// Disposition du contrôleur `port`, s'il fait partie des modèles reconnus.
    pub fn for_port(port: &str) -> Option<GridMap> {
        let p = port.to_lowercase();
        let has = |s: &str| p.contains(s);
        let lp_pages = |cc: bool| {
            Some(PageButtons {
                first: 89,
                step: -10,
                count: 8,
                cc,
            })
        };
        let novation = |name, pages| GridMap {
            name,
            rows: 8,
            cols: 8,
            channel: 0,
            // 11 en bas à gauche … 88 en haut à droite.
            layout: Layout::BottomUp {
                base: 11,
                stride: 10,
            },
            pages,
        };
        // Launchpad récents (mode « Programmer ») : X, Mini MK3, Pro MK3, premier Pro.
        if has("lpx") || has("launchpad x") {
            return Some(novation("Novation Launchpad X", lp_pages(true)));
        }
        if has("lpminimk3") || (has("launchpad") && has("mini") && has("mk3")) {
            return Some(novation("Novation Launchpad Mini MK3", lp_pages(true)));
        }
        if has("lppromk3") || (has("launchpad") && has("pro") && has("mk3")) {
            return Some(novation("Novation Launchpad Pro MK3", lp_pages(true)));
        }
        if has("launchpad pro") {
            return Some(novation("Novation Launchpad Pro", lp_pages(true)));
        }
        if has("launchpad") && has("mk2") && !has("mini") {
            // Disposition « Session » d'origine : boutons de droite en notes.
            return Some(novation("Novation Launchpad MK2", lp_pages(false)));
        }
        if has("launchpad") {
            // Launchpad S, Mini mk1 / mk2, premier Launchpad : disposition X-Y.
            return Some(GridMap {
                name: "Novation Launchpad S / Mini",
                rows: 8,
                cols: 8,
                channel: 0,
                layout: Layout::TopDown {
                    base: 0,
                    stride: 16,
                },
                pages: Some(PageButtons {
                    first: 8,
                    step: 16,
                    count: 8,
                    cc: false,
                }),
            });
        }
        // Akai APC Key 25 (mk1, mk2) et APC40 mkII : 5 × 8, 0 en bas à gauche, boutons
        // « Scene Launch » (82 à 86) à droite pour les pages.
        let apc5 = |name| GridMap {
            name,
            rows: 5,
            cols: 8,
            channel: 0,
            layout: Layout::BottomUp { base: 0, stride: 8 },
            pages: Some(PageButtons {
                first: 82,
                step: 1,
                count: 5,
                cc: false,
            }),
        };
        if has("apc key 25") || has("apc key25") {
            return Some(apc5(if has("mk2") {
                "Akai APC Key 25 mk2"
            } else {
                "Akai APC Key 25"
            }));
        }
        if has("apc40") && (has("mkii") || has("mk2")) {
            return Some(apc5("Akai APC40 mkII"));
        }
        if has("push 2") {
            // Ableton Push 2 : 36 en bas à gauche … 99 en haut à droite ; boutons de scène
            // (CC 43 en haut … 36 en bas) pour les pages.
            return Some(GridMap {
                name: "Ableton Push 2",
                rows: 8,
                cols: 8,
                channel: 0,
                layout: Layout::BottomUp {
                    base: 36,
                    stride: 8,
                },
                pages: Some(PageButtons {
                    first: 43,
                    step: -1,
                    count: 8,
                    cc: true,
                }),
            });
        }
        if has("fl studio fire") {
            // Akai Fire : 4 × 16, 54 en haut à gauche ; boutons « Mute / Solo » de chaque
            // ligne (0x24 à 0x27) pour les pages 1 à 4.
            return Some(GridMap {
                name: "Akai Fire",
                rows: 4,
                cols: 16,
                channel: 0,
                layout: Layout::TopDown {
                    base: 54,
                    stride: 16,
                },
                pages: Some(PageButtons {
                    first: 0x24,
                    step: 1,
                    count: 4,
                    cc: false,
                }),
            });
        }
        // Le port « DAW » des Novation (MIDIIN2 …) ne porte pas les pads en mode autonome.
        let daw_port = has("midiin2") || has("midiout2");
        if !daw_port
            && (has("lkmk3") || has("lkmk4") || (has("launchkey") && (has("mk3") || has("mk4"))))
        {
            // Novation Launchkey MK3 / Mini MK3 / MK4 : 2 × 8, disposition « Drum » d'origine
            // sur le canal 10.
            return Some(GridMap {
                name: "Novation Launchkey (pads)",
                rows: 2,
                cols: 8,
                channel: 9,
                layout: Layout::Table(&[
                    40, 41, 42, 43, 48, 49, 50, 51, //
                    36, 37, 38, 39, 44, 45, 46, 47,
                ]),
                pages: None,
            });
        }
        if has("mpd218") {
            // Akai MPD218 : 4 × 4, banque A, 36 en bas à gauche, canal 10.
            return Some(GridMap {
                name: "Akai MPD218",
                rows: 4,
                cols: 4,
                channel: 9,
                layout: Layout::BottomUp {
                    base: 36,
                    stride: 4,
                },
                pages: None,
            });
        }
        if (has("minilab 3") || has("minilab3")) && !(has("mcu") || has("din") || has("alv")) {
            // Arturia MiniLab 3 : 8 pads sur une ligne, banque A (36 à 43), canal 10.
            return Some(GridMap {
                name: "Arturia MiniLab 3",
                rows: 1,
                cols: 8,
                channel: 9,
                layout: Layout::BottomUp {
                    base: 36,
                    stride: 8,
                },
                pages: None,
            });
        }
        if has("nanopad2") {
            // Korg nanoPAD2 : 2 × 8, scène 1, notes en quinconce (impaires en haut).
            return Some(GridMap {
                name: "Korg nanoPAD2",
                rows: 2,
                cols: 8,
                channel: 0,
                layout: Layout::Table(&[
                    37, 39, 41, 43, 45, 47, 49, 51, //
                    36, 38, 40, 42, 44, 46, 48, 50,
                ]),
                pages: None,
            });
        }
        if has("mpk mini 3") || has("mpk mini mk3") || has("mpk mini iii") {
            // Akai MPK mini mk3 : 2 × 4, banque A, 36 en bas à gauche, canal 10.
            return Some(GridMap {
                name: "Akai MPK mini mk3",
                rows: 2,
                cols: 4,
                channel: 9,
                layout: Layout::BottomUp {
                    base: 36,
                    stride: 4,
                },
                pages: None,
            });
        }
        if has("apc") && has("mini") {
            let mk2 = has("mk2");
            return Some(GridMap {
                name: if mk2 {
                    "Akai APC mini mk2"
                } else {
                    "Akai APC mini"
                },
                rows: 8,
                cols: 8,
                channel: 0,
                // 0 en bas à gauche … 63 en haut à droite.
                layout: Layout::BottomUp { base: 0, stride: 8 },
                pages: Some(PageButtons {
                    first: if mk2 { 0x70 } else { 82 },
                    step: 1,
                    count: 8,
                    cc: false,
                }),
            });
        }
        None
    }

    /// Note du pad en ligne `r` (0 = en haut) et colonne `c` (0 = à gauche).
    pub fn pad_key(&self, r: usize, c: usize) -> MidiKey {
        let (r, c) = (r.min(self.rows - 1), c.min(self.cols - 1));
        let number = match self.layout {
            Layout::BottomUp { base, stride } => {
                base as usize + stride as usize * (self.rows - 1 - r) + c
            }
            Layout::TopDown { base, stride } => base as usize + stride as usize * r + c,
            Layout::Table(t) => t.get(r * self.cols + c).copied().unwrap_or(0) as usize,
        };
        MidiKey {
            kind: MidiKind::Note,
            channel: self.channel,
            number: (number & 0x7f) as u8,
        }
    }

    /// Pad (indice ligne × colonnes + colonne) qui envoie `key`.
    pub fn pad_at(&self, key: MidiKey) -> Option<usize> {
        (0..self.rows * self.cols).find(|&i| self.pad_key(i / self.cols, i % self.cols) == key)
    }

    /// Nombre de pages pilotables par des boutons du contrôleur.
    pub fn page_buttons(&self) -> usize {
        self.pages
            .map_or(0, |p| (p.count as usize).min(MAX_PAGE_BUTTONS))
    }

    /// Bouton de changement de page pour la page `page` (0 = la première).
    pub fn page_key(&self, page: usize) -> Option<MidiKey> {
        let b = self.pages?;
        if page >= self.page_buttons() {
            return None;
        }
        let number = b.first as i32 + b.step as i32 * page as i32;
        Some(MidiKey {
            kind: if b.cc { MidiKind::Cc } else { MidiKind::Note },
            channel: 0,
            number: (number & 0x7f) as u8,
        })
    }

    /// Page demandée par un bouton de changement de page.
    pub fn page_of(&self, key: MidiKey) -> Option<usize> {
        (0..self.page_buttons()).find(|&p| {
            self.page_key(p)
                .is_some_and(|k| k.number == key.number && k.kind == key.kind)
        })
    }

    /// Message à envoyer à la connexion pour que le contrôleur envoie bien cette
    /// disposition (mode « Programmer » des Launchpad récents).
    pub fn init_sysex(port: &str) -> Option<&'static [u8]> {
        let p = port.to_lowercase();
        if p.contains("lpx") || p.contains("launchpad x") {
            Some(&[0xF0, 0x00, 0x20, 0x29, 0x02, 0x0C, 0x0E, 0x01, 0xF7])
        } else if p.contains("lpminimk3") || (p.contains("mini") && p.contains("mk3")) {
            Some(&[0xF0, 0x00, 0x20, 0x29, 0x02, 0x0D, 0x0E, 0x01, 0xF7])
        } else if p.contains("lppromk3") || (p.contains("pro") && p.contains("mk3")) {
            Some(&[0xF0, 0x00, 0x20, 0x29, 0x02, 0x0E, 0x0E, 0x01, 0xF7])
        } else if p.contains("launchpad pro") {
            // Premier Launchpad Pro : disposition « Programmer ».
            Some(&[0xF0, 0x00, 0x20, 0x29, 0x02, 0x10, 0x2C, 0x03, 0xF7])
        } else if p.contains("apc40") {
            // APC40 mkII : sortie du mode « Generic » pour que l'ordinateur pilote les LED
            // (mode 0x41).
            Some(&[
                0xF0, 0x47, 0x7F, 0x29, 0x60, 0x00, 0x04, 0x41, 0x01, 0x00, 0x00, 0xF7,
            ])
        } else if p.contains("push 2") && (p.contains("midiin2") || p.contains("midiout2")) {
            // Push 2 sur son port « User » : passage en mode User (pads envoyés sur ce port).
            Some(&[0xF0, 0x00, 0x21, 0x1D, 0x01, 0x01, 0x0A, 0x01, 0xF7])
        } else {
            None
        }
    }
}

/// Message MIDI à envoyer pour un contrôle donné.
pub fn led_message(mode: LedMode, key: MidiKey, state: LedState) -> Option<[u8; 3]> {
    let ch = key.channel & 0x0f;
    if let LedState::Page(current) = state {
        // Boutons simples (colonne de droite) : allumé = page affichée.
        let value = match mode {
            LedMode::Off | LedMode::Auto => return None,
            LedMode::Launchpad => {
                if current {
                    3
                } else {
                    1
                }
            }
            LedMode::LaunchpadClassic => {
                if current {
                    48 + 12
                } else {
                    16 + 12
                }
            }
            LedMode::Generic => {
                if current {
                    127
                } else {
                    0
                }
            }
            LedMode::ApcMini | LedMode::ApcMiniMk2 => u8::from(current),
            LedMode::Push2 => {
                if current {
                    122
                } else {
                    0
                }
            }
        };
        let status = match key.kind {
            MidiKind::Note => 0x90 | ch,
            MidiKind::Cc => 0xb0 | ch,
        };
        return Some([status, key.number & 0x7f, value]);
    }
    let (status_ch, value) = match mode {
        LedMode::Off | LedMode::Auto => return None,
        LedMode::Generic => (
            ch,
            if matches!(state, LedState::Playing(_)) {
                127
            } else {
                0
            },
        ),
        LedMode::Launchpad => match state {
            LedState::Off | LedState::Page(_) => (ch, 0),
            // Canal 1 = couleur fixe (pas de clignotement ni de pulsation).
            LedState::Loaded(c) => (0, LP_DIM[c as usize % 12]),
            LedState::Playing(c) => (0, LP_BRIGHT[c as usize % 12]),
        },
        LedMode::ApcMini => match state {
            LedState::Off | LedState::Page(_) => (ch, 0),
            // 5 = jaune, 1 = vert (fixes ; les valeurs paires clignotent).
            LedState::Loaded(_) => (ch, 5),
            LedState::Playing(_) => (ch, 1),
        },
        // Protocole Akai des pads RVB (APC mini mk2, APC Key 25 mk2) : le canal de la
        // Note On règle le comportement (0-6 = luminosité 10 à 100 %, 7-10 = pulsation,
        // 11-15 = clignotement), la vélocité la couleur (même palette que le Launchpad).
        LedMode::ApcMiniMk2 => match state {
            LedState::Off | LedState::Page(_) => (6, 0),
            // Fixe et faible (25 %) quand le pad est chargé, fixe à 100 % pendant la lecture.
            LedState::Loaded(c) => (1, LP_BRIGHT[c as usize % 12]),
            LedState::Playing(c) => (6, LP_BRIGHT[c as usize % 12]),
        },
        // Push 2 : palette par défaut, 122 = blanc, 126 = vert (canal 1 = fixe).
        LedMode::Push2 => match state {
            LedState::Off | LedState::Page(_) => (0, 0),
            LedState::Loaded(_) => (0, 122),
            LedState::Playing(_) => (0, 126),
        },
        // Anciens Launchpad : vélocité = 16 × vert + rouge (0 à 3 chacun) + 12 (« copier
        // et effacer », affichage normal).
        LedMode::LaunchpadClassic => match state {
            LedState::Off | LedState::Page(_) => (ch, 12),
            LedState::Loaded(_) => (ch, 16 + 1 + 12),
            LedState::Playing(_) => (ch, 48 + 12),
        },
    };
    let status = match key.kind {
        MidiKind::Note => 0x90 | status_ch,
        MidiKind::Cc => 0xb0 | ch,
    };
    Some([status, key.number & 0x7f, value])
}

pub fn list_ports() -> Vec<String> {
    let Ok(m) = MidiInput::new("TracksPlus") else {
        return Vec::new();
    };
    m.ports()
        .iter()
        .filter_map(|p| m.port_name(p).ok())
        .collect()
}

fn parse(msg: &[u8]) -> Option<MidiEvent> {
    let status = *msg.first()?;
    let ch = status & 0x0f;
    let (kind, press) = match status & 0xf0 {
        0x90 => (MidiKind::Note, *msg.get(2)? > 0),
        0x80 => (MidiKind::Note, false),
        0xb0 => (MidiKind::Cc, *msg.get(2)? >= 64),
        _ => return None,
    };
    Some(MidiEvent {
        key: MidiKey {
            kind,
            channel: ch,
            number: *msg.get(1)?,
        },
        press,
    })
}

impl Midi {
    /// Ouvre la sortie LED : port donné, sinon celui qui correspond aux entrées connectées.
    pub fn connect_leds(&mut self, port: Option<&str>) {
        self.outs.clear();
        self.led_sent.clear();
        let wanted: Vec<String> = match port {
            Some(p) => vec![p.to_string()],
            None => {
                let keys: Vec<String> = self.connected.iter().map(|n| port_key(n)).collect();
                list_out_ports()
                    .into_iter()
                    .filter(|o| keys.contains(&port_key(o)))
                    .collect()
            }
        };
        for name in wanted {
            let Ok(out) = MidiOutput::new("TracksPlus") else {
                continue;
            };
            let Some(p) = out
                .ports()
                .into_iter()
                .find(|p| out.port_name(p).ok().as_deref() == Some(name.as_str()))
            else {
                continue;
            };
            if let Ok(c) = out.connect(&p, "tracksplus-led") {
                self.outs.push((name, c));
            }
        }
    }

    /// Passe les contrôleurs qui en ont besoin dans la disposition « Programmer ».
    pub fn init_layout(&mut self) {
        for (name, c) in &mut self.outs {
            if let Some(msg) = GridMap::for_port(name).and_then(|_| GridMap::init_sysex(name)) {
                let _ = c.send(msg);
            }
        }
        // Le contrôleur a pu effacer ses LED : on les renverra.
        self.led_sent.clear();
    }

    /// Remplace la table contrôle → pad. L'ancienne table (et ses sons) est libérée
    /// après avoir relâché le verrou : le thread MIDI n'attend jamais une libération.
    pub fn set_map(&self, map: HashMap<MidiKey, (u16, Arc<SampleData>, PadParams)>) {
        let old = std::mem::replace(&mut self.shared.lock().map, map);
        drop(old);
    }

    pub fn led_ports(&self) -> Vec<String> {
        self.outs.iter().map(|(n, _)| n.clone()).collect()
    }

    /// Envoie les changements d'état des LED (n'envoie rien si rien n'a changé).
    pub fn update_leds(&mut self, mode: LedMode, states: &[(MidiKey, LedState)]) {
        if self.outs.is_empty() {
            return;
        }
        // Contrôles présents, en ensemble : tout reste en O(n).
        let present: HashSet<MidiKey> = states.iter().map(|(k, _)| *k).collect();
        let mut msgs: Vec<(MidiKey, [u8; 3])> = Vec::new();
        // Le modèle est déduit du nom de la première sortie (un contrôleur à la fois).
        if let Some((name, _)) = self.outs.first() {
            let m = mode.resolve(name);
            for &(k, st) in states {
                if let Some(msg) = led_message(m, k, st) {
                    msgs.push((k, msg));
                }
            }
            // Contrôles qui ne sont plus associés à un pad : on les éteint.
            for k in self.led_sent.keys().filter(|k| !present.contains(*k)) {
                if let Some(msg) = led_message(m, *k, LedState::Off) {
                    msgs.push((*k, msg));
                }
            }
        }
        for (k, msg) in msgs {
            if self.led_sent.get(&k) == Some(&msg) {
                continue;
            }
            for (_, c) in &mut self.outs {
                let _ = c.send(&msg);
            }
            if present.contains(&k) {
                self.led_sent.insert(k, msg);
            } else {
                self.led_sent.remove(&k);
            }
        }
    }

    /// Éteint toutes les LED allumées (fermeture, changement de contrôleur).
    pub fn leds_off(&mut self, mode: LedMode) {
        let keys: Vec<MidiKey> = self.led_sent.keys().copied().collect();
        if let Some(name) = self.outs.first().map(|(n, _)| n.clone()) {
            let m = mode.resolve(&name);
            for k in keys {
                if let Some(msg) = led_message(m, k, LedState::Off) {
                    for (_, c) in &mut self.outs {
                        let _ = c.send(&msg);
                    }
                }
            }
        }
        self.led_sent.clear();
    }

    pub fn new() -> Self {
        let (events_tx, events) = unbounded();
        Self {
            shared: Arc::new(Mutex::new(MidiShared::default())),
            events,
            events_tx,
            conns: Vec::new(),
            connected: Vec::new(),
            error: None,
            outs: Vec::new(),
            led_sent: HashMap::new(),
        }
    }

    /// Connecte un port (ou tous si `ALL_PORTS`). `None` déconnecte.
    pub fn connect(&mut self, port: Option<&str>) {
        self.conns.clear();
        self.connected.clear();
        self.error = None;
        let Some(want) = port else {
            return;
        };
        let names = list_ports();
        for name in names {
            if want != ALL_PORTS && want != name {
                continue;
            }
            let Ok(input) = MidiInput::new("TracksPlus") else {
                continue;
            };
            let Some(p) = input
                .ports()
                .into_iter()
                .find(|p| input.port_name(p).ok().as_deref() == Some(name.as_str()))
            else {
                continue;
            };
            let shared = self.shared.clone();
            let ev = self.events_tx.clone();
            let res = input.connect(
                &p,
                "tracksplus-in",
                move |_, msg, _| {
                    let Some(e) = parse(msg) else {
                        return;
                    };
                    // Verrou tenu le moins longtemps possible : recherche du pad et envoi
                    // non bloquant de sa commande, puis le reste une fois relâché.
                    let ctx = {
                        let sh = shared.lock();
                        if let (Some(tx), Some((slot, sample, params))) =
                            (&sh.tx, sh.map.get(&e.key))
                        {
                            let cmd = if e.press {
                                PadCmd::Press {
                                    slot: *slot,
                                    sample: sample.clone(),
                                    params: *params,
                                }
                            } else {
                                PadCmd::Release { slot: *slot }
                            };
                            let _ = tx.try_send(cmd);
                        }
                        sh.ctx.clone()
                    };
                    let _ = ev.send(e);
                    if let Some(c) = ctx {
                        c.request_repaint();
                    }
                },
                (),
            );
            match res {
                Ok(c) => {
                    self.conns.push(c);
                    self.connected.push(name);
                }
                Err(e) => self.error = Some(format!("{name} : {e}")),
            }
        }
        if self.connected.is_empty() && self.error.is_none() && want != ALL_PORTS {
            self.error = Some(format!("« {want} » introuvable"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_messages() {
        let e = parse(&[0x91, 36, 100]).unwrap();
        assert_eq!(
            e.key,
            MidiKey {
                kind: MidiKind::Note,
                channel: 1,
                number: 36
            }
        );
        assert!(e.press);
        assert!(!parse(&[0x90, 36, 0]).unwrap().press);
        assert!(!parse(&[0x80, 36, 0]).unwrap().press);
        assert!(parse(&[0xb0, 7, 127]).unwrap().press);
        assert!(parse(&[0xf8]).is_none());
    }

    #[test]
    fn port_matching_and_leds() {
        assert_eq!(
            port_key("MIDIIN2 (LPX MIDI)"),
            port_key("MIDIOUT2 (LPX MIDI)")
        );
        assert_eq!(port_key("nanoPAD2 MIDI In"), port_key("nanoPAD2 MIDI Out"));
        assert_ne!(port_key("APC MINI"), port_key("Launchpad X"));
        assert_eq!(
            LedMode::Auto.resolve("Launchpad Mini MK3 LPMiniMK3 MIDI"),
            LedMode::Launchpad
        );
        assert_eq!(LedMode::Auto.resolve("APC MINI"), LedMode::ApcMini);
        // Modèles reconnus d'après le nom de leur port.
        assert_eq!(LedMode::Auto.resolve("APC mini mk2"), LedMode::ApcMiniMk2);
        assert_eq!(
            LedMode::Auto.resolve("APC mini mk2 Control"),
            LedMode::ApcMiniMk2
        );
        assert_eq!(LedMode::Auto.resolve("Launchpad MK2"), LedMode::Launchpad);
        assert_eq!(LedMode::Auto.resolve("Launchpad X"), LedMode::Launchpad);
        assert_eq!(LedMode::Auto.resolve("Launchpad Pro"), LedMode::Launchpad);
        assert_eq!(
            LedMode::Auto.resolve("Launchpad Mini"),
            LedMode::LaunchpadClassic
        );
        assert_eq!(
            LedMode::Auto.resolve("Launchpad S"),
            LedMode::LaunchpadClassic
        );
        assert_eq!(
            LedMode::Auto.resolve("nanoPAD2"),
            LedMode::Off,
            "pas de LED pilotables"
        );
        assert_eq!(LedMode::Auto.resolve("Arturia KeyStep"), LedMode::Generic);
        let k = MidiKey {
            kind: MidiKind::Note,
            channel: 0,
            number: 36,
        };
        assert_eq!(
            led_message(LedMode::Generic, k, LedState::Playing(0)),
            Some([0x90, 36, 127])
        );
        assert_eq!(
            led_message(LedMode::Launchpad, k, LedState::Playing(0)),
            Some([0x90, 36, 5])
        );
        assert_eq!(
            led_message(LedMode::Launchpad, k, LedState::Loaded(0)),
            Some([0x90, 36, 7])
        );
        assert_eq!(
            led_message(LedMode::ApcMini, k, LedState::Off),
            Some([0x90, 36, 0])
        );
        // APC mini mk2 : rouge (5) fixe à 25 % (canal 1) chargé, à 100 % (canal 6) en lecture.
        assert_eq!(
            led_message(LedMode::ApcMiniMk2, k, LedState::Loaded(0)),
            Some([0x91, 36, 5])
        );
        assert_eq!(
            led_message(LedMode::ApcMiniMk2, k, LedState::Playing(0)),
            Some([0x96, 36, 5])
        );
        // Aucun mode ne clignote ni ne pulse en lecture.
        for m in [LedMode::Launchpad, LedMode::ApcMiniMk2] {
            let st = led_message(m, k, LedState::Playing(4)).unwrap()[0];
            assert!((0x90..=0x96).contains(&st), "{m:?} : {st:#x}");
        }
        assert_eq!(
            led_message(LedMode::ApcMini, k, LedState::Playing(0)),
            Some([0x90, 36, 1])
        );
        assert_eq!(
            led_message(LedMode::ApcMiniMk2, k, LedState::Off),
            Some([0x96, 36, 0])
        );
        // Dispositions : coins de la grille et boutons de page, par modèle.
        let lp = GridMap::for_port("Launchpad MK2").unwrap();
        assert_eq!(
            lp.pad_key(0, 0).number,
            81,
            "en haut à gauche (vérifié sur un MK2)"
        );
        assert_eq!(lp.pad_key(0, 1).number, 82);
        assert_eq!(lp.pad_key(7, 7).number, 18);
        assert_eq!(lp.page_key(0).unwrap().number, 89);
        assert_eq!(lp.page_key(0).unwrap().kind, MidiKind::Note);
        assert_eq!((lp.rows, lp.cols), (8, 8));
        assert_eq!(lp.pad_at(lp.pad_key(3, 5)), Some(3 * 8 + 5));
        let lpx = GridMap::for_port("LPX MIDI").unwrap();
        assert_eq!(lpx.page_key(7).unwrap().kind, MidiKind::Cc);
        assert_eq!(
            lpx.page_of(MidiKey {
                kind: MidiKind::Cc,
                channel: 0,
                number: 19
            }),
            Some(7)
        );
        let apc = GridMap::for_port("APC MINI").unwrap();
        assert_eq!(apc.pad_key(7, 0).number, 0, "en bas à gauche");
        assert_eq!(apc.pad_key(0, 7).number, 63);
        assert_eq!(apc.page_key(0).unwrap().number, 82);
        let mk2 = GridMap::for_port("APC mini mk2").unwrap();
        assert_eq!(mk2.page_key(0).unwrap().number, 0x70);
        let old = GridMap::for_port("Launchpad Mini").unwrap();
        assert_eq!(old.pad_key(1, 2).number, 18);
        assert_eq!(old.page_key(1).unwrap().number, 24);
        // Nouveaux modèles : taille de la grille, coins, canal, boutons de page.
        let key25 = GridMap::for_port("MIDIIN2 (APC Key 25 mk2)").unwrap();
        assert_eq!((key25.rows, key25.cols), (5, 8));
        assert_eq!(key25.pad_key(0, 0).number, 32, "en haut à gauche");
        assert_eq!(key25.pad_key(4, 7).number, 7, "en bas à droite");
        assert_eq!(key25.page_buttons(), 5);
        assert_eq!(key25.page_key(4).unwrap().number, 86);
        assert!(key25.page_key(5).is_none());
        assert_eq!(LedMode::Auto.resolve("APC Key 25 mk2"), LedMode::ApcMiniMk2);
        assert_eq!(LedMode::Auto.resolve("APC Key 25"), LedMode::ApcMini);
        let apc40 = GridMap::for_port("APC40 mkII").unwrap();
        assert_eq!(apc40.pad_key(0, 0).number, 32);
        assert_eq!(LedMode::Auto.resolve("APC40 mkII"), LedMode::Launchpad);
        assert!(GridMap::init_sysex("APC40 mkII").is_some());
        let push = GridMap::for_port("Ableton Push 2").unwrap();
        assert_eq!(push.pad_key(7, 0).number, 36);
        assert_eq!(push.pad_key(0, 7).number, 99);
        assert_eq!(
            push.page_key(0).unwrap(),
            MidiKey {
                kind: MidiKind::Cc,
                channel: 0,
                number: 43
            }
        );
        assert!(
            GridMap::init_sysex("Ableton Push 2").is_none(),
            "port Live : rien"
        );
        assert!(GridMap::init_sysex("MIDIIN2 (Ableton Push 2)").is_some());
        let fire = GridMap::for_port("FL STUDIO FIRE").unwrap();
        assert_eq!((fire.rows, fire.cols), (4, 16));
        assert_eq!(fire.pad_key(0, 0).number, 54);
        assert_eq!(fire.pad_key(3, 15).number, 117);
        assert_eq!(LedMode::Auto.resolve("FL STUDIO FIRE"), LedMode::Off);
        let lk = GridMap::for_port("LKMK3 MIDI").unwrap();
        assert_eq!((lk.rows, lk.cols), (2, 8));
        assert_eq!(
            lk.pad_key(1, 0),
            MidiKey {
                kind: MidiKind::Note,
                channel: 9,
                number: 36
            }
        );
        assert_eq!(lk.pad_key(0, 7).number, 51);
        assert!(
            GridMap::for_port("MIDIIN2 (LKMK3 MIDI)").is_none(),
            "port DAW"
        );
        assert!(GridMap::for_port("Launchkey Mini MK3 MIDI").is_some());
        let mpd = GridMap::for_port("MPD218").unwrap();
        assert_eq!(mpd.pad_key(3, 0).number, 36);
        assert_eq!(mpd.pad_key(0, 3).number, 51);
        assert_eq!(mpd.pad_key(0, 0).channel, 9);
        assert_eq!(mpd.page_buttons(), 0);
        let ml = GridMap::for_port("MiniLab 3 MIDI").unwrap();
        assert_eq!((ml.rows, ml.cols), (1, 8));
        assert_eq!(ml.pad_key(0, 7).number, 43);
        assert!(GridMap::for_port("MiniLab 3 MCU").is_none());
        let nano = GridMap::for_port("nanoPAD2").unwrap();
        assert_eq!(nano.pad_key(0, 0).number, 37);
        assert_eq!(nano.pad_key(1, 7).number, 50);
        assert_eq!(nano.pad_at(nano.pad_key(1, 3)), Some(8 + 3));
        let mpk = GridMap::for_port("MPK mini 3").unwrap();
        assert_eq!((mpk.rows, mpk.cols), (2, 4));
        assert_eq!(mpk.pad_key(1, 0).number, 36);
        assert_eq!(mpk.pad_key(0, 3).number, 43);
        assert!(GridMap::for_port("Arturia KeyStep").is_none());
        assert!(GridMap::init_sysex("Launchpad MK2").is_none());
        assert!(GridMap::init_sysex("LPMiniMK3 MIDI").is_some());
        // Ancien Launchpad : ambre faible (29) chargé, vert plein (60) en lecture.
        assert_eq!(
            led_message(LedMode::LaunchpadClassic, k, LedState::Loaded(3)),
            Some([0x90, 36, 29])
        );
        assert_eq!(
            led_message(LedMode::LaunchpadClassic, k, LedState::Playing(3)),
            Some([0x90, 36, 60])
        );
        let cc = MidiKey {
            kind: MidiKind::Cc,
            channel: 1,
            number: 20,
        };
        assert_eq!(
            led_message(LedMode::Generic, cc, LedState::Playing(0)),
            Some([0xb1, 20, 127])
        );
    }
}
