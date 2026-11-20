# NAT越え実機検証メモ

現在の`signaling/`はWebRTCの接続情報だけを交換するCloudflare Workers / Durable Objectsの試験実装。`/diagnostic.html`は、異なる回線のWindowsブラウザーとiPhoneブラウザーでDataChannelの接続経路を確認するための画面。同期ファイルは送らない。同期アプリ自身のNAT越えはまだ未実装。

## 検証の順序

1. Cloudflareへ公開する前に、`signaling`で`npm install`と`npm run check`を実行する。CIの「Check Cloudflare signaling」でも型検査と公開なしのビルドを確認できる。
2. 公開を許可されたCloudflareアカウントでWorkerをデプロイする。ここで作成するDurable ObjectsはSQLiteバックエンドを使い、無料プランに対応する設定とする。現時点ではデプロイしていない。
3. 16バイトの乱数を16進数32桁にした秘密のルーム値を作る。WindowsとiPhoneの`https://<WorkerのURL>/diagnostic.html`で同じ値を入力する。両端末は異なるLANまたは片方をモバイル回線にする。
4. Windowsネイティブ経路を試す場合は、CIの「Build Windows WebRTC route probe」成果物を使う。iPhoneでは診断画面を開き、Windowsで`p2p-rtc-poc.exe wss://<WorkerのURL>/signal <ルーム値>`を実行する。`DataChannel pong`が表示されればWindowsネイティブ実装とiPhoneブラウザー間で経路が成立した。試験用ピアIDは起動のたびに生成する。
5. TURN資格情報を空欄にして接続し、DataChannelの応答と選択候補の種類を記録する。直接接続できない回線では失敗してよい。
6. TURN使用を別途許可した場合のみ、Cloudflare Realtimeが発行した短期資格情報を両端末に入れて再試行する。Windows側はコマンド末尾にユーザー名とパスワードを指定する。`relay`候補が選ばれた場合だけTURN中継として記録する。実転送量と請求画面の使用量を確認する。

## 既存の同期処理を通すWindows間の試験

Windows用PoCは環境変数`P2P_RTC_TCP_CONNECT`または`P2P_RTC_TCP_LISTEN`を指定すると、DataChannelとローカルTCP接続の間でバイト列を転送する。指定できるアドレスは`127.0.0.1`などのループバックのみ。元の相互TLSと証明書ピンはDataChannelの内側でそのまま検証する。

1. 配信側は既存の`p2p-send setup`で`server.json`の待受を`127.0.0.1:8765`にし、`publish`後に`serve`を起動する。
2. 配信側で`P2P_RTC_TCP_CONNECT=127.0.0.1:8765`を設定し、Windows用PoCを起動する。
3. 受信側で`P2P_RTC_TCP_LISTEN=127.0.0.1:9865`を設定し、同じルーム値でWindows用PoCを起動する。受信側の`direct.json`の`address`だけを`127.0.0.1:9865`へ変更する。証明書ピンは変更しない。
4. `Local TCP bridge listening`と`Local TCP bridge connected`が出た後、受信側GUIから同期する。CLIで試す場合は`p2p-send pull <direct.json> <receiver-state-dir> <destination>`を使う。接続が切れたらPoCを再起動する。現時点では同時に1接続だけ扱う。

この試験はWindows間の同期経路に限る。iOSアプリへのWebRTC組み込みと別LANの実機試験は未完了。

## 残る実装

- Windows・iOSアプリ内にWebRTC接続を組み込み、DataChannelをRustの`channel_stream`へ接続する。現状の手動設定の共有ID・証明書ピン・端末IDを使う。
- 接続候補を直接優先にし、直接接続失敗時にTURNを使用する。TURNの利用者上限とGUI使用量表示を追加する。上限に達した時は中継を閉じる。
- 別LANのWindowsとiPhone 11で、同期ファイルの取得・切断後の再開まで実測する。

Cloudflare Realtime TURNはSTUNと異なり、無料枠を超えると料金が生じ得る。現在の仕様では課金が生じ得るサービスを明示的承認なく作成・利用しない。
