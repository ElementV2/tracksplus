//! Raccourcis clavier paramétrables (Réglages → Raccourcis), enregistrés dans
//! `shortcuts.json` du dossier des réglages. Seules les actions changées y sont écrites :
//! les autres suivent les valeurs par défaut (une nouvelle action arrive donc avec la
//! sienne). Une combinaison n'appartient qu'à une action à la fois.

use eframe::egui::{self, Key, Modifiers};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const PLAY: &str = "play";
pub const RECORD: &str = "record";
pub const GO_START: &str = "go_start";
pub const GO_END: &str = "go_end";
pub const LOOP: &str = "loop";
pub const PREV: &str = "prev";
pub const NEXT: &str = "next";
pub const MARKERS: [&str; 9] = [
    "marker1", "marker2", "marker3", "marker4", "marker5", "marker6", "marker7", "marker8",
    "marker9",
];
pub const ZOOM_FIT: &str = "zoom_fit";
pub const FOLLOW: &str = "follow";
pub const METERS: &str = "meters";
pub const SNAP: &str = "snap";
pub const SETTINGS: &str = "settings";
pub const ADD_MARKER: &str = "add_marker";
pub const MARK_IN: &str = "mark_in";
pub const MARK_OUT: &str = "mark_out";
pub const SPLIT: &str = "split";
pub const DELETE: &str = "delete";
pub const SELECT_ALL: &str = "select_all";
pub const DESELECT: &str = "deselect";
pub const UNDO: &str = "undo";
pub const REDO: &str = "redo";
pub const SAVE: &str = "save";
pub const EXPORT: &str = "export";
pub const TAB_MULTI: &str = "tab_multi";
pub const TAB_BOARD: &str = "tab_board";
pub const PADS_STOP: &str = "pads_stop";
pub const PAD_DELETE: &str = "pad_delete";

/// Une action : identifiant (fichier), libellé, groupe affiché, combinaisons par défaut
/// (noms egui, « Ctrl+Shift+Z »), et si la touche maintenue la répète.
pub struct Def {
    pub id: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub defaults: &'static [&'static str],
    pub repeat: bool,
}

const fn def(
    id: &'static str,
    label: &'static str,
    group: &'static str,
    defaults: &'static [&'static str],
    repeat: bool,
) -> Def {
    Def {
        id,
        label,
        group,
        defaults,
        repeat,
    }
}

pub const DEFS: &[Def] = &[
    def(PLAY, "Lecture / arrêt", "Transport", &["Space"], false),
    def(RECORD, "Enregistrer", "Transport", &["R"], false),
    def(GO_START, "Début (ou début de la plage)", "Transport", &["Home", "Enter"], false),
    def(GO_END, "Fin de la session", "Transport", &["End"], false),
    def(LOOP, "Boucle", "Transport", &["L"], false),
    def(PREV, "Repère ou bord de plage précédent", "Navigation", &["ArrowLeft"], true),
    def(NEXT, "Repère ou bord de plage suivant", "Navigation", &["ArrowRight"], true),
    def(MARKERS[0], "Aller au repère 1", "Navigation", &["1"], false),
    def(MARKERS[1], "Aller au repère 2", "Navigation", &["2"], false),
    def(MARKERS[2], "Aller au repère 3", "Navigation", &["3"], false),
    def(MARKERS[3], "Aller au repère 4", "Navigation", &["4"], false),
    def(MARKERS[4], "Aller au repère 5", "Navigation", &["5"], false),
    def(MARKERS[5], "Aller au repère 6", "Navigation", &["6"], false),
    def(MARKERS[6], "Aller au repère 7", "Navigation", &["7"], false),
    def(MARKERS[7], "Aller au repère 8", "Navigation", &["8"], false),
    def(MARKERS[8], "Aller au repère 9", "Navigation", &["9"], false),
    def(ZOOM_FIT, "Tout afficher", "Navigation", &["F"], false),
    def(FOLLOW, "Suivre la lecture", "Affichage", &["Ctrl+F"], false),
    def(METERS, "Mixage et vumètres", "Affichage", &["V"], false),
    def(SNAP, "Magnétisme", "Affichage", &["G"], false),
    def(ADD_MARKER, "Poser un repère", "Montage", &["M"], false),
    def(MARK_IN, "Début de plage", "Montage", &["I"], false),
    def(MARK_OUT, "Fin de plage", "Montage", &["O"], false),
    def(SPLIT, "Couper à la tête de lecture", "Montage", &["S"], false),
    def(DELETE, "Supprimer", "Montage", &["Delete", "Backspace"], false),
    def(SELECT_ALL, "Tout sélectionner", "Montage", &["Ctrl+A"], false),
    def(DESELECT, "Tout désélectionner", "Montage", &["Escape"], false),
    def(UNDO, "Annuler", "Montage", &["Ctrl+Z"], true),
    def(REDO, "Rétablir", "Montage", &["Ctrl+Shift+Z", "Ctrl+Y"], true),
    def(SAVE, "Enregistrer la session", "Session", &["Ctrl+S"], false),
    def(EXPORT, "Exporter", "Session", &["Ctrl+E"], false),
    def(TAB_MULTI, "Onglet Multipiste", "Session", &["Ctrl+1"], false),
    def(TAB_BOARD, "Onglet Soundboard", "Session", &["Ctrl+2"], false),
    def(SETTINGS, "Réglages", "Session", &["Ctrl+Comma"], false),
    def(PADS_STOP, "Arrêter tous les pads", "Soundboard", &["Escape"], false),
    def(PAD_DELETE, "Supprimer le pad sélectionné", "Soundboard", &["Delete"], false),
];

