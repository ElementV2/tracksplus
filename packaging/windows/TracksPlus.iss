; Installeur Windows de TracksPlus (Inno Setup 6).
; Généré par scripts/package-windows.ps1, qui fournit AppVersion et le chemin de l'exécutable :
;   ISCC /DAppVersion=0.1.0 /DExe=..\..\dist\TracksPlus.exe packaging\windows\TracksPlus.iss

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef Exe
  #define Exe "..\..\dist\TracksPlus.exe"
#endif

[Setup]
; Identifiant fixe : ne jamais le changer (mises à jour et désinstallation en dépendent).
AppId={{C09414C1-6494-4D3D-8876-623870737525}
AppName=TracksPlus
AppVersion={#AppVersion}
AppVerName=TracksPlus {#AppVersion}
AppPublisher=TracksPlus
VersionInfoVersion={#AppVersion}
VersionInfoProductName=TracksPlus
VersionInfoDescription=Installation de TracksPlus
DefaultDirName={autopf}\TracksPlus
DefaultGroupName=TracksPlus
DisableProgramGroupPage=yes
; Installation pour l'utilisateur courant par défaut (sans droits administrateur),
; ou pour tous les utilisateurs au choix.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
SetupIconFile=..\..\assets\icon\tracksplus.ico
UninstallDisplayIcon={app}\TracksPlus.exe
UninstallDisplayName=TracksPlus
WizardStyle=modern
Compression=lzma2/ultra64
SolidCompression=yes
OutputDir=..\..\dist
OutputBaseFilename=TracksPlus-Setup-{#AppVersion}
; Mise à jour pendant que l'application tourne : on propose de la fermer.
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "french"; MessagesFile: "compiler:Languages\French.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#Exe}"; DestDir: "{app}"; DestName: "TracksPlus.exe"; Flags: ignoreversion
Source: "..\..\assets\fonts\Inter-LICENSE.txt"; DestDir: "{app}\Licences"; DestName: "Inter (police) - OFL.txt"; Flags: ignoreversion
Source: "LISEZMOI.txt"; DestDir: "{app}"; Flags: ignoreversion isreadme
Source: "..\..\LICENSE"; DestDir: "{app}"; DestName: "LICENCE (GPLv3).txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\TracksPlus"; Filename: "{app}\TracksPlus.exe"
Name: "{autodesktop}\TracksPlus"; Filename: "{app}\TracksPlus.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\TracksPlus.exe"; Description: "{cm:LaunchProgram,TracksPlus}"; Flags: nowait postinstall skipifsilent
; Mise à jour depuis l'application (installation silencieuse) : TracksPlus est relancé.
Filename: "{app}\TracksPlus.exe"; Flags: nowait runasoriginaluser skipifnotsilent

; Les réglages (%APPDATA%\TracksPlus) et les sessions (Musique\TracksPlus) appartiennent à
; l'utilisateur : ils sont conservés à la désinstallation.
