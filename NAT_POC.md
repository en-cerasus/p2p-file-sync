# NAT越え実機検証メモ

現在の`signaling/`はWebRTCの接続情報だけを交換するCloudflare Workers / Durable Objectsの試験実装。`/diagnostic.html`は、異なる回線のWindowsブラウザーとiPhoneブラウザーでDataChannelの接続経路を確認するための画面。同期ファイルは送らない。同期アプリ自身のNAT越えはまだ未実装。

## 検証の順序

1. Cloudflareへ公開する前に、`signaling`で`npm install`と`npm run check`を実行する。CIの「Check Cloudflare signaling」でも型検査と公開なしのビルドを確認できる。
2. 公開を許可されたCloudflareアカウントでWorkerをデプロイする。ここで作成するDurable ObjectsはSQLiteバックエンドを使い、無料プランに対応する設定とする。現時点ではデプロイしていない。
3. 16バイトの乱数を16進数32桁にした秘密のルーム値を作る。WindowsとiPhoneの`https://<WorkerのURL>/diagnostic.html`で同じ値を入力する。両端末は異なるLANまたは片方をモバイル回線にする。
4. TURN資格情報を空欄にして接続し、DataChannelの応答と選択候補の種類を記録する。直接接続できない回線では失敗してよい。
5. TURN使用を別途許可した場合のみ、Cloudflare Realtimeが発行した短期資格情報を両端末に入れて再試行する。`relay`候補が選ばれた場合だけTURN中継として記録する。実転送量と請求画面の使用量を確認する。

## 残る実装

- Windows・iOSアプリ内にWebRTC接続を組み込み、DataChannelをRustの`channel_stream`へ接続する。現状の手動設定の共有ID・証明書ピン・端末IDを使う。
- 接続候補を直接優先にし、直接接続失敗時にTURNを使用する。TURNの利用者上限とGUI使用量表示を追加する。上限に達した時は中継を閉じる。
- 別LANのWindowsとiPhone 11で、同期ファイルの取得・切断後の再開まで実測する。

Cloudflare Realtime TURNはSTUNと異なり、無料枠を超えると料金が生じ得る。現在の仕様では課金が生じ得るサービスを明示的承認なく作成・利用しない。