/// Actions de l'onglet Soundboard : leurs touches peuvent reprendre celles du multipiste
/// (onglets différents, jamais actifs ensemble).
fn board_scope(id: &str) -> bool {
    matches!(id, PADS_STOP | PAD_DELETE)
}

/// Gestes de souris (non paramétrables), affichés pour mémoire.
pub const MOUSE: &[(&str, &str)] = &[
    ("Maj + glisser", "Inverser le magnétisme (prises, tête, plage)"),
    ("Ctrl + clic sur la règle", "Début de plage"),
    ("Alt + clic sur la règle", "Fin de plage"),
    ("Ctrl / Maj + clic sur une prise", "Ajouter à la sélection"),
    ("Maj + glisser un fader", "Réglage fin"),
    ("Double-clic sur un fader", "0 dB"),
    ("Double-clic sur un panoramique", "Centre"),
    ("Double-clic sur un nom de piste", "Renommer"),
    ("Ctrl + clic sur un pad", "Jouer le pad"),
    ("Ctrl + molette (éditeur de pad)", "Zoom"),
];

/// Une combinaison : touche et modificateurs exacts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Combo {
    pub key: Key,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Combo {
    pub fn new(key: Key, m: Modifiers) -> Self {
        Self {
            key,
            ctrl: m.command || m.ctrl,
            shift: m.shift,
            alt: m.alt,
        }
    }

    /// « Ctrl+Shift+Z » (noms egui) → combinaison.
    pub fn parse(s: &str) -> Option<Self> {
        let mut c = Self {
            key: Key::A,
            ctrl: false,
            shift: false,
            alt: false,
        };
        let mut key = None;
        for part in s.split('+').map(str::trim) {
            match part {
                "Ctrl" => c.ctrl = true,
                "Shift" => c.shift = true,
                "Alt" => c.alt = true,
                k => key = Some(Key::from_name(k)?),
            }
        }
        c.key = key?;
        Some(c)
    }

    /// Forme enregistrée (noms egui, stables d'une version à l'autre).
    pub fn store(&self) -> String {
        let mut s = String::new();
        for (on, m) in [(self.ctrl, "Ctrl+"), (self.shift, "Shift+"), (self.alt, "Alt+")] {
            if on {
                s.push_str(m);
            }
        }
        s + self.key.name()
    }

    /// Forme affichée, en français (« Ctrl+Maj+Z », « Espace », « Suppr »).
    pub fn label(&self) -> String {
        let mut s = String::new();
        for (on, m) in [(self.ctrl, "Ctrl+"), (self.shift, "Maj+"), (self.alt, "Alt+")] {
            if on {
                s.push_str(m);
            }
        }
        s + key_label(self.key)
    }

    /// Même combinaison qu'un appui reçu (modificateurs exacts). Les chiffres valent aussi
    /// par leur position physique : sur un clavier AZERTY, la touche « 1 » sans Maj donne
    /// « & », elle doit quand même aller au repère 1.
    fn hit(&self, key: Key, physical: Option<Key>, m: Modifiers) -> bool {
        let same_key = key == self.key || (is_digit(self.key) && physical == Some(self.key));
        same_key && (m.command || m.ctrl) == self.ctrl && m.shift == self.shift && m.alt == self.alt
    }
}

