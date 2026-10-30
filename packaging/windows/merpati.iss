; Inno Setup script for the Windows installer (Merpati-Setup-<version>-x64.exe).
;
;   iscc /DAppVersion=0.2.0 /DSourceDir=..\..\target\dist /DOutputDir=..\..\target\dist-pkg packaging\windows\merpati.iss
;
; Installs for the current user by default (no administrator rights needed),
; or for all users when chosen in the first dialog. An update installs over
; the previous version (same AppId) and closes a running Merpati first.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\target\dist"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\target\dist-pkg"
#endif

[Setup]
AppId={{6F2C8E7A-3B1D-4E5F-9A2B-7C4D1E8F0A6B}
AppName=Merpati
AppVersion={#AppVersion}
AppVerName=Merpati {#AppVersion}
AppPublisher=Merpati contributors
AppPublisherURL=https://github.com/ilhamnadhif/merpati
AppSupportURL=https://github.com/ilhamnadhif/merpati/issues
AppUpdatesURL=https://github.com/ilhamnadhif/merpati/releases
DefaultDirName={autopf}\Merpati
DefaultGroupName=Merpati
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
LicenseFile=..\..\LICENSE-MIT
SetupIconFile=merpati.ico
UninstallDisplayIcon={app}\merpati-app.exe
UninstallDisplayName=Merpati
OutputDir={#OutputDir}
OutputBaseFilename=Merpati-Setup-{#AppVersion}-x64
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
ChangesEnvironment=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "addtopath"; Description: "Add the merpati command-line tool to PATH"; GroupDescription: "Command line:"

[Files]
Source: "{#SourceDir}\merpati-app.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\merpati.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "merpati.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Merpati"; Filename: "{app}\merpati-app.exe"; IconFilename: "{app}\merpati.ico"
Name: "{autodesktop}\Merpati"; Filename: "{app}\merpati-app.exe"; IconFilename: "{app}\merpati.ico"; Tasks: desktopicon

[Registry]
; The CLI on PATH (per user, or for all users in an administrative install).
Root: HKA; Subkey: "{code:EnvKey}"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; \
  Check: NeedsAddPath(ExpandConstant('{app}')); Tasks: addtopath

[Run]
Filename: "{app}\merpati-app.exe"; Description: "{cm:LaunchProgram,Merpati}"; Flags: nowait postinstall skipifsilent

[Code]
function EnvKey(Param: String): String;
begin
  if IsAdminInstallMode then
    Result := 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
  else
    Result := 'Environment';
end;

function NeedsAddPath(Dir: String): Boolean;
var
  Paths: String;
  RootKey: Integer;
begin
  if IsAdminInstallMode then RootKey := HKEY_LOCAL_MACHINE else RootKey := HKEY_CURRENT_USER;
  if not RegQueryStringValue(RootKey, EnvKey(''), 'Path', Paths) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') = 0;
end;
