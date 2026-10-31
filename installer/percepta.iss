; Build with: iscc /DAppVersion=1.0.0 installer\percepta.iss
; after: cargo build --release

#ifndef AppVersion
  #error Pass the version: iscc /DAppVersion=1.0.0 installer\percepta.iss
#endif

[Setup]
; Never change this: it is how Windows and Setup recognise an existing install.
AppId={{7C1E0B52-3F4A-4E7D-9A61-5D2B8F0C6E13}
AppName=Percepta
AppVersion={#AppVersion}
AppPublisher=Nodralis
AppPublisherURL=https://github.com/KamiyaShiroba/Percepta
AppSupportURL=https://github.com/KamiyaShiroba/Percepta/issues
; Per user, so that installing and updating never ask for an administrator.
PrivilegesRequired=lowest
DefaultDirName={autopf}\Percepta
DisableProgramGroupPage=yes
DefaultGroupName=Percepta
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\target\installer
OutputBaseFilename=Percepta-Setup-{#AppVersion}
SetupIconFile=..\assets\percepta.ico
UninstallDisplayIcon={app}\percepta.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ShowLanguageDialog=no
CloseApplications=yes
RestartApplications=no

; Setup follows the Windows display language. The first entry is the fallback.
[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
; Inno Setup's releases don't bundle this one yet: it comes from its source repository.
Name: "chinesesimplified"; MessagesFile: "ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\percepta.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\NOTICE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Percepta"; Filename: "{app}\percepta.exe"
Name: "{autodesktop}\Percepta"; Filename: "{app}\percepta.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\percepta.exe"; Description: "{cm:LaunchProgram,Percepta}"; Flags: nowait postinstall skipifsilent
; An update runs silently, and started from the app, which has closed for it.
Filename: "{app}\percepta.exe"; Flags: nowait; Check: WizardSilent