fn is_digit(k: Key) -> bool {
    matches!(
        k,
        Key::Num0
            | Key::Num1
            | Key::Num2
            | Key::Num3
            | Key::Num4
            | Key::Num5
            | Key::Num6
            | Key::Num7
            | Key::Num8
            | Key::Num9
    )
}

/// Nom d'une touche à l'écran.
pub fn key_label(k: Key) -> &'static str {
    match k {
        Key::Space => "Espace",
        Key::Enter => "Entrée",
        Key::Home => "Début",
        Key::End => "Fin",
        Key::Delete => "Suppr",
        Key::Backspace => "Retour arrière",
        Key::Escape => "Échap",
        Key::Tab => "Tab",
        Key::Insert => "Inser",
        Key::PageUp => "Page préc.",
        Key::PageDown => "Page suiv.",
        Key::ArrowLeft => "←",
        Key::ArrowRight => "→",
        Key::ArrowUp => "↑",
        Key::ArrowDown => "↓",
        k => k.symbol_or_name(),
    }
}

/// Touches qu'on ne peut pas attribuer seules (modificateurs, ou utilisées partout).
pub fn assignable(c: &Combo) -> bool {
    !matches!(c.key, Key::Tab) || c.ctrl || c.alt
}

#[derive(Clone, Debug, PartialEq)]
pub struct Keymap {
    map: BTreeMap<&'static str, Vec<Combo>>,
}

impl Default for Keymap {
    fn default() -> Self {
        let map = DEFS
            .iter()
            .map(|d| (d.id, d.defaults.iter().filter_map(|s| Combo::parse(s)).collect()))
            .collect();
        Self { map }
    }
}

