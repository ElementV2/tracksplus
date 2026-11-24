//! Contrôleurs MIDI USB (Launchpad, APC, nanoPAD, Stream Deck en mode MIDI…).
//!
//! Les messages déclenchent les pads directement depuis le thread MIDI vers le
//! moteur audio (latence minimale, même si la fenêtre est réduite), et sont aussi
//! transmis à l'interface pour l'apprentissage (MIDI learn) et l'affichage.

use crate::sampler::{PadCmd, PadParams, SampleData};
use crate::soundboard::{MidiKey, MidiKind};
use crossbeam_channel::{Receiver, Sender, unbounded};
use midir::{MidiInput, MidiInputConnection};
use parking_lot::Mutex;
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
    pub fn new() -> Self {
        let (events_tx, events) = unbounded();
        Self {
            shared: Arc::new(Mutex::new(MidiShared::default())),
            events,
            events_tx,
            conns: Vec::new(),
            connected: Vec::new(),
            error: None,
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
}
