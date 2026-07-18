#ifndef AppVersion
  #error AppVersion must be provided with /DAppVersion=x.y.z
#endif

#ifndef StageDir
  #error StageDir must be provided with /DStageDir=path
#endif

[Setup]
AppId={{93358674-85C3-456A-81DC-DB8BB2EE9A09}
AppName=fyler
AppVersion={#AppVersion}
AppPublisher=sg004baa
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
DefaultDirName={localappdata}\Programs\fyler
DefaultGroupName=fyler
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
UninstallDisplayIcon={app}\fyler.exe
CloseApplications=yes
Compression=lzma2
SolidCompression=yes
OutputDir=..\dist
OutputBaseFilename=fyler-v{#AppVersion}-windows-x64-setup
WizardStyle=modern
ChangesAssociations=yes

[Tasks]
Name: "contextmenu"; Description: "Add 'Open in fyler' to context menu"; GroupDescription: "Add:"; Flags: unchecked

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{userprograms}\fyler"; Filename: "{app}\fyler.exe"; WorkingDir: "{app}"; IconFilename: "{app}\fyler.exe"

[Registry]
Root: HKCU; Subkey: "Software\Classes\Directory\shell\fyler"; ValueType: string; ValueName: ""; ValueData: "Open in fyler"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKCU; Subkey: "Software\Classes\Directory\shell\fyler"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\fyler.exe"; Tasks: contextmenu
Root: HKCU; Subkey: "Software\Classes\Directory\shell\fyler\command"; ValueType: string; ValueName: ""; ValueData: """{app}\fyler.exe"" ""%1"""; Tasks: contextmenu
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\fyler"; ValueType: string; ValueName: ""; ValueData: "Open in fyler"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\fyler"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\fyler.exe"; Tasks: contextmenu
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\fyler\command"; ValueType: string; ValueName: ""; ValueData: """{app}\fyler.exe"" ""%V"""; Tasks: contextmenu
