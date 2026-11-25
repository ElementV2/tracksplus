# TracksPlus

Enregistreur **multipiste** et **soundboard** pour le live, dans une seule application.
Fonctionne avec Dante Virtual Soundcard (64 entrées et plus), ASIO, WASAPI et CoreAudio.

| Multipiste | Soundboard |
| --- | --- |
| ![Multipiste](docs/multipiste.png) | ![Soundboard](docs/soundboard.png) |

## Installation et mises à jour

- Page [Releases](https://github.com/ElementV2/tracksplus/releases) :
  `TracksPlus-Setup-<version>.exe` (Windows 10/11 64 bits, aucun prérequis) ou la version
  portable `…-windows-portable.zip`.
- Au premier lancement, Windows SmartScreen peut afficher « Windows a protégé votre
  ordinateur » → « Informations complémentaires » → « Exécuter quand même ».
- Quand une nouvelle version est publiée, une pastille **« Version x.y.z »** apparaît en haut
  à droite (et un point orange sur le menu ☰). Elle affiche les nouveautés et le bouton
  **« Installer et redémarrer »** : TracksPlus se met à jour et se relance, réglages et
  sessions conservés. Indisponible pendant une prise.
- « Ignorer cette version » ne la propose plus. Menu ☰ → « Rechercher des mises à jour »
  pour vérifier à la main ; la recherche au lancement est désactivable. La version portable
  ouvre la page de téléchargement.
- Fenêtre de 1280 × 720 minimum (affichage Windows à 100 %).

## Au lancement

- Un écran d'accueil propose de **reprendre la dernière session**, d'en **créer** une,
  d'en **ouvrir** une, ou de choisir dans la **liste des sessions** (date, nombre de pistes
  et de prises, durée, recherche par nom).
- Après un crash pendant une prise, la session interrompue se rouvre directement.

## Sessions

Une session est un dossier (par défaut `Musique\TracksPlus\<session>`), à copier, déplacer
ou archiver d'un bloc :

```
<session>\
├── <session>.tracksplus   pistes, prises, montage, repères
├── soundboard.json        pages et pads du soundboard
├── Audio\                 prises enregistrées (jamais modifiées)
├── Sons\                  sons du soundboard
└── Export\                fichiers exportés
```

