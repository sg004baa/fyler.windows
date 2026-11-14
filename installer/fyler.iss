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
AppVerName=fyler
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

[Code]
var
  // Flag controlling whether user data is deleted in usPostUninstall.
  // Pascal Script globals default to False, but InitializeUninstall always
  // assigns this explicitly, so that implicit default is not relied upon.
  DeleteUserData: Boolean;

// Check whether /KEEPDATA (case-insensitive) is present on the command line.
// ParamCount / ParamStr are built-in functions available to Setup and Uninstall.
function HasKeepDataParam: Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
  begin
    if CompareText(ParamStr(I), '/KEEPDATA') = 0 then
    begin
      Result := True;
      Exit;
    end;
  end;
end;

// Decide once, at uninstall start, whether user data should be deleted.
// The default is always to delete (opt-out).
//   - /KEEPDATA keeps data without prompting (argument takes precedence).
//   - Silent uninstall without /KEEPDATA deletes data.
//   - Interactive uninstall without /KEEPDATA prompts via MsgBox(MB_YESNO),
//     with the first button (Yes) as the default.
function InitializeUninstall: Boolean;
begin
  Result := True;
  if HasKeepDataParam then
  begin
    DeleteUserData := False;
  end
  else if UninstallSilent then
  begin
    DeleteUserData := True;
  end
  else
  begin
    DeleteUserData :=
      MsgBox(
        'Also delete fyler settings and undo data (' + ExpandConstant('{userappdata}') + '\fyler, ' +
        ExpandConstant('{localappdata}') + '\fyler)? (Default: Yes)' + #13#10 +
        'Choose No to keep these folders.',
        mbConfirmation, MB_YESNO) = IDYES;
  end;
end;

// Recursively delete the specified directory. Missing directories are normal.
// On failure, record it in the log and notify interactive users with a message.
// Inno Setup's uninstaller cannot return an arbitrary exit code, so in silent
// mode the log is the only way to report failure to the caller.
procedure DeleteDataDir(const Dir: String);
begin
  if not DirExists(Dir) then
    Exit;

  if not DelTree(Dir, True, True, True) then
  begin
    Log('fyler: failed to delete user data directory: ' + Dir);
    if not UninstallSilent then
    begin
      MsgBox(
        'Failed to delete user data: ' + Dir + #13#10 +
        'Please delete it manually.',
        mbError, MB_OK);
    end;
  end;
end;

// Delete user data after application files have been removed (usPostUninstall).
// Never touch %LOCALAPPDATA%\Programs\fyler (the installation directory).
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if (CurUninstallStep = usPostUninstall) and DeleteUserData then
  begin
    DeleteDataDir(ExpandConstant('{userappdata}\fyler'));
    DeleteDataDir(ExpandConstant('{localappdata}\fyler'));
  end;
end;