impl Keymap {
    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("TracksPlus").join("shortcuts.json"))
    }

    /// Raccourcis enregistrés (les valeurs par défaut pour le reste, ou si le fichier est
    /// absent ou illisible).
    pub fn load() -> Self {
        let text = Self::path().and_then(|p| std::fs::read_to_string(p).ok());
        Self::from_json(text.as_deref().unwrap_or(""))
    }

    fn from_json(text: &str) -> Self {
        let mut km = Self::default();
        let saved: BTreeMap<String, Vec<String>> = serde_json::from_str(text).unwrap_or_default();
        for d in DEFS {
            if let Some(v) = saved.get(d.id) {
                km.map
                    .insert(d.id, v.iter().filter_map(|s| Combo::parse(s)).take(2).collect());
            }
        }
        km
    }

    fn to_json(&self) -> String {
        let defaults = Self::default();
        let changed: BTreeMap<&str, Vec<String>> = self
            .map
            .iter()
            .filter(|(id, v)| defaults.map.get(*id) != Some(v))
            .map(|(id, v)| (*id, v.iter().map(Combo::store).collect()))
            .collect();
        serde_json::to_string_pretty(&changed).unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let p = Self::path().ok_or_else(|| anyhow::anyhow!("dossier des réglages introuvable"))?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        crate::project::write_atomic(&p, self.to_json().as_bytes())
    }

    pub fn combos(&self, id: &str) -> &[Combo] {
        self.map.get(id).map_or(&[], |v| v.as_slice())
    }

    pub fn is_default(&self, id: &str) -> bool {
        Self::default().map.get(id) == self.map.get(id)
    }

    /// Premier raccourci d'une action, pour les bulles d'aide (vide si aucun).
    pub fn label(&self, id: &str) -> String {
        self.combos(id).first().map(Combo::label).unwrap_or_default()
    }

    /// « Texte (raccourci) », ou le texte seul si l'action n'a pas de raccourci.
    pub fn tip(&self, text: &str, id: &str) -> String {
        match self.label(id) {
            l if l.is_empty() => text.to_string(),
            l => format!("{text} ({l})"),
        }
    }

    /// Appui sur un raccourci de l'action : consommé (une seule action le reçoit).
    pub fn pressed(&self, i: &mut egui::InputState, id: &str) -> bool {
        let repeat_ok = DEFS.iter().any(|d| d.id == id && d.repeat);
        let combos = self.combos(id);
        let mut hit = false;
        i.events.retain(|e| match e {
            egui::Event::Key {
                key,
                physical_key,
                pressed: true,
                repeat,
                modifiers,
            } if !hit
                && (repeat_ok || !*repeat)
                && combos.iter().any(|c| c.hit(*key, *physical_key, *modifiers)) =>
            {
                hit = true;
                false
            }
            _ => true,
        });
        hit
    }

    /// Cette touche (et ces modificateurs) est-elle un raccourci de l'action ?
    pub fn is(&self, id: &str, key: Key, m: Modifiers) -> bool {
        self.combos(id).iter().any(|c| c.hit(key, None, m))
    }

    /// Attribue `c` à l'action (`slot` 0 ou 1). Retiré des actions qui l'avaient (dans le
    /// même onglet) : renvoie leurs libellés.
    pub fn set(&mut self, id: &'static str, slot: usize, c: Combo) -> Vec<&'static str> {
        let mut taken = Vec::new();
        for d in DEFS {
            if d.id == id || board_scope(d.id) != board_scope(id) {
                continue;
            }
            if let Some(v) = self.map.get_mut(d.id) {
                let before = v.len();
                v.retain(|x| *x != c);
                if v.len() < before {
                    taken.push(d.label);
                }
            }
        }
        let v = self.map.entry(id).or_default();
        v.retain(|x| *x != c);
        if slot < v.len() {
            v[slot] = c;
        } else {
            v.push(c);
        }
        v.truncate(2);
        taken
    }

    /// Retire le raccourci `slot` de l'action.
    pub fn clear(&mut self, id: &str, slot: usize) {
        if let Some(v) = self.map.get_mut(id)
            && slot < v.len()
        {
            v.remove(slot);
        }
    }

    pub fn reset(&mut self, id: &'static str) {
        if let Some(v) = Self::default().map.remove(id) {
            self.map.insert(id, v);
        }
    }

    pub fn reset_all(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_never_collide() {
        let km = Keymap::default();
        for d in DEFS {
            assert_eq!(km.combos(d.id).len(), d.defaults.len(), "{}", d.id);
        }
        // Une combinaison par action dans un même onglet.
        for a in DEFS {
            for b in DEFS {
                if a.id < b.id && board_scope(a.id) == board_scope(b.id) {
                    for c in km.combos(a.id) {
                        assert!(!km.combos(b.id).contains(c), "{} / {}", a.id, b.id);
                    }
                }
            }
        }
    }

    #[test]
    fn exact_modifiers_and_azerty_digits() {
        let km = Keymap::default();
        let none = Modifiers::NONE;
        assert!(km.is(SPLIT, Key::S, none));
        assert!(!km.is(SPLIT, Key::S, Modifiers::SHIFT));
        assert!(km.is(UNDO, Key::Z, Modifiers::COMMAND));
        assert!(!km.is(UNDO, Key::Z, Modifiers::COMMAND | Modifiers::SHIFT));
        assert!(km.is(REDO, Key::Z, Modifiers::COMMAND | Modifiers::SHIFT));
        // AZERTY : « & » sur la touche du 1.
        let c = km.combos(MARKERS[0])[0];
        assert!(c.hit(Key::Exclamationmark, Some(Key::Num1), none));
        // Lettres : jamais par position (Q d'un AZERTY n'est pas A).
        let a = km.combos(SELECT_ALL)[0];
        assert!(!a.hit(Key::Q, Some(Key::A), Modifiers::COMMAND));
    }

    #[test]
    fn rebinding_moves_the_combo_and_survives_a_save() {
        let mut km = Keymap::default();
        let c = Combo::parse("M").unwrap();
        let taken = km.set(SPLIT, 0, c);
        assert_eq!(taken, vec!["Poser un repère"]);
        assert!(km.combos(ADD_MARKER).is_empty());
        assert_eq!(km.combos(SPLIT), &[c]);
        // Échap reste aux deux onglets.
        assert_eq!(km.combos(PADS_STOP), km.combos(DESELECT));
        let back = Keymap::from_json(&km.to_json());
        assert_eq!(back, km);
        // Seules les actions changées sont écrites.
        assert!(!km.to_json().contains("\"play\""));
        km.reset_all();
        assert_eq!(km, Keymap::default());
        assert_eq!(Combo::parse("Ctrl+Shift+Z").unwrap().label(), "Ctrl+Maj+Z");
        assert_eq!(Combo::parse("Space").unwrap().label(), "Espace");
    }
}
