//! Contrôleurs MIDI USB (Launchpad, APC, nanoPAD, Stream Deck en mode MIDI…).
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
use std::collections::HashMap;
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
    /// Novation Launchpad : couleur du pad, pulsation pendant la lecture.
    Launchpad,
    /// Akai APC mini : jaune = chargé, vert clignotant = en lecture.
    ApcMini,
}

impl LedMode {
    pub const ALL: [LedMode; 5] = [
        LedMode::Auto,
        LedMode::Off,
        LedMode::Generic,
        LedMode::Launchpad,
        LedMode::ApcMini,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LedMode::Auto => "Automatique",
            LedMode::Off => "Désactivées",
            LedMode::Generic => "Générique (on/off)",
            LedMode::Launchpad => "Novation Launchpad",
            LedMode::ApcMini => "Akai APC mini",
        }
    }

    /// Résout le mode automatique d'après le nom du port.
    pub fn resolve(self, port: &str) -> LedMode {
        if self != LedMode::Auto {
            return self;
        }
        let p = port.to_lowercase();
        if p.contains("launchpad")
            || p.contains("lpx")
            || p.contains("lpmini")
            || p.contains("lppro")
        {
            LedMode::Launchpad
        } else if p.contains("apc") {
            LedMode::ApcMini
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
}

/// Couleurs de la palette Launchpad (vives / atténuées) pour les 12 couleurs de pads.
const LP_BRIGHT: [u8; 12] = [5, 9, 13, 17, 21, 29, 37, 45, 49, 53, 10, 3];
const LP_DIM: [u8; 12] = [7, 11, 15, 19, 23, 31, 39, 47, 51, 55, 11, 1];

/// Message MIDI à envoyer pour un contrôle donné.
pub fn led_message(mode: LedMode, key: MidiKey, state: LedState) -> Option<[u8; 3]> {
    let ch = key.channel & 0x0f;
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
            LedState::Off => (ch, 0),
            LedState::Loaded(c) => (0, LP_DIM[c as usize % 12]),
            // Canal 3 = pulsation sur les Launchpad récents.
            LedState::Playing(c) => (2, LP_BRIGHT[c as usize % 12]),
        },
        LedMode::ApcMini => match state {
            LedState::Off => (ch, 0),
            LedState::Loaded(_) => (ch, 5),
            LedState::Playing(_) => (ch, 2),
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

    pub fn led_ports(&self) -> Vec<String> {
        self.outs.iter().map(|(n, _)| n.clone()).collect()
    }

    /// Envoie les changements d'état des LED (n'envoie rien si rien n'a changé).
    pub fn update_leds(&mut self, mode: LedMode, states: &[(MidiKey, LedState)]) {
        if self.outs.is_empty() {
            return;
        }
        let mut msgs: Vec<(MidiKey, [u8; 3])> = Vec::new();
        for (name, _) in &self.outs {
            let m = mode.resolve(name);
            for &(k, st) in states {
                if let Some(msg) = led_message(m, k, st) {
                    msgs.push((k, msg));
                }
            }
            // Contrôles qui ne sont plus associés à un pad : on les éteint.
            for (k, _) in self
                .led_sent
                .iter()
                .filter(|(k, _)| !states.iter().any(|(s, _)| s == *k))
            {
                if let Some(msg) = led_message(m, *k, LedState::Off) {
                    msgs.push((*k, msg));
                }
            }
            break;
        }
        let present: Vec<MidiKey> = states.iter().map(|(k, _)| *k).collect();
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
                    let sh = shared.lock();
                    if let (Some(tx), Some((slot, sample, params))) = (&sh.tx, sh.map.get(&e.key)) {
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
                    let _ = ev.send(e);
                    if let Some(c) = &sh.ctx {
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
            Some([0x92, 36, 5])
        );
        assert_eq!(
            led_message(LedMode::Launchpad, k, LedState::Loaded(0)),
            Some([0x90, 36, 7])
        );
        assert_eq!(
            led_message(LedMode::ApcMini, k, LedState::Off),
            Some([0x90, 36, 0])
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
