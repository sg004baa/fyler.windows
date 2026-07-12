# fyler for windows — lazy scan フェーズ計画(M13 / issue #37)

> 2026-07-12 作成。issue #37(ドライブroot等の巨大rootで全スキャン型baselineが遅い)への
> 根本対応。設計の正典は [DESIGN.md](DESIGN.md)。本書はissue #37コメントの
> 「lazy structural baseline + independent SearchCatalog」方針を実装フェーズへ展開したもの。
> 矛盾した場合は DESIGN.md > issue #37コメント > 本書 の順で優先する。

## 背景

`C:\Users` → `^` → `C:\` が体感10〜15秒(#27実機確認中に観測)。現行設計は
「root全体を再帰列挙してbaseline化する」前提で、コストと常駐メモリがrootの総entry数に
比例する。root遷移だけでなく、遷移後の visible_lines / visible_file_infos / picker候補構築 /
save diff / watchティック / sort・hidden切替 / apply後reconcile もすべて総entry数の影響を受ける。
暫定対応(root変更scanのworker化、830f09c)は実機UXが微妙でrevert済み(9733838)。
ただしその cancellable scan 機構(進捗コールバック+AtomicBoolキャンセル+fallible ID解決)は
本フェーズのloader実装で復活・流用する。

## 方針: 2レイヤー分離(issue #37コメントが正典)

1. **StructuralBaseline** — 表示・編集・diff/apply安全性を担う。root直下+展開済みdirだけを
   保持し、展開時にオンデマンド列挙する。`Unloaded`(意図的未列挙)と #27 の
   `Incomplete`(権限/I/Oエラー)は**別状態**: unloadedは展開すればロードされ、
   dir自身への rename/move/delete/copy は opaque な操作として許可する。
   incompleteは従来どおりplanを遮断する。
2. **SearchCatalog** — fuzzy finder専用の独立index。正典は `EntryId` ではなく
   root相対path。backgroundで再帰構築し、pickerへprogressiveに供給する。
   catalog完成後もStructuralBaselineへは混ぜない。

Explorerと同型(folder表示=対象folder単位の列挙、横断検索=Windows Searchの別index)。
「root遷移時だけshallow scanし裏で完全baselineへ育てる」方式は、scan完了後に全操作が
再び重くなるため**採らない**。

## 本書で確定する追加設計判断(コメントに未記載の3点)

1. **matchingはGUI paintスレッドで実行しない。** `fyler_core::search::search` は実測
   約36ms/50k entry。ドライブ規模(10^6)では約0.7s/キーストロークになるため、
   catalog導入(M13-2)と同時にapp側の検索workerへ移す(query世代トークンで陳腐化した
   結果を破棄。GUIは最後に受信した結果を表示し続ける)。
2. **catalogの鮮度はrecursive watcherの全域イベントで維持する。** 現行watcherはroot全域の
   イベントを既に受けている(baseline反映時にフィルタしているだけ)。debounce済み
   変更パス集合でcatalogを差分更新(re-statしてupsert/remove)し、watcher
   Degraded/overflow時はcatalog再構築。これにより「catalog完成後に作られたファイルが
   `g/` で見つからない」を防ぐ。
3. **すべての列挙は単一の非同期loader機構を通す。** 展開1回のread_dirもnetwork shareで
   ハングし得る。830f09cのcancellable scan+単一owner+完了イベント再ゲートのパターンを
   復活させ、root遷移(shallow)・dir展開・zR再帰展開・catalog構築が共用する。
   進捗ダイアログは再帰展開など長時間になり得る操作のみ表示する。

## フェーズ分割(コメントの「安全な実装順序」準拠)

picker分離を先に行い、lazy baseline導入中に `g/` が機能縮小する期間を作らない。
PR構成: M13-1+M13-2 = `agent/issue-37-search-catalog`、
M13-3+M13-4 = `agent/issue-37-lazy-baseline`(stacked PR)。

### M13-1: picker候補・選択のpath依存化(sourceは現baselineのまま)

- `fyler_core::search::SearchCandidate` から `id: EntryId` を外し、正典を `path: TreePath` へ
- `GuiAction::PickerSelect` / `AppEvent::PickerSelect` を `entry_id` → `path` へ
- app側 `handle_picker_select` は選択pathを**現在の**baselineで再解決
  (`get_by_path` → EntryId)してから既存のjump/open/revealへ。解決不能は既存のstale Warn
- 挙動不変(候補構築はbaselineのまま)。既存picker系テストの追従+stale-path回帰テスト

### M13-2: SearchCatalog(baseline非依存のpicker完全動作)

- `fyler-fsops/catalog.rs` 新設: cancellableな再帰walker。symlink非潜行・hydration非誘発・
  アクセス失敗dirはskipして継続。hiddenはentryへのflag付与とし、picker側でpane設定により
  フィルタ(hidden切替でcatalog再構築しない)
- catalogはpane root毎のsession cache。picker初回起動でbackground構築を開始し、
  `g/` は構築中でも即座に開いて `Indexing… N entries` を表示、結果はprogressiveに追加
- 検索workerをapp側へ新設: GUIはquery変更を送るだけ、workerが上位K件を計算して
  `GuiEvent` で返す(世代トークン)。catalog candidateはcompact表現
  (display+key+kind+hidden。TreePathは選択時にparse)でメモリを抑える
- watch debounce済みパスでcatalogを差分更新。Degraded/overflowで再構築
- 選択はpathをbaselineで再解決(M13-1の契約のまま)。同点scoreの安定順は
  「catalog挿入順」へ変わる(仕様として明記)

### M13-3: directory coverage + shallow scan + 展開時ロード

- `BaselineTree` に unloaded sidecar(incomplete_dirsと同設計: PartialEq非対象、
  強制collapsed扱い)。`Unloaded / Loaded` はbaseline、`Loading` はapp層の一時状態
- `scan_baseline_shallow_with`(root直下のみ列挙、子dirをunloaded登録)+
  `load_directory`(unloaded dirの直下1階層を列挙してCoW挿入。既存pathのIDは維持、
  新規はIdAllocatorから採番)。830f74系のcancellable機構を復活させloader worker化
- 展開系(`<CR>` toggle / zo / za / reveal)がunloaded dirに当たったら非同期ロード→
  完了後にSetLines。zR/zO等の再帰展開は進捗ダイアログ+キャンセル付き
- root遷移はshallow scanへ(read_dir 1回=体感即時)。startup・pane splitも同様
- diff/validate: unloadedはcollapsed相当として扱い、dir自身へのopaque操作
  (rename/move/delete/copy)を**許可**する(incompleteと違い遮断しない)。
  unloaded配下はバッファに行が存在しないため操作対象になり得ない。
  移動先衝突は既存のM9-3実FS preflightが検出する(baseline非依存を確認・テスト)
- undo/transfer/file-info/git badgeの各消費者がunloaded sidecarと整合することを監査

### M13-4: 通常運用のloaded範囲化

- watchティック: 変更パスをloaded集合でフィルタしてから部分rescan
  (unloaded配下のイベントはbaselineへは無反映、catalogへは反映)
- sort/hidden切替・apply後reconcile・offline retry/probe: loaded dir単位の再列挙へ。
  root全体の同期全再スキャン経路を排除
- picker選択がunloaded配下を指す場合: 祖先チェーンを順にlazy loadしてから
  reveal(clean時のみ=既存M11契約と同じ)
- メモリ・時間のbench(release, `#[ignore]`)でloaded entry数∝展開範囲を確認

## 完了条件(issue #37本文+コメント)

- ドライブroot(`C:\`)への遷移が体感即時(Windows実機が正典)
- 遷移後のカーソル移動・fold・ファイル情報・保存処理がroot総entry数に比例しない
- loaded baseline entry数が可視/展開範囲に比例し、catalog完成後も増えない
- `g/` は即時表示・構築中progressive追加・完了後はroot全体を検索可能。
  background構築中も入力latencyが悪化しない
- apply後reconcile・sort/hidden切替・watch更新がroot全体の同期scanへ戻らない
- plan生成・undo・transfer・offline/incompleteの既存契約が維持される

## スコープ外(必要になったら別issue)

- collapsed subtreeのLRU eviction(ロード済みentryの常駐はまず許容)
- catalogの永続化・Windows Search連携・NTFS USN差分更新
- root直下そのものが巨大な単一directoryのchunk列挙/virtualization
