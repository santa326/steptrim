; Inno Setup script for StepTrim. Built in CI by .github/workflows/release.yml with
;   iscc /DAppVersion=<version> /DBinDir=<folder with the exes> installer\steptrim.iss
; after `cargo build --release`. Produces StepTrim-Setup-<version>.exe in OutputDir
; (..\dist unless overridden with /O).

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
; Folder holding the four executables to package: the plain build output, or
; the folder the signed binaries were downloaded to in CI.
#ifndef BinDir
  #define BinDir "..\target\release"
#endif

[Setup]
AppId={{8C1F8D2E-6B0B-4D7B-9E4A-5F3C1E2A7B10}
AppName=StepTrim
AppVersion={#AppVersion}
AppVerName=StepTrim {#AppVersion}
AppPublisher=Ravi Teja Potineni
AppPublisherURL=https://github.com/santa326/steptrim
AppSupportURL=https://github.com/santa326/steptrim/issues
AppUpdatesURL=https://github.com/santa326/steptrim/releases
DefaultDirName={autopf}\StepTrim
DefaultGroupName=StepTrim
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
OutputDir=..\dist
OutputBaseFilename=StepTrim-Setup-{#AppVersion}
SetupIconFile=..\crates\steptrim-app\assets\steptrim.ico
UninstallDisplayIcon={app}\steptrim.exe
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Per-user install by default: no administrator prompt, lands in %LOCALAPPDATA%\Programs.
; The dialog lets the user pick an all-users install instead.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ChangesAssociations=yes
MinVersion=10.0
; Version resource of the installer itself; SignPath checks product name and version.
VersionInfoProductName=StepTrim
VersionInfoVersion={#AppVersion}
VersionInfoProductVersion={#AppVersion}
VersionInfoCompany=Ravi Teja Potineni
VersionInfoCopyright=Copyright (c) 2026 Ravi Teja Potineni. MIT License.

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked
Name: "assoc"; Description: "Add StepTrim to ""Open with"" for .stp and .step files"; GroupDescription: "File types:"

[Files]
Source: "{#BinDir}\steptrim.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\stepinfo.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\stepprune.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\stepreplicate.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\CODE_SIGNING_POLICY.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{group}\StepTrim"; Filename: "{app}\steptrim.exe"
Name: "{group}\StepTrim README"; Filename: "{app}\README.md"
Name: "{autodesktop}\StepTrim"; Filename: "{app}\steptrim.exe"; Tasks: desktopicon

[Registry]
; "Open with" entries only: the user's default STEP application is left alone.
Root: HKA; Subkey: "Software\Classes\StepTrim.STEP"; ValueType: string; ValueName: ""; ValueData: "STEP assembly"; Flags: uninsdeletekey; Tasks: assoc
Root: HKA; Subkey: "Software\Classes\StepTrim.STEP\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\steptrim.exe,0"; Tasks: assoc
Root: HKA; Subkey: "Software\Classes\StepTrim.STEP\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\steptrim.exe"" ""%1"""; Tasks: assoc
Root: HKA; Subkey: "Software\Classes\.stp\OpenWithProgids"; ValueType: string; ValueName: "StepTrim.STEP"; ValueData: ""; Flags: uninsdeletevalue; Tasks: assoc
Root: HKA; Subkey: "Software\Classes\.step\OpenWithProgids"; ValueType: string; ValueName: "StepTrim.STEP"; ValueData: ""; Flags: uninsdeletevalue; Tasks: assoc
Root: HKA; Subkey: "Software\Classes\Applications\steptrim.exe\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\steptrim.exe"" ""%1"""; Flags: uninsdeletekey; Tasks: assoc

[Run]
Filename: "{app}\steptrim.exe"; Description: "Launch StepTrim"; Flags: nowait postinstall skipifsilent
