# TracksPlus

Enregistreur **multipiste** et **soundboard** dans une seule application, sobre et épurée.
Pensé pour le live : Dante Virtual Soundcard (jusqu'à 64 entrées et plus), ASIO, CoreAudio.

| Multipiste | Soundboard |
| --- | --- |
| ![Multipiste](docs/multipiste.png) | ![Soundboard](docs/soundboard.png) |

## Onglet Multipiste

- **Carte son** : pilote (ASIO / WASAPI, CoreAudio, ALSA / JACK), entrée, sortie, fréquence,
  buffer. Configuration mémorisée : on retrouve sa carte Dante au lancement.
- **Pistes** : une piste = une entrée mono. Sélecteur d'entrée en grille (1 à 64+, entrées déjà
  utilisées signalées). « 1 piste / entrée » crée d'un coup les pistes d'un profil Dante.
  Armement, **M**ute / **S**olo pour l'écoute, renommage direct.
- **Vumètres pro** : crête façon IEC 60268-18 (échelle dB non linéaire, plus précise près de 0),
  zones vert / jaune (−12 dB) / rouge (−3 dB), maintien de crête, valeur crête max en dB et
  voyant de saturation (clic pour remettre à zéro). Un vumètre par piste, un **vumètre master**
  stéréo, et un **bandeau de vumètres** affichant toutes les entrées d'un coup.
- **Enregistrement** : WAV mono 24 bits ou 32 bits flottant écrit directement sur disque
  (en-têtes mis à jour toutes les 2 s : fichiers lisibles même après un crash), sauvegarde
  automatique à chaque stop, temps d'enregistrement restant sur le disque, écoute des pistes
  existantes pendant la prise.
- **Navigation** : la timeline **suit** la tête de lecture en continu ; si vous faites défiler
  pendant la lecture ou l'enregistrement, le suivi se met en pause 3 s puis recadre
  automatiquement (y compris verticalement sur les pistes en cours d'enregistrement).
  Clic sur la règle / une piste = se déplacer (même en lecture), glisser = sélectionner une
  plage, double-clic sur une prise = sélectionner la prise, barre de défilement horizontale.
- **Repères** (touche M, même pendant l'enregistrement) : renommer, supprimer, sélectionner
  jusqu'au repère suivant, flèches ← / → pour sauter de repère en repère.
- **Export** : pistes **choisies une à une** (ou la sélection de la timeline), toute la
  session ou **seulement la plage sélectionnée**, pistes séparées et/ou mix stéréo,
  44.1 / 48 / 88.2 / 96 kHz, 16 bits (dither TPDF) / 24 bits / 32 bits entier / 32 bits flottant.

## Onglet Soundboard

- **Pads** en grille (4×2 à 8×8), jusqu'à 8 pages. Glissez-déposez des fichiers (WAV, AIFF,
  MP3, AAC/M4A, FLAC, OGG) ou cliquez sur un pad vide. Déclenchement à l'appui (pas au
  relâchement) pour la réactivité.
- **Modes** : One shot (relance), Play / Stop, Maintien (joue tant que le pad est tenu),
  lecture en boucle.
- **Éditeur** : forme d'onde zoomable, poignées **début / fin**, poignées de **fondu d'entrée
  et de sortie** (courbes douces), gain en dB, couleur, nom.
- **Contrôleur USB** : choix du port MIDI (ou tous), **MIDI learn** par pad (notes et CC),
  déclenchement direct dans le thread audio : fonctionne même fenêtre réduite.
  **Touches clavier** assignables par pad (pratique aussi avec un Stream Deck).
- **Sortie dédiée** (ex. sorties 3–4 de la Dante), volume général, vumètre stéréo des pads,
  **Tout arrêter** avec fondu (Échap). Le soundboard est sauvegardé automatiquement et peut
  être enregistré / rechargé comme fichier.

## Raccourcis

| Touche | Multipiste | Soundboard |
| --- | --- | --- |
| Espace | Lecture / Stop | |
| R | Enregistrer / Stop | |
| M | Ajouter un repère | |
| ← / → | Repère précédent / suivant | |
| Origine / Entrée | Début (ou début de la sélection) | |
| F | Tout afficher | |
| Échap | Effacer la sélection | Tout arrêter |
| Ctrl + molette | Zoom | Zoom de l'éditeur |
| Maj + molette | Défiler dans le temps | |
| Ctrl + 1 / Ctrl + 2 | Changer d'onglet | Changer d'onglet |
| Ctrl + S / Ctrl + E | Enregistrer / Exporter | |

## Utilisation avec Dante

1. Réglages carte son (pastille en haut à droite) → pilote **ASIO** (Windows) ou **CoreAudio**
   (macOS) → **Dante Virtual Soundcard**. Fréquence identique à Dante Controller.
2. Multipiste : « 1 piste / entrée », renommez, armez, ⏺.
3. Soundboard : choisissez la paire de sorties Dante dédiée aux pads.

## Compiler

Prérequis : [Rust](https://rustup.rs) stable.

```sh
# Windows avec ASIO (Dante VSC) — télécharge le SDK Steinberg, nécessite LLVM/clang
cargo build --release --features asio
# macOS / Linux (Linux : libasound2-dev)
cargo build --release
```

La CI GitHub (`.github/workflows/build.yml`) produit un `.exe` Windows (ASIO) et une `.app`
macOS universelle. Police : [Inter](https://rsms.me/inter/) (licence OFL, `assets/fonts`).

## Architecture

| Fichier | Rôle |
| --- | --- |
| `src/audio.rs` | Périphériques, flux cpal, callbacks temps réel (sans allocation ni verrou), vumètres |
| `src/sampler.rs` | Moteur des pads (voix, fondus, modes) exécuté dans le callback de sortie |
| `src/soundboard.rs` | Modèle du soundboard, décodage (symphonia) et rééchantillonnage des sons |
| `src/midi.rs` | Contrôleurs MIDI, déclenchement direct vers le moteur audio |
| `src/recorder.rs` / `src/player.rs` | Threads disque : enregistrement et lecture multipiste |
| `src/render.rs` / `src/export.rs` | Lecture des prises, formes d'onde, export |
| `src/project.rs` | Session, pistes, prises, repères |
| `src/app.rs`, `src/ui/*` | Interface (egui) : thème, widgets, onglets |
