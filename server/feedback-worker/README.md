# fyler feedback Worker

fylerの匿名フィードバックを受信し、Cloudflare D1へ保存するCloudflare Workerです。
外部npmパッケージには依存していません。デプロイには`wrangler 4.36.0`以降が必要です。

## 初回デプロイ

1. D1データベースを作成します。

   ```console
   wrangler d1 create fyler-feedback
   ```

2. コマンド出力の`database_id`で[wrangler.toml](wrangler.toml)の
   `REPLACE_WITH_YOUR_D1_DATABASE_ID`を置き換えます。

3. テーブルを作成します。

   ```console
   wrangler d1 execute fyler-feedback --remote --file=schema.sql
   ```

4. Workerをデプロイします。

   ```console
   wrangler deploy
   ```

5. GitHubリポジトリのSettings → Secrets and variables → Actions → Variablesで、
   Actions variable `FYLER_FEEDBACK_URL`にデプロイされたWorkerのURLを設定します。
   この値はrelease build時に実行ファイルへ焼き込まれます。variableが未設定または空なら、
   フィードバック機能が無効なrelease binaryとしてビルドされます。

ローカルの入力検証テストは、依存パッケージのインストールなしで実行できます。

```console
node --test test/
```

## 閲覧と保持期間

保存済みフィードバックは、必要な列だけを指定して閲覧します。

```console
wrangler d1 execute fyler-feedback --remote --command "SELECT id, received_at, kind, body, app_version, os, arch FROM feedback ORDER BY received_at DESC LIMIT 100"
```

受信から12ヶ月を経過したデータを定期的に削除します。運用ジョブまたは手動作業で、
少なくとも月1回を目安に次のSQLを実行してください。

```console
wrangler d1 execute fyler-feedback --remote --command "DELETE FROM feedback WHERE received_at < datetime('now', '-12 months')"
```

このテーブルはIPアドレス、User-Agent、その他の識別子を保存しません。

## 機能停止

今後のreleaseで送信機能を無効にするには、GitHub Actions variable
`FYLER_FEEDBACK_URL`を削除して次のreleaseを作成します。既に配布済みのbinaryからの送信も
即時停止する場合は、あわせてWorkerを削除します。

```console
wrangler delete
```
