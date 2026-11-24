# TracksPlus

Enregistreur **multipiste** et **soundboard** dans une seule application. Interface sombre,
plate et dense : gris anthracite, accent orange, angles quasi droits, couleurs de pistes
désaturées.
Pensé pour le live : Dante Virtual Soundcard (jusqu'à 64 entrées et plus), ASIO, CoreAudio.

| Multipiste | Soundboard |
| --- | --- |
| ![Multipiste](docs/multipiste.png) | ![Soundboard](docs/soundboard.png) |

## Au lancement

Un **écran d'accueil** en pleine page (aucune session n'est ouverte derrière tant qu'on n'a
pas choisi) propose de **reprendre la dernière session**, d'en créer une nouvelle,
d'en ouvrir une, ou de choisir dans la **liste des sessions** (celles du dossier
`Musique\TracksPlus` et les sessions récentes ouvertes ailleurs), avec leur date, leur nombre
de pistes et de prises et leur durée, et une recherche par nom. L'application attend ce
choix : aucune session vide n'est ouverte en douce. « Ouvrir une session… » utilise le même
explorateur intégré : les dossiers de session y apparaissent directement comme des sessions,
comme dans le menu ☰ → « Ouvrir une session… ». Après un crash pendant une prise, la
session interrompue est rouverte directement, sans passer par cet écran.

## Une session = un dossier

Tout ce qui appartient à une session est rangé dans son dossier (par défaut
`Musique\TracksPlus\<session>`) : on peut le copier, le déplacer ou l'archiver d'un bloc.

```
<session>\
├── <session>.tracksplus   pistes, prises, montage, repères
├── soundboard.json        pages et pads du soundboard de la session
├── Audio\                 prises enregistrées (WAV, jamais modifiées)
│   └── .peaks\            formes d'onde en cache (réouverture rapide, recalculées au besoin)
├── Sons\                  sons du soundboard (copiés à leur ajout)
└── Export\                fichiers exportés
```

