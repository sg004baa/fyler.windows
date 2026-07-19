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
  // usPostUninstall で参照する、ユーザーデータ削除を実行するかどうかのフラグ。
  // Pascal Script のグローバル変数はデフォルトで False に初期化されるが、
  // InitializeUninstall で必ず明示代入するため、その暗黙初期値には依存しない。
  DeleteUserData: Boolean;

// コマンドライン引数に /KEEPDATA (大文字小文字無視) があるかを調べる。
// ParamCount / ParamStr は Setup と Uninstall の両方で使える組み込み関数。
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

// アンインストール開始時にユーザーデータを削除するかどうかを一度だけ判定する。
// デフォルトは常に「削除する」(opt-out)。
//   - /KEEPDATA が付いていれば確認なしで保持する (引数優先)。
//   - silent アンインストール (UninstallSilent = True) で /KEEPDATA が
//     なければ削除する。
//   - 対話アンインストールで /KEEPDATA がなければ MsgBox(MB_YESNO,
//     デフォルトボタンは第1ボタン = はい) で確認する。
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
        'アプリの設定や undo データ (' + ExpandConstant('{userappdata}') + '\fyler, ' +
        ExpandConstant('{localappdata}') + '\fyler) も削除しますか? (既定: はい)' + #13#10 +
        '「いいえ」を選ぶとこれらのフォルダーは残ります。',
        mbConfirmation, MB_YESNO) = IDYES;
  end;
end;

// 指定ディレクトリを再帰削除する。存在しなければ何もしない (正常系)。
// 失敗時は握りつぶさず Log に記録し、対話時は MsgBox でも通知する。
// Inno のアンインストーラは任意の終了コードを返せないため、silent 時に
// 失敗を呼び出し元へ伝える手段は Log のみとなる。
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
        'ユーザーデータの削除に失敗しました: ' + Dir + #13#10 +
        '手動で削除してください。',
        mbError, MB_OK);
    end;
  end;
end;

// ファイル本体の削除が終わった後 (usPostUninstall) にユーザーデータを削除する。
// %LOCALAPPDATA%\Programs\fyler (インストール先) には一切触れない。
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if (CurUninstallStep = usPostUninstall) and DeleteUserData then
  begin
    DeleteDataDir(ExpandConstant('{userappdata}\fyler'));
    DeleteDataDir(ExpandConstant('{localappdata}\fyler'));
  end;
end;
