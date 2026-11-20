# NAT越え実機検証メモ

`signaling/`はWebRTCの接続情報だけを交換するCloudflare Workers / Durable Objectsの試験実装。`/diagnostic.html`は、異なる回線のWindowsブラウザーとiPhoneブラウザーでDataChannelの接続経路を確認するための画面。同期ファイルは送らない。同期アプリでは、手動設定でWebRTC経路を有効にできる。

## 検証の順序

1. Cloudflareへ公開する前に、`signaling`で`npm install`と`npm run check`を実行する。CIの「Check Cloudflare signaling」でも型検査と公開なしのビルドを確認できる。
2. 公開を許可されたCloudflareアカウントでWorkerをデプロイする。ここで作成するDurable ObjectsはSQLiteバックエンドを使い、無料プランに対応する設定とする。現時点ではデプロイしていない。
3. 16バイトの乱数を16進数32桁にした秘密のルーム値を作る。WindowsとiPhoneの`https://<WorkerのURL>/diagnostic.html`で同じ値を入力する。両端末は異なるLANまたは片方をモバイル回線にする。
4. Windowsネイティブ経路を試す場合は、CIの「Build Windows WebRTC route probe」成果物を使う。iPhoneでは診断画面を開き、Windowsで`p2p-rtc-poc.exe wss://<WorkerのURL>/signal <ルーム値>`を実行する。`DataChannel pong`が表示されればWindowsネイティブ実装とiPhoneブラウザー間で経路が成立した。試験用ピアIDは起動のたびに生成する。
5. TURN資格情報を空欄にして接続し、DataChannelの応答と選択候補の種類を記録する。直接接続できない回線では失敗してよい。
6. TURN使用を別途許可した場合のみ、Cloudflare Realtimeが発行した短期資格情報を両端末に入れて再試行する。Windows側はコマンド末尾にユーザー名とパスワードを指定する。`relay`候補が選ばれた場合だけTURN中継として記録する。実転送量と請求画面の使用量を確認する。

## 既存の同期処理を通すWindows間の試験

Rust共通コアの`p2p-send rtc-bridge <rtc.json>`は、DataChannelとローカルTCP接続の間でバイト列を転送する。`p2p_sync_rtc_bridge_run_v1`からも同じ処理を呼べる。指定できるTCPアドレスは`127.0.0.1`などのループバックのみ。元の相互TLSと証明書ピンはDataChannelの内側でそのまま検証する。`rtc-poc`の環境変数方式も診断用に残している。

`rtc.json`の例。ルーム値は16バイトの乱数を16進数32桁にしたもので、両端末に同じ値を設定する。`signal_url`には公開済みWorkerの`wss://.../signal`を指定する。TURNを許可して試す場合だけ、短期の`turn_username`と`turn_password`に加え、端末ごとの中継送受信量上限`relay_byte_limit`を正のバイト数で指定する。同じ起動中は再接続しても中継量を合算し、上限を超えたら経路を切断する。クライアント側の上限はCloudflareアカウント全体の請求額を保証しない。

```json
{"signal_url":"wss://<WorkerのURL>/signal","room":"0123456789abcdef0123456789abcdef","local_tcp":"127.0.0.1:8765","mode":"connect"}
```

1. 配信側は既存の`p2p-send setup`で`server.json`の待受を`127.0.0.1:8765`にし、`publish`後に`serve`を起動する。
2. 配信側の`rtc.json`は`mode: "connect"`と`local_tcp: "127.0.0.1:8765"`にして、`p2p-send rtc-bridge`を起動する。
3. 受信側の`rtc.json`は`mode: "listen"`と`local_tcp: "127.0.0.1:9865"`にして、同じルーム値で`p2p-send rtc-bridge`を起動する。受信側の`direct.json`の`address`だけを`127.0.0.1:9865`へ変更する。証明書ピンは変更しない。
4. 受信側に`RTC ready`が出た後、GUIから同期する。CLIで試す場合は`p2p-send pull <direct.json> <receiver-state-dir> <destination>`を使う。同時に1接続だけ扱う。切断後はWebRTC経路を作り直し、受信側で同期を再実行できる。取得済みの内容は既存の同期状態に保存する。

WindowsとiOSのアプリでは、選択した既存設定フォルダーに`rtc.json`を置くと、配信開始時または同期開始時にRust共通コアのWebRTC経路を起動する。配信側は`connect`、受信側は`listen`とし、受信側の`direct.json`の接続先は`rtc.json`のループバックアドレスに合わせる。`rtc.json`がなければ従来の直接TCPを使う。Windows・iOSのビルドとローカルWebRTC通信試験は成功済み。ローカル試験では接続を作り直した後の2回目の100KB転送も成功した。別LANの実機試験とiPhone 11への署名・インストールは未完了。

接続後、アプリの同期・配信状態には`route:direct`または`route:relay`と送受信量（バイト）が表示される。中継量の上限に達すると`relay-limit`を通知して切断する。候補がまだ確定していない間は経路を表示しない。

## 残る実装

- Windows・iOSアプリで手動設定によるWebRTC接続を実機検証し、失敗時の表示と中断転送の再開を確認する。
- 異なるLANで直接候補とTURN候補の選択を実測し、直接接続の優先と中継上限停止を確認する。利用者ごとの上限と事業者側の請求制限は別途検証する。
- 別LANのWindowsとiPhone 11で、同期ファイルの取得・切断後の再開まで実測する。

Cloudflare Realtime TURNはSTUNと異なり、無料枠を超えると料金が生じ得る。現在の仕様では課金が生じ得るサービスを明示的承認なく作成・利用しない。