- **Chaque session a son soundboard.** Un son glissé dans un pad est **copié** dans `Sons\` :
  la session reste complète même si l'original est déplacé ou supprimé (un son déjà copié
  n'est pas dupliqué). Les chemins sont relatifs au dossier de la session.
- **Communs à toutes les sessions** (réglages de l'ordinateur, dans `%APPDATA%\TracksPlus`) :
  carte son et sorties, contrôleur MIDI, LED et disposition du soundboard, mises à jour.
- « Enregistrer sous… » copie aussi le soundboard et ses sons, en arrière-plan (fenêtre de
  progression), et refuse d'écraser une session existante du même nom. Menu ☰ → « Importer un
  soundboard… » reprend les pads d'un autre soundboard (celui d'une autre session, ou
  l'ancien soundboard commun mis de côté dans `%APPDATA%\TracksPlus\soundboard (avant les
  sessions).json`) en copiant ses sons dans la session.

## Onglet Multipiste

- **Réglages** (menu ☰ → « Réglages… », ou la pastille de la carte son en haut à droite) :
  une seule fenêtre à onglets, **Carte son** et **Soundboard**.
- **Carte son** : pilote (ASIO / WASAPI, CoreAudio, ALSA / JACK), entrée, fréquence, buffer, et
  **toutes les sorties au même endroit** : multipiste, soundboard et monitoring, chacune sur
  n'importe quelle paire de n'importe quel périphérique (ou « Comme le multipiste »).
  Configuration mémorisée : on retrouve sa carte Dante au lancement. Après un changement
  de buffer ou de fréquence dans le panneau du pilote (RME, Focusrite…), **« Actualiser »**
  dans l'onglet « Carte son » rouvre la carte et reprend ces réglages (en ASIO, buffer et fréquence
  repassent en « Auto »). Indisponible pendant une prise.
- **Fenêtre** : 1280 × 720 minimum (à l'échelle d'affichage 100 % de Windows).
- **Pistes** : bouton **+** au-dessus des pistes (à droite du nombre de pistes) → nombre de pistes, **Mono** ou **Stéréo**, première
  entrée et nom. Mono = une entrée ; stéréo = une paire fixe (1-2, 3-4…) enregistrée dans un
  seul WAV stéréo. La flèche à gauche du nom déplie le panneau de la piste : type (mono /
  stéréo), entrée (sélecteur en grille, entrées déjà utilisées signalées), **couleur**,
  armement et monitoring.
  Armement, **monitoring direct** (casque : l'entrée de la piste est écoutée en temps réel sur
  la sortie « Monitoring »), **M**ute / **S**olo pour la lecture, renommage par double-clic.
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
  Clic sur la règle / une piste = se déplacer (même en lecture). **Glisser sur la règle =
  faire défiler la timeline** (attraper la tête de lecture la déplace). **Plage de temps**
  (export ou suppression d'une plage) : **Ctrl + clic sur la règle = point d'entrée, Alt +
  clic = point de sortie**, ou à la tête de lecture avec les boutons **Entrée / Sortie** de
  la barre d'outils (touches **I** / **O**) ; un point seul attend l'autre ; Échap efface. **Glisser sur les
  pistes = rectangle de sélection des prises** (Ctrl / Maj pour ajouter). Touches **1 à 9** = aller au
  repère 1 à 9. Double-clic sur une prise = sélectionner sa durée, barre de défilement.
- **Barre d'outils en pictogrammes** (bulle d'aide au survol, avec le raccourci) : suivre la
  lecture, magnétisme · repère, point d'entrée `[`, point de sortie `]`, effacer la plage ·
  couper (ciseaux), supprimer (corbeille) · annuler, rétablir · vumètres ; à droite, exporter.
- **Formes d'onde** détaillées (un point toutes les 256 échantillons, trois niveaux de zoom,
  courbe lissée au zoom fin), claires sur fond teinté, échelle d'affichage relevée pour les
  passages faibles (le son n'est jamais modifié).
- **Montage** : cliquer une prise pour la sélectionner (Ctrl pour en ajouter), la **glisser**
  pour la déplacer (y compris vers une autre piste), **rogner** en tirant ses bords,
  **couper** à la tête de lecture (S), **supprimer** (Suppr) les prises sélectionnées ou tout le
  contenu d'une plage de temps. **Magnétisme** sur la tête de lecture, les repères, la sélection
  et les bords des autres prises (désactivé au lancement : bouton aimant ; Maj pour l'ignorer). **Annuler / Rétablir** (Ctrl+Z,
  Ctrl+Maj+Z). Le montage est non destructif : les fichiers enregistrés ne sont jamais modifiés.
- **Fondus sur les prises** : poignées blanches aux coins supérieurs de chaque prise à tirer
  pour régler le fondu d'entrée et de sortie (courbe en S, sans clic), ou clic droit →
  fondus 10 ms / 100 ms / 1 s sur la sélection. Deux prises qui se chevauchent avec un fondu
  font un fondu enchaîné. Les fondus s'appliquent à l'écoute comme à l'export.
- **Repères** (touche M, même pendant l'enregistrement) : clic = y aller ; clic droit ou
  double-clic sur l'étiquette = renommer, supprimer, sélectionner
  jusqu'au repère suivant, flèches ← / → pour sauter de repère en repère.
- **Export** : une fenêtre simple, chaque option expliquée par une bulle avec un exemple concret.
  - **Contenu** : pistes **choisies une à une** (ou les pistes sélectionnées dans la colonne de
    gauche), toute la session ou **seulement la plage In/Out** ; pistes séparées (stéréo pour les
    pistes stéréo) et/ou mix **stéréo ou mono**.
  - **Formats** : **WAV** (RF64 automatique au-delà de 4 Go), **BWF** (horodatage et description :
    chaque fichier se recale seul en post-production), **AIFF**, **FLAC** (sans perte),
    **MP3** (128 à 320 kbit/s ou VBR V0, tags ID3) et **Ogg Vorbis** (qualité 5, 7 ou 9).
  - **Résolution** 16 bits (dither TPDF) / 24 bits / 32 bits entier / 32 bits flottant selon le
    format ; **fréquence** 44,1 à 192 kHz (48 kHz maximum en MP3 et Ogg), conversion de haute
    qualité si elle diffère de la session.
  - **Normalisation** par fichier : crête −0,1 ou −1 dB, ou volume perçu −14 LUFS (streaming),
    −16 LUFS (podcast), −23 LUFS (EBU R128), crête vraie gardée sous −1 dBTP.
  - Résumé avant export : nombre de fichiers, format et **taille estimée**.

## Onglet Soundboard

- **Pads** en grille, jusqu'à 8 pages. Sans contrôleur reconnu, la grille se choisit dans
  les réglages (4×2 à 8×8) ; avec un contrôleur reconnu, elle prend sa taille (voir
  « Disposition automatique »). Glissez-déposez des fichiers (WAV, AIFF, MP3, AAC/M4A, FLAC,
  OGG) sur une case, ou cliquez sur une case vide : l'**explorateur de sons intégré**
  s'ouvre (raccourcis Sons de la session, Musique, Bureau, Documents, Téléchargements,
  disques ; chemin cliquable ; recherche ; plusieurs sons avec Ctrl / Maj ; double-clic pour
  ajouter ; il se rouvre dans le dernier dossier utilisé). Déclenchement à l'appui (pas au
  relâchement) pour la réactivité.
- **Pads plus larges que hauts** : titre sur plusieurs lignes, et durée en gros, alignée sur
  le titre, au format `mm:ss.cc` (deux chiffres partout ; temps restant pendant la lecture).
  **Double-clic sur un pad (ou clic droit → Renommer) pour le renommer** directement dans la
  grille (Entrée valide, Échap annule).
- **Trois modes**, réglés pad par pad dans l'éditeur (nouveau pad : **Relancer**) :
  - **Lecture-Stop** : un appui lance le son, un nouvel appui l'arrête ;
  - **Maintenir** : le son joue tant que le pad (touche, pad MIDI ou Ctrl + clic) est tenu
    et s'arrête au relâchement ;
  - **Relancer** : chaque appui repart du début, même si le son joue déjà.
  Option de lecture en boucle.
- **Éditeur** (en bas, toujours affiché : la grille ne change pas de taille quand on clique
  un pad) : forme d'onde zoomable, poignées **début / fin**, poignées de **fondu d'entrée et
  de sortie** (courbes douces), gain en dB, couleur, nom, touche clavier et MIDI du pad.
- **Contrôleur USB** : choix du port MIDI (ou tous), déclenchement direct dans le thread
  audio : fonctionne même fenêtre réduite. En disposition manuelle, **MIDI learn** par pad
  (notes et CC). **Touches clavier** assignables par pad. Le code MIDI d'un pad s'affiche dans l'éditeur, pas sur la grille.
- **Disposition automatique** (par défaut) : quand le contrôleur choisi dans les réglages est
  un modèle reconnu, la grille prend **sa taille et sa disposition**, sans MIDI learn : **le
  MIDI appartient à la case**, pas au son. Glisser un son dans une case le fait jouer par le
  pad au même endroit (qui s'allume s'il a des LED), sur la page affichée. Débrancher le
  contrôleur rend la grille choisie dans les réglages. Avec « Tous les ports MIDI », le
  premier contrôleur reconnu est utilisé. La disposition « Manuelle » garde le MIDI learn
  pad par pad (pour les autres contrôleurs, ou pour tout régler soi-même).

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

  Les contrôleurs sont utilisés dans leur réglage d'usine. Les Launchpad récents passent
  seuls en mode « Programmer », l'APC40 mkII dans le mode où l'ordinateur pilote ses LED, le Push 2 en
  mode « User » s'il est branché sur son port User. Sans boutons de page, les pages se
  changent à l'écran. Ports à choisir : pour l'APC Key 25 mk2, le second port (celui des
  pads) ; pour les Launchkey, le port principal (pas « MIDIIN2 »).
- **LED du contrôleur** : les pads qui ont un son s'allument, sans clignoter : couleur du
  pad faible quand un son y est chargé, forte pendant la lecture (voir le tableau). En
  disposition manuelle, ce sont les pads associés en MIDI qui s'allument (on/off pour les
  contrôleurs non reconnus). Détection automatique du modèle et de la sortie MIDI
  correspondante, réglable dans le menu « LED ».
- **À la souris** : un clic sélectionne le pad (réglages dans l'éditeur), un glissé le range
  sur une autre case (échange si elle est occupée) ou sur un onglet de page pour l'y envoyer.
  **Suppr** supprime le pad sélectionné ; clic droit : Renommer, Remplacer le son…, Écouter,
  Arrêter, Supprimer le pad.
  **Ctrl + clic joue le pad** (pour tester sans contrôleur, selon son mode). Les sons se
  jouent aussi au clavier, au contrôleur MIDI, ou avec « Écouter » dans l'éditeur (repart
  toujours du début).
- **Transport des pads** dans la barre du haut (à la place de celui du multipiste) : **stop**
  = tout arrêter, net, sans fondu (Échap), voyant vert dès qu'un pad joue, compteur du temps
  restant du dernier pad à finir et du nombre de pads en lecture. Une prise multipiste en
  cours reste signalée (pastille REC, clic : retour au multipiste).
- **Sortie dédiée** (ex. sorties 3–4 de la Dante) à 0 dB, vumètre stéréo des pads. Le
  soundboard est enregistré automatiquement dans la session (voir « Une session = un
  dossier »).

## Fiabilité en live

Conçu pour ne jamais perdre une prise :

- **Écriture directe sur disque** par un thread dédié, tampon de 4 s par piste ; le callback
  audio n'alloue ni ne libère jamais de mémoire et ne prend aucun verrou. Testé à 64 pistes
  48 kHz en temps réel sans aucune perte.
- **Si le disque prend du retard**, les trames manquantes sont remplacées par du silence au
  même endroit : la piste ne se décale jamais par rapport aux autres. Alerte immédiate.
- **En-têtes WAV mis à jour toutes les 2 s, données forcées sur le disque toutes les 10 s.**
- **Prises longues** : au-delà de 2 Go, la prise continue sans coupure dans `nom_p2.wav`,
  `nom_p3.wav`… (un WAV ne peut pas dépasser 4 Go), posés bout à bout dans la session.
- **Récupération après crash ou coupure** : la session et un journal des prises sont écrits
  avant le lancement de l'enregistrement. Si l'application est tuée en pleine prise, elle
  rouvre la session au redémarrage, répare les fichiers (en-têtes recalés sur la taille
  réelle) et replace les prises sur la timeline.
- **À l'arrêt**, tout ce qui reste dans les tampons est écrit, les fichiers sont fermés et la
  session sauvegardée immédiatement.
- **Pas d'arrêt accidentel** : pendant une prise, Espace / R / le bouton Rec demandent une
  confirmation (second appui) ; fermer la fenêtre demande confirmation ; changer de carte son
  ou exporter est bloqué.
- **Alertes en direct** (bandeau rouge) : plus de signal de la carte (horloge Dante, câble),
  erreur disque, disque presque plein, saturation disque, erreur du pilote.
- **Pas de mise en veille** de l'ordinateur pendant une prise.
- **Stop sans attente** : la fin d'écriture des fichiers se fait en arrière-plan ; si le disque
  ne répond plus, l'application le signale et n'enregistre plus avant un redémarrage (les
  prises sont récupérées). Un arrêt du thread d'écriture pendant une prise est signalé.
- **Prises recalées** : en enregistrant sur des pistes existantes, la latence entrée + sortie
  mesurée sur la carte est compensée.
- **Fichiers toujours complets** : session, soundboard et réglages sont écrits dans un fichier
  temporaire forcé sur le disque puis renommé ; une coupure ne laisse jamais de fichier
  tronqué. Une sauvegarde impossible (disque plein, clé retirée) est signalée.
- **Export** : WAV et BWF passent en RF64 au-delà de 4 Go ; l'AIFF, limité à 4 Go, est refusé
  avec un message clair avant d'écrire. Un export annulé ne laisse pas de fichier à moitié écrit.
- **Mises à jour vérifiées** : l'installeur téléchargé est contrôlé (empreinte SHA-256 publiée
  par GitHub) avant d'être lancé, après une sauvegarde complète.
- **Une seule instance** : relancer TracksPlus déjà ouvert affiche un message et ramène la
  fenêtre existante (deux instances se disputeraient la carte son).
- **Décrochages** signalés par le pilote audio : comptés seulement pendant une prise ou une
  lecture (remis à zéro à chaque prise) et rappelés à l'arrêt de la prise.
- **Une erreur d'interface n'arrête jamais l'audio** (rattrapée, journalisée dans
  `crash.log` du dossier de configuration). Sauvegarde automatique du montage toutes les 30 s.
- Sons du soundboard limités à 30 min (chargés en mémoire).

## Installation et mises à jour

- **Télécharger** : page [Releases](https://github.com/ElementV2/tracksplus/releases) →
  `TracksPlus-Setup-<version>.exe` (Windows 10/11 64 bits, aucun prérequis), ou la version
  portable (`…-windows-portable.zip`, sans installation).
- **Mises à jour depuis l'application** : TracksPlus regarde au lancement si une version plus
  récente est publiée (désactivable). Si oui, une pastille **« Version x.y.z »** apparaît en
  haut à droite, et un point orange sur le menu ☰. Elle ouvre les nouveautés et le bouton
  **« Installer et redémarrer »** : l'installeur est téléchargé, TracksPlus se ferme, se met
  à jour et se relance (réglages et sessions conservés). Indisponible pendant une prise.
  « Ignorer cette version » ne la propose plus. Recherche manuelle : menu ☰ → « Rechercher
  des mises à jour ». La version portable ouvre la page de téléchargement.
- L'exécutable n'est pas signé : au premier lancement, Windows SmartScreen peut afficher
  « Windows a protégé votre ordinateur » → « Informations complémentaires » → « Exécuter
  quand même ».

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
| Maj + molette | Défiler dans le temps | |
| Ctrl + 1 / Ctrl + 2 | Changer d'onglet | Changer d'onglet |
| Ctrl + S / Ctrl + E | Enregistrer / Exporter | |

## Utilisation avec Dante

1. Réglages, onglet Carte son (pastille en haut à droite) → pilote **ASIO** (Windows) ou **CoreAudio**
   (macOS) → **Dante Virtual Soundcard**. Fréquence identique à Dante Controller.
2. Multipiste : bouton **+** au-dessus des pistes (mono ou stéréo) pour chaque entrée, renommez, armez, ⏺.
3. Même onglet : sortie Soundboard sur la paire de sorties Dante dédiée aux pads.

## Compiler

Prérequis : [Rust](https://rustup.rs) stable. Les builds se font en local (pas de CI GitHub) :

| Script | Rôle |
| --- | --- |
| `scripts/check.sh` | « CI » locale : formatage, lint strict, tests, test de charge 64 pistes temps réel, build release Linux, compilation Windows et macOS |
| `scripts/package-windows.ps1` | **À distribuer** — sous Windows : `dist/TracksPlus-Setup-<version>.exe` (installeur : menu Démarrer, raccourci bureau, désinstallation) et `dist/TracksPlus-<version>-windows-portable.zip`, avec ASIO et WASAPI, exécutable autonome. Prérequis : Visual Studio Build Tools (C++), `winget install LLVM.LLVM` et `winget install JRSoftware.InnoSetup`. Le SDK ASIO est téléchargé automatiquement |
| `scripts/release.ps1` | **Publier une version** : `-Notes notes.md`. Vérifie que tout est commité et poussé, construit le paquet Windows puis crée la release GitHub `v<version>` (installeur + portable + notes). Les applications installées la proposent ensuite d'elles-mêmes. Avant : monter `version` dans `Cargo.toml`. Prérequis : `gh auth login` |
| `scripts/build-windows.sh` | Depuis Linux : `dist/TracksPlus.exe` **avec ASIO** (Dante VSC), autonome (aucune DLL à installer). Prérequis : `apt install mingw-w64 clang`, `rustup target add x86_64-pc-windows-gnu`. Le SDK ASIO est récupéré automatiquement |
| `scripts/build-macos.sh` | Sur un Mac : `dist/TracksPlus.app` universelle (Apple Silicon + Intel) |

Sous Windows avec Visual Studio, `cargo build --release --features asio` fonctionne aussi
(variable `CPAL_ASIO_DIR` vers le SDK ASIO). Linux : `cargo build --release` (paquet
`libasound2-dev`). Le workflow GitHub (`.github/workflows/build.yml`) ne se lance plus qu'à
la main.

## Licence

TracksPlus est distribué sous licence **GNU GPL v3** (voir [`LICENSE`](LICENSE)).

Composants tiers : police [Inter](https://rsms.me/inter/) (OFL, `assets/fonts`) ; SDK ASIO
de Steinberg, utilisé sous sa licence GPLv3 ; `vendor/asio-sys` est une copie d'asio-sys
(Apache-2.0) avec une seule modification pour la compilation MinGW. Export : encodeur MP3
LAME (LGPL), libvorbis et libogg (BSD), flacenc (Apache-2.0), mesure de sonie ebur128 (MIT).

## Architecture

| Fichier | Rôle |
| --- | --- |
| `src/audio.rs` | Périphériques, flux cpal, callbacks temps réel (sans allocation ni verrou), vumètres |
| `src/sampler.rs` | Moteur des pads (voix, fondus, modes) exécuté dans le callback de sortie |
| `src/soundboard.rs` | Modèle du soundboard (enregistré dans la session, sons copiés dans `Sons`), décodage (symphonia) et rééchantillonnage des sons |
| `src/midi.rs` | Contrôleurs MIDI : dispositions des modèles reconnus, LED, déclenchement direct vers le moteur audio |
| `src/recorder.rs` / `src/player.rs` | Threads disque : enregistrement et lecture multipiste |
| `src/render.rs` / `src/export.rs` | Lecture des prises, formes d'onde, export (rendu, conversion, normalisation) |
| `src/encode.rs` | Écriture des fichiers exportés : WAV/RF64, BWF, AIFF, FLAC, MP3, Ogg Vorbis |
| `src/project.rs` | Session, pistes, prises, repères |
| `src/update.rs` | Mises à jour : dernière release GitHub, téléchargement et lancement de l'installeur |
| `src/sessions.rs` | Sessions connues (dossier des sessions + récentes), pour l'écran d'accueil |
| `src/ui/browser.rs` | Explorateur intégré (sons des pads, sessions à ouvrir) |
| `src/app.rs`, `src/ui/*` | Interface (egui) : thème, widgets, onglets |
