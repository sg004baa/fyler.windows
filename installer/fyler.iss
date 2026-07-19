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

[Code]
var
  // usPostUninstall で参照する、purge を実行するかどうかのフラグ。
  // Pascal Script のグローバル変数はデフォルトで False に初期化される。
  PurgeUserData: Boolean;

// コマンドライン引数に /PURGEDATA (大文字小文字無視) があるかを調べる。
// ParamCount / ParamStr は Setup と Uninstall の両方で使える組み込み関数。
function HasPurgeDataParam: Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
  begin
    if CompareText(ParamStr(I), '/PURGEDATA') = 0 then
    begin
      Result := True;
      Exit;
    end;
  end;
end;

// アンインストール開始時に purge するかどうかを一度だけ判定する。
// デフォルトは常に「削除しない」(opt-in)。
//   - silent アンインストール (UninstallSilent = True):
//       コマンドラインに /PURGEDATA がある場合のみ purge。
//   - 対話アンインストール:
//       /PURGEDATA が付いていれば確認なしで purge (引数優先)。
//       付いていなければ MsgBox(MB_YESNO or MB_DEFBUTTON2, デフォルト No) で確認する。
function InitializeUninstall: Boolean;
begin
  Result := True;
  if HasPurgeDataParam then
  begin
    PurgeUserData := True;
  end
  else if UninstallSilent then
  begin
    PurgeUserData := False;
  end
  else
  begin
    PurgeUserData :=
      MsgBox(
        'アプリの設定や undo データ (' + ExpandConstant('{userappdata}') + '\fyler, ' +
        ExpandConstant('{localappdata}') + '\fyler) も削除しますか?' + #13#10 +
        '「いいえ」を選ぶとこれらのフォルダーは残ります。',
        mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES;
  end;
end;

// 指定ディレクトリを再帰削除する。存在しなければ何もしない (正常系)。
// 失敗時は握りつぶさず Log に記録し、対話時は MsgBox でも通知する。
// Inno のアンインストーラは任意の終了コードを返せないため、silent 時に
// 失敗を呼び出し元へ伝える手段は Log のみとなる。
procedure PurgeDataDir(const Dir: String);
begin
  if not DirExists(Dir) then
    Exit;

  if not DelTree(Dir, True, True, True) then
  begin
    Log('fyler: failed to delete user data directory: ' + Dir);
    if not UninstallSilent then
    begin
      MsgBox(
        'ユーザーデータの削除に失敗しました: ' + Dir + #13#10 +
        '手動で削除してください。',
        mbError, MB_OK);
    end;
  end;
end;

// ファイル本体の削除が終わった後 (usPostUninstall) に purge を実行する。
// %LOCALAPPDATA%\Programs\fyler (インストール先) には一切触れない。
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if (CurUninstallStep = usPostUninstall) and PurgeUserData then
  begin
    PurgeDataDir(ExpandConstant('{userappdata}\fyler'));
    PurgeDataDir(ExpandConstant('{localappdata}\fyler'));
  end;
end;