- Chaque session a son propre soundboard. Un son ajouté à un pad est copié dans `Sons\`.
- Carte son, sorties, contrôleur MIDI, LED, disposition du soundboard et mises à jour sont
  communs à toutes les sessions.
- Menu ☰ : Nouvelle session…, **Ouvrir une session…** (explorateur intégré, les dossiers de
  session y apparaissent comme des sessions), Enregistrer (Ctrl+S), Enregistrer sous… (copie
  aussi le soundboard et ses sons, sans écraser une session existante), Afficher le dossier,
  Exporter… (Ctrl+E), format d'enregistrement, **Importer un soundboard…** (reprend les pads
  et les sons d'un autre soundboard), Réglages…, mises à jour.

## Multipiste

### Pistes et enregistrement

- Bouton **+** au-dessus des pistes : nombre de pistes, **Mono** ou **Stéréo** (paire fixe
  1-2, 3-4… dans un seul fichier), première entrée, nom.
- La flèche à gauche du nom déplie la piste : type, entrée (grille, entrées déjà utilisées
  signalées), couleur. Armement, **monitoring direct** (sortie « Monitoring multipiste »),
  **M**ute / **S**olo, renommage par double-clic.
- Enregistrement en WAV 24 bits ou 32 bits flottant (menu ☰), sauvegarde à chaque stop,
  temps restant sur le disque, écoute des pistes existantes pendant la prise.
- **Vumètres** crête IEC 60268-18 : vert / jaune (−12 dB) / rouge (−3 dB), maintien, crête max
  en dB, voyant de saturation (clic pour remettre à zéro). Un par piste, un master stéréo, et
  un bandeau de toutes les pistes (entrée pour les pistes d'enregistrement, lecture pour les
  pistes audio).

### Pistes audio (musique, sons)

- Glisser des fichiers audio (WAV, AIFF, MP3, FLAC, Ogg, M4A) depuis l'explorateur sur le
  multipiste : **une piste audio par fichier**, la prise posée à l'endroit du dépôt (à la
  tête de lecture si on lâche sur la colonne des pistes). Un fichier lâché sur une piste
  audio existante s'y ajoute.
- Les fichiers sont convertis à la fréquence de la session et copiés dans `Audio\` ; les
  originaux ne sont jamais modifiés. Progression et « Annuler » pendant la conversion ;
  chaque fichier refusé est signalé avec la raison. Ctrl+Z retire les pistes ajoutées.
- Une piste audio est en lecture seule : pas d'entrée, pas d'enregistrement ni de monitoring,
  mono ou stéréo selon ses fichiers. Son vumètre montre le **niveau de lecture** (une barre
  par canal). Montage, fondus, M / S et export comme les autres pistes.

### Navigation

- La timeline suit la tête de lecture ; après un défilement manuel, le suivi reprend au bout
  de 3 s.
- Clic sur la règle ou une piste : déplacer la tête de lecture. Glisser sur la règle : faire
  défiler (sur la tête de lecture : la déplacer).
- **Plage In/Out** : Ctrl + clic sur la règle = entrée, Alt + clic = sortie, ou **I** / **O**
  à la tête de lecture. Échap l'efface.
- Glisser sur les pistes : rectangle de sélection (Ctrl / Maj pour ajouter). Double-clic sur
  une prise : sélectionner sa durée.
- **Barre d'outils** (bulle d'aide avec le raccourci) : suivre la lecture, magnétisme ·
  repère, entrée `[`, sortie `]`, effacer la plage · couper, supprimer · annuler, rétablir ·
  vumètres ; à droite, exporter.

### Montage et repères

- Clic sur une prise : la sélectionner (Ctrl pour ajouter). Glisser : la déplacer, aussi vers
  une autre piste. Tirer ses bords : la rogner.
- **S** coupe à la tête de lecture ; **Suppr** supprime la sélection ou le contenu de la plage.
- **Magnétisme** (bouton aimant, désactivé au lancement) : tête de lecture, repères,
  sélection, bords des prises ; Maj pour l'ignorer.
- **Fondus** : poignées aux coins supérieurs des prises, ou clic droit → 10 ms / 100 ms / 1 s.
  Deux prises qui se chevauchent avec un fondu font un fondu enchaîné.
- Ctrl+Z / Ctrl+Maj+Z. Le montage ne modifie jamais les fichiers enregistrés.
- **Repères** : **M** (même pendant une prise). Clic : y aller ; clic droit ou double-clic :
  renommer, supprimer, sélectionner jusqu'au suivant. ← / → et **1 à 9** pour y sauter.

## Export

Chaque option a une bulle d'aide avec un exemple.

- **Contenu** : pistes choisies une à une (ou celles sélectionnées à gauche), toute la
  session ou seulement la **plage In/Out** ; **stems** (stéréo pour les pistes stéréo) et/ou
  **mix stéréo ou mono**.
- **Formats** : **WAV** (RF64 au-delà de 4 Go), **BWF** (horodaté, se recale seul en
  post-production), **AIFF** (4 Go maximum), **FLAC**, **MP3** (128 à 320 kbit/s ou VBR V0,
  tags ID3), **Ogg Vorbis** (qualité 5, 7 ou 9).
- **Résolution** 16 bits (avec dither), 24 bits, 32 bits entier ou flottant selon le format ;
  **fréquence** 44,1 à 192 kHz (48 kHz maximum en MP3 et Ogg).
- **Normalisation** par fichier : crête −0,1 ou −1 dB, ou −14 LUFS (streaming), −16 LUFS
  (podcast), −23 LUFS (EBU R128), crête vraie sous −1 dBTP.
- Résumé avant export : nombre de fichiers, format, **taille estimée**.

## Soundboard

### Pads

- Grille de pads, jusqu'à 8 pages, de 4×2 à 8×8 (ou la taille du contrôleur reconnu).
- Ajouter un son : glisser-déposer un fichier (WAV, AIFF, MP3, AAC/M4A, FLAC, OGG, 30 min
  maximum) sur une case, ou cliquer sur une case vide pour ouvrir l'**explorateur de sons**
  (Sons de la session, Musique, Bureau, Documents, Téléchargements, disques ; chemin
  cliquable ; recherche ; plusieurs sons avec Ctrl / Maj ; double-clic pour ajouter ; rouvert
  dans le dernier dossier).
- Chaque pad affiche son titre et sa durée (`mm:ss.cc`, temps restant pendant la lecture).
- **Modes**, pad par pad, avec option de boucle : **Lecture-Stop** (un appui lance, le
  suivant arrête), **Maintenir** (joue tant que le pad est tenu), **Relancer** (chaque appui
  repart du début ; mode des nouveaux pads). Déclenchement à l'appui.
- **Éditeur** (en bas) : forme d'onde zoomable, poignées **début / fin** et **fondus**, gain en
  dB, couleur, nom, touche clavier, code MIDI, « Écouter » (depuis le début).
- Clic : sélectionner. **Ctrl + clic** : jouer. Glisser : ranger sur une autre case (échange
  si occupée) ou sur un onglet de page. Double-clic : renommer (Entrée / Échap). **Suppr** :
  supprimer. Clic droit : Renommer, Remplacer le son…, Écouter, Arrêter, Supprimer le pad.

### Transport et sortie

- Sur l'onglet Soundboard, la barre du haut affiche le transport des pads à la place de
  celui du multipiste : **Stop** (panique en deux temps, Échap : un premier appui éteint
  tous les pads en fondu d'une seconde, un second appui pendant le fondu les coupe net),
  voyant vert quand
  un pad joue, temps restant du dernier pad à finir, nombre de pads en lecture, pastille
  **REC** pendant une prise multipiste (clic : retour au multipiste).
- Les pads jouent à 0 dB sur la sortie Soundboard (Réglages → Carte son), chacun avec son
  gain. Vumètre stéréo des pads.
- Le soundboard est enregistré automatiquement dans la session.
- **Mode spectacle** (cadenas de la barre du soundboard) : les pads restent jouables, mais
  rien ne peut être supprimé, déplacé, remplacé ou réglé par erreur. Pas de mot de passe.
- **Anti-rebond** : un second appui sur le même pad à moins de 150 ms (rebond d'un bouton
  MIDI, double-clic) est ignoré.

### Contrôleur MIDI

- Choix du port MIDI (ou tous les ports). Les pads se déclenchent même fenêtre réduite.
- **Disposition automatique** (par défaut) : avec un contrôleur reconnu, la grille prend sa
  taille et sa disposition. Le MIDI appartient à la case : un son glissé dans une case est
  joué par le pad physique au même endroit, sur la page affichée. Débrancher le contrôleur
  rend la grille des réglages. Avec « Tous les ports MIDI », le premier contrôleur reconnu
  est utilisé.
- **Disposition manuelle** : **MIDI learn** pad par pad (notes et CC), pour tout autre
  contrôleur.
- **LED** : les pads chargés s'allument faiblement dans leur couleur, fortement pendant la
  lecture. En disposition manuelle, les pads associés s'allument (on/off pour les contrôleurs
  non reconnus). Modèle et sortie MIDI détectés automatiquement, réglables dans le menu LED.

| Contrôleur | Grille | Pages depuis le contrôleur | LED |
| --- | --- | --- | --- |
| Novation Launchpad MK2, X, Mini MK3, Pro, Pro MK3 | 8 × 8 | colonne de droite (1 à 8) | couleur du pad |
| Novation Launchpad S, Mini mk1 / mk2 | 8 × 8 | colonne de droite (1 à 8) | ambre / vert |
| Akai APC mini mk2 | 8 × 8 | colonne de droite (1 à 8) | couleur du pad |
| Akai APC mini | 8 × 8 | colonne de droite (1 à 8) | jaune / vert |
| Ableton Push 2 | 8 × 8 | boutons de scène (1 à 8) | blanc / vert |
| Akai APC Key 25 mk2 | 5 × 8 | Scene Launch (1 à 5) | couleur du pad |
| Akai APC Key 25 | 5 × 8 | Scene Launch (1 à 5) | jaune / vert |
| Akai APC40 mkII | 5 × 8 | Scene Launch (1 à 5) | couleur du pad |
| Akai Fire | 4 × 16 | boutons Mute / Solo des lignes (1 à 4) | — |
| Akai MPD218 | 4 × 4 (banque A) | — | — |
| Novation Launchkey MK3, Mini MK3, MK4 | 2 × 8 (mode Drum) | — | — |
| Korg nanoPAD2 | 2 × 8 (scène 1) | — | — |
| Akai MPK mini mk3 | 2 × 4 (banque A) | — | — |
| Arturia MiniLab 3 | 1 × 8 (banque A) | — | — |

- Contrôleurs utilisés dans leur réglage d'usine. Les Launchpad récents passent seuls en mode
  « Programmer », l'APC40 mkII en mode piloté par l'ordinateur, le Push 2 en mode « User »
  sur son port User.
- Sans boutons de page, les pages se changent à l'écran.
- Port à choisir : APC Key 25 mk2 → le second port (pads) ; Launchkey → le port principal
  (pas « MIDIIN2 »).

## Réglages

Menu ☰ → « Réglages… », ou la pastille de la carte son en haut à droite.

- **Carte son** : pilote (ASIO / WASAPI, CoreAudio, ALSA / JACK), entrée, fréquence, buffer,
  et les sorties, dans l'ordre **Main multipiste**, **Monitoring multipiste**, **Soundboard** :
  chacune sur n'importe quelle paire de n'importe quel périphérique (ou « Comme le main
  multipiste »). La configuration est retrouvée au lancement.
  **« Actualiser »** reprend les réglages changés dans le panneau du pilote (buffer,
  fréquence). Indisponible pendant une prise.
- **Soundboard** : contrôleur MIDI, LED, disposition (automatique / manuelle), taille de la
  grille.

## Raccourcis

| Touche | Multipiste | Soundboard |
| --- | --- | --- |
| Espace | Lecture / Stop (pendant une prise : deux appuis) | |
| R | Enregistrer (pendant une prise : deux appuis pour arrêter) | |
| M | Ajouter un repère | |
| I / O | Point d'entrée / de sortie de la plage à la tête de lecture | |
| ← / → | Repère précédent / suivant | |
| 1 … 9 | Aller au repère 1 … 9 | |
| Origine / Entrée | Début (ou début de la sélection) | |
| F | Tout afficher | |
| S | Couper à la tête de lecture | |
| Suppr | Supprimer la sélection | Supprimer le pad sélectionné |
| Ctrl + Z / Ctrl + Maj + Z | Annuler / Rétablir | |
| Échap | Effacer les sélections | Tout arrêter (pendant un renommage : l'annuler) |
| Ctrl + clic | Ajouter une prise à la sélection ; sur la règle : point d'entrée de la plage | Jouer le pad |
| Alt + clic (règle) | Point de sortie de la plage | |
| Glisser sur la règle | Faire défiler la timeline (sur la tête de lecture : la déplacer) | |
| Double-clic | Renommer une piste / un repère | Renommer le pad |
| Ctrl + molette | Zoom | Zoom de l'éditeur |
| Alt + molette | Défiler dans le temps | |
| Ctrl + 1 / Ctrl + 2 | Changer d'onglet | Changer d'onglet |
| Ctrl + S / Ctrl + E | Enregistrer / Exporter | |

## Utilisation avec Dante

1. Réglages → Carte son : pilote **ASIO** (Windows) ou **CoreAudio** (macOS) → **Dante
   Virtual Soundcard**, à la même fréquence que le réseau Dante.
2. Multipiste : bouton **+** pour chaque entrée (mono ou stéréo), renommez, armez, ⏺.
3. Réglages → Carte son : sortie Soundboard sur la paire Dante dédiée aux pads.

## Fiabilité en live

- **Copie de sécurité** (option, désactivée par défaut : Réglages → Enregistrement) : chaque
  prise est écrite en même temps sur un second disque, avec un double de la session ouvrable
  tel quel. La place libre du second disque est affichée en bas de l'écran et surveillée
  (alerte dès 30 min restantes, ou si le disque est introuvable). Un second disque lent, plein ou
  débranché ne gêne jamais l'enregistrement principal.
- **Fichiers BWF** : chaque prise porte son heure d'enregistrement (TimeReference) et le nom
  de sa piste ; Pro Tools, Nuendo ou Reaper peuvent la poser à son heure réelle.
- **Carte son perdue** (DVS relancé, câble USB) : reconnexion automatique dès que la prise
  est arrêtée.
- **Disque qui revient** (clé rebranchée, place libérée) : la piste reprend toute seule,
  calée sur les autres.
- **Aucune prise perdue** : fichiers lisibles même après un crash ou une coupure de courant.
  Au redémarrage, la session est rouverte, les fichiers réparés et les prises replacées sur
  la timeline.
- **Disque en retard** : 4 à 15 s de réserve par piste ; au-delà, du silence remplace les
  trames manquantes, la piste reste calée. Alerte immédiate, et dès 30 min de place restante.
- **Prises longues** : au-delà de 2 Go, la prise continue sans coupure dans `nom_p2.wav`,
  `nom_p3.wav`…, posés bout à bout.
- **Pas d'arrêt accidentel** : pendant une prise, Espace, R et le bouton Rec demandent un
  second appui ; fermer la fenêtre demande confirmation ; changer de carte son et exporter
  sont bloqués.
- **Alertes en direct** (bandeau rouge) : plus de signal de la carte (horloge Dante, câble),
  erreur ou saturation du disque, disque presque plein, erreur du pilote, sauvegarde
  impossible.
- **Décrochages** du pilote comptés pendant la prise ou la lecture et rappelés à l'arrêt.
- **Prises recalées** : la latence de la carte est compensée en enregistrant sur des pistes
  existantes.
- **Stop immédiat**, sauvegarde aussitôt ; sauvegarde automatique du montage toutes les 30 s.
- Pas de mise en veille de l'ordinateur pendant une prise.
- Une erreur d'interface n'arrête jamais l'audio (journalisée dans `crash.log`, dossier
  `%APPDATA%\TracksPlus`).
- Un export annulé ne laisse pas de fichier incomplet.
- Mises à jour vérifiées (empreinte SHA-256) avant installation, après une sauvegarde.
- Une seule instance : relancer TracksPlus ramène la fenêtre déjà ouverte.

## Préparer l'ordinateur avant un spectacle

Réglages recommandés par Audinate, Steinberg et Allen & Heath pour l'enregistrement
multipiste ; TracksPlus ne peut pas les faire à votre place.

**Windows**
- Mode d'alimentation **Performances élevées** (ou « Performances optimales »), veille et
  écran de veille sur **Jamais**.
- Options d'alimentation avancées : **suspension sélective USB désactivée** ; dans le
  gestionnaire de périphériques, décocher « Autoriser l'ordinateur à éteindre ce
  périphérique » pour la carte réseau Dante et les interfaces USB.
- Exclure le dossier des sessions de l'analyse en temps réel de l'antivirus ; pas de mise à
  jour Windows pendant le spectacle (une mise à jour peut réactiver les économies d'énergie :
  revérifier après chaque mise à jour).
- Pilote **ASIO** de préférence (sinon WASAPI : TracksPlus passe alors son thread audio en
  priorité « Pro Audio »).

**Dante Virtual Soundcard**
- Carte réseau **intégrée** en Gigabit (éviter les adaptateurs USB), réseau Gigabit de bout
  en bout, une horloge maître matérielle sur le réseau.
- Latence DVS : **10 ms** sur un ordinateur portable ou peu puissant (4 ms seulement sur une
  machine très stable), buffer ASIO 128 à 256.
- Ne jamais changer la fréquence ou la latence de DVS pendant que TracksPlus enregistre.

**macOS**
- Économiseur d'énergie : pas de suspension, disques non mis en veille ; Bluetooth et
  synchronisations cloud coupés pendant le spectacle.

**Avant les portes**
- Une prise test de 2 minutes sur toutes les pistes, réécoutée ; vérifier qu'aucun
  « décrochage » n'est compté en bas de l'écran.
- Copie de sécurité activée sur un second disque, place libre vérifiée sur les deux disques.
- Soundboard en **mode spectacle** (cadenas).

## Développement

Prérequis : [Rust](https://rustup.rs) stable. Builds en local (le workflow
`.github/workflows/build.yml` ne se lance qu'à la main).

| Script | Rôle |
| --- | --- |
| `scripts/check.sh` | Formatage, lint strict, tests, test de charge 64 pistes, build release Linux, compilation Windows et macOS |
| `scripts/package-windows.ps1` | Sous Windows : `dist/TracksPlus-Setup-<version>.exe` et `dist/TracksPlus-<version>-windows-portable.zip` (ASIO + WASAPI). Prérequis : Visual Studio Build Tools (C++), `winget install LLVM.LLVM`, `winget install JRSoftware.InnoSetup`. SDK ASIO téléchargé automatiquement |
| `scripts/release.ps1` | Publie la release GitHub `v<version>` (installeur, portable, notes) après avoir vérifié que tout est commité et poussé. À lancer avec PowerShell 7 : `pwsh -File scripts\release.ps1 -Notes notes.md`. Avant : monter `version` dans `Cargo.toml`. Prérequis : `gh auth login` |
| `scripts/build-windows.sh` | Depuis Linux : `dist/TracksPlus.exe` avec ASIO, autonome. Prérequis : `apt install mingw-w64 clang`, `rustup target add x86_64-pc-windows-gnu`. SDK ASIO récupéré automatiquement |
| `scripts/build-macos.sh` | Sur un Mac : `dist/TracksPlus.app` universelle (Apple Silicon + Intel) |

Autres builds : `cargo build --release --features asio` sous Windows avec Visual Studio
(`CPAL_ASIO_DIR` vers le SDK ASIO) ; `cargo build --release` sous Linux (`libasound2-dev`).

| Fichier | Rôle |
| --- | --- |
| `src/audio.rs` | Périphériques, flux cpal, callbacks temps réel, vumètres |
| `src/sampler.rs` | Moteur des pads (voix, fondus, modes) |
| `src/soundboard.rs` | Modèle du soundboard, décodage (symphonia) et rééchantillonnage des sons |
| `src/midi.rs` | Contrôleurs MIDI : dispositions, LED, déclenchement direct |
| `src/recorder.rs` / `src/player.rs` | Enregistrement et lecture multipiste sur disque |
| `src/render.rs` / `src/export.rs` | Lecture des prises, formes d'onde, export (rendu, conversion, normalisation) |
| `src/encode.rs` | Écriture des fichiers exportés : WAV/RF64, BWF, AIFF, FLAC, MP3, Ogg Vorbis |
| `src/project.rs` | Session, pistes, prises, repères |
| `src/update.rs` | Mises à jour depuis les releases GitHub |
| `src/sessions.rs` | Sessions connues, pour l'écran d'accueil |
| `src/ui/browser.rs` | Explorateur intégré (sons, sessions) |
| `src/app.rs`, `src/ui/*` | Interface (egui) : thème, widgets, onglets |

### Licence

**GNU GPL v3** (voir [`LICENSE`](LICENSE)). Composants tiers : police
[Inter](https://rsms.me/inter/) (OFL, `assets/fonts`) ; SDK ASIO de Steinberg, sous sa
licence GPLv3 ; `vendor/asio-sys`, copie d'asio-sys (Apache-2.0) avec une seule modification
pour MinGW ; export : LAME (LGPL), libvorbis et libogg (BSD), flacenc (Apache-2.0), ebur128
(MIT).
