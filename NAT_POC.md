# NAT越え実機検証メモ

`signaling/`はWebRTCの接続情報だけを交換するCloudflare Workers / Durable Objectsの試験実装。`/diagnostic.html`は、異なる回線のWindowsブラウザーとiPhoneブラウザーでDataChannelの接続経路を確認するための画面。同期ファイルは送らない。同期アプリでは、手動設定でWebRTC経路を有効にできる。

## 検証の順序

1. Cloudflareへ公開する前に、`signaling`で`npm install`と`npm run check`を実行する。CIの「Check Cloudflare signaling」でも型検査と公開なしのビルドを確認できる。
2. 公開を許可されたCloudflareアカウントでWorkerをデプロイする。ここで作成するDurable ObjectsはSQLiteバックエンドを使い、無料プランに対応する設定とする。2026-11-26に既存の`signaling/wrangler.jsonc`からデプロイし、公開URLの応答を確認した。TURNは作成・利用していない。
3. 16バイトの乱数を16進数32桁にした秘密のルーム値を作る。WindowsとiPhoneの`https://<WorkerのURL>/diagnostic.html`で同じ値を入力する。両端末は異なるLANまたは片方をモバイル回線にする。
4. Windowsネイティブ経路を試す場合は、CIの「Build Windows WebRTC route probe」成果物を使う。iPhoneでは診断画面を開き、Windowsで`p2p-rtc-poc.exe wss://<WorkerのURL>/signal <ルーム値>`を実行する。`DataChannel pong`が表示されればWindowsネイティブ実装とiPhoneブラウザー間で経路が成立した。試験用ピアIDは起動のたびに生成する。
5. TURN資格情報を空欄にして接続し、DataChannelの応答と選択候補の種類を記録する。直接接続できない回線では失敗してよい。
6. TURN使用を別途許可した場合のみ、[Cloudflareの手順](https://developers.cloudflare.com/realtime/turn/generate-credentials/)でTURNキーから有効期限付きの`username`と`credential`を発行し、両端末の`turn_username`と`turn_password`へ設定して再試行する。TURNキー自体は端末の設定へ入れない。診断用Windowsコマンドでは末尾に発行済みユーザー名とパスワードを指定する。`relay`候補が選ばれた場合だけTURN中継として記録する。実転送量と請求画面の使用量を確認する。

## 既存の同期処理を通すWindows間の試験

Rust共通コアの`p2p-send rtc-bridge <rtc.json>`は、DataChannelとローカルTCP接続の間でバイト列を転送する。`p2p_sync_rtc_bridge_run_v1`からも同じ処理を呼べる。指定できるTCPアドレスは`127.0.0.1`などのループバックのみ。元の相互TLSと証明書ピンはDataChannelの内側でそのまま検証する。`rtc-poc`の環境変数方式も診断用に残している。

Windowsの「Test and build Windows client」成果物には、GUIと共通コアDLLに加えて`p2p-send.exe`を同梱する。手動設定の`setup`とCLI経由の経路確認に使用できる。

`rtc.json`の例。ルーム値は16バイトの乱数を16進数32桁にしたもので、両端末に同じ値を設定する。`signal_url`には公開済みWorkerの`wss://.../signal`を指定する。TURNを許可して試す場合だけ、短期の`turn_username`と`turn_password`に加え、端末ごとの中継送受信量上限`relay_byte_limit`を正のバイト数で指定する。同じ起動中は再接続しても中継量を合算し、上限を超えたら経路を切断する。クライアント側の上限はCloudflareアカウント全体の請求額を保証しない。

```json
{"signal_url":"wss://<WorkerのURL>/signal","room":"0123456789abcdef0123456789abcdef","local_tcp":"127.0.0.1:8765","mode":"connect"}
```

TURNを試すときは上のJSONに`"turn_username":"<発行されたusername>"`、`"turn_password":"<発行されたcredential>"`、`"relay_byte_limit":10000000`を加える。受信側も同様に設定する。[Cloudflareの料金と無料枠](https://developers.cloudflare.com/realtime/turn/faq/)は実利用の直前に再確認する。

1. 配信側は既存の`p2p-send setup`で`server.json`の待受を`127.0.0.1:8765`にし、`publish`後に`serve`を起動する。
2. 配信側の`rtc.json`は`mode: "connect"`と`local_tcp: "127.0.0.1:8765"`にして、`p2p-send rtc-bridge`を起動する。
3. 受信側の`rtc.json`は`mode: "listen"`と`local_tcp: "127.0.0.1:9865"`にして、同じルーム値で`p2p-send rtc-bridge`を起動する。受信側の`direct.json`の`address`だけを`127.0.0.1:9865`へ変更する。証明書ピンは変更しない。
4. 受信側に`RTC ready`が出た後、GUIから同期する。CLIで試す場合は、先に`<destination>`フォルダーを作り、`p2p-send pull <direct.json> <receiver-state-dir> <destination>`を使う。同時に1接続だけ扱う。切断後はWebRTC経路を作り直し、受信側で同期を再実行できる。取得済みの内容は既存の同期状態に保存する。

WindowsとiOSのアプリでは、選択した既存設定フォルダーに`rtc.json`を置くと、配信開始時または同期開始時にRust共通コアのWebRTC経路を起動する。配信側は`connect`、受信側は`listen`とし、受信側の`direct.json`の接続先は`rtc.json`のループバックアドレスに合わせる。`rtc.json`がなければ従来の直接TCPを使う。Windows・iOSのビルドとローカルWebRTC通信試験は成功済み。ローカル試験では接続を作り直した後の2回目の100KB転送と、試験用TURNを強制した100KB転送も成功した。後者はCloudflareの実サービスを使っていない。別LANの実機試験とiPhone 11への署名・インストールは未完了。

同じローカルTURNを利用可能にした通常のICE設定では、直接経路が選ばれた。直接UDPだけを遮断した場合は、設定を変えずにTURN中継へ切り替わり、100KBを転送できた。これはGitHub Actions内の試験用TURNサーバーでの確認であり、別LANやCloudflare Realtime TURNでの実測ではない。

2026-11-26にWindowsの`p2p-rtc-poc.exe`とChromeの診断画面を、ローカルWorker経由で接続した。双方でDataChannelの応答を確認し、Chrome側の選択経路は`host → host`（直接接続）だった。Rust側が1行を複数のDataChannelメッセージに分けて送る場合にも診断画面が行を組み立てるよう修正した。この試験は同じPC上で行い、iPhone Safariや別LANの確認ではない。

同日、ローカルWorkerを介してWindows上の配信側と受信側それぞれで`p2p-send rtc-bridge`を起動し、既存の相互TLS同期をDataChannel越しに実行した。受信側の`direct.json`の接続先だけをローカル待受ポートへ変更した。`pull`は1イベント・1ファイルを取得し、元ファイルと取得ファイルのSHA-256が一致した。両側の経路ログは`direct`、`relay_total=0`を示した。これは同一PC上の二つのWindowsプロセスでの確認であり、別LANやiOSでの転送成功を示さない。

到達不能なローカル試験用TURNだけを候補にした場合、両側のICEが約30秒で失敗を通知し、WebRTC経路が再接続へ進むことを確認した。通常の直接候補へ戻してファイル同期を再実行し、取得ファイルのSHA-256一致を確認した。短時間で終わる転送では、経路統計が出る前に切断される側もある。これらはローカルWorkerでの試験であり、Cloudflare Realtime TURNの実測ではない。

2026-11-26に公開済みCloudflare WorkerのHTTPS診断画面が`200`を返し、WSSで接続した2ピア間のシグナリング転送を確認した。さらに同じPC上のWindows配信・受信プロセスをそのWorker経由で接続し、DataChannel上の`pull`で1ファイルを取得した。元ファイルと取得ファイルのSHA-256は一致し、両側の経路ログは`direct`、`relay_total=0`だった。別LAN、iPhone 11、Cloudflare Realtime TURNでの実測は残る。

### iPhone 11との別回線試験の準備（2026-11-26）

GitHub Actionsの既存iOSビルドを`364ea79`から実行し、未署名IPAの生成が成功した（[実行結果](https://github.com/en-cerasus/p2p-file-sync/actions/runs/45771039337)）。利用者からiPhone 11へのIPAインストール完了の報告を受けた。アプリでの同期動作は未確認。

Windowsの検証用に、既存の共有や同期状態とは別の無視対象`.toolchain/nat-device-20261126/`へ新規ピア設定・送受信用状態・ランダムな2 MiBのファイルを作成した。配信側はループバック`127.0.0.1:41985`で起動し、公開WorkerへのWebRTCシグナリング待機を確認した。iPhone用の受信設定は暗号鍵を含むためGitHubへ登録せず、私的な転送経路だけで端末へ渡す。ブラウザー診断用ルームと同期用ルームは分離した。TURN設定はどちらにも入れていない。

試験ファイルのSHA-256は`7A324B62EAC7A4AEEE86FDF9206427C1C62AD84A447C993D4B3DA6D06CE10AF3`。iPhoneから取得できた場合に、受信後のファイルと照合する。Rust共通コアの`resumable_chunks`テスト2件はローカルで成功した。これは中断再開のロジック確認であり、iOS実機での中断再開結果ではない。

iPhoneへ渡すものと同一の`direct.json`・証明書・鍵を使い、Windowsの新規受信状態からループバックTCPで2 MiBの試験ファイルを取得した。別の新規受信状態でも再取得でき、どちらも送信元とのSHA-256一致を確認した。これにより今回の配信内容と受信設定をローカルで確認できたが、WebRTC経路やiOSでの動作を証明する試験ではない。

Windowsのネイティブ診断プロセスは公開Workerの別ルームで待機を確認した。2026-11-26 19:31 JST、Wi-FiをオフにしたiPhone 11のSafari画面には同じWindowsピアへの`connected`、`DataChannel接続`、`srflx → srflx (直接接続)`、応答`49 ms`が表示された。Windows側ログも`connected`、`DataChannel open`、`DataChannel pong: 44 ms`を記録した。その後Windows側では`disconnected`と`failed`を記録した。候補の種類とiPhone側の回線状態は利用者の報告に基づく。これで別回線のDataChannel直接接続を確認したが、この診断だけではファイル受信・SHA-256・中断再開を証明しない。

設定ZIPは秘密鍵を含むためGitHubには置かず、利用者本人のGoogle Driveへアップロードした。Driveのメタデータで`shared: false`、権限は本人の`owner`のみと確認した。iPhoneでの設定フォルダー選択は次の同期成功報告から確認できた。

利用者はiPhoneへ設定フォルダーを読み込ませ、手動の同期操作で新規1件と表示されたと報告した。iPhoneのFilesアプリで`nat-test-fc028bc9.bin`（2 MiB）が存在することも確認した。WindowsのRTCログではこの同期のDataChannel接続と`route:direct`、`relay_total=0`、約1.95 MBと約0.56 MBの送信カウンタを記録した。カウンタは接続ごとの通信量であり、ファイル内容のハッシュや単一セッションでの完了を直接示さない。

自動同期をオンにした状態でWindowsから50バイトの新規`nat-auto-20261126-1931.txt`をイベント2として登録した。利用者はアプリを前面に置いて待った後、iPhoneのFilesにこのファイルが自動で増えたと報告した。その間のWindows RTCログは`route:direct`と`relay_total=0`を示した。

利用者がiPhoneから本人のDriveへ戻した2 MiBの`nat-test-fc028bc9.bin`をWindowsの隔離フォルダーへ読み戻した。受信後のファイルと元ファイルはともに2,097,152バイトで、SHA-256が`7A324B62EAC7A4AEEE86FDF9206427C1C62AD84A447C993D4B3DA6D06CE10AF3`で一致した。50バイトの自動同期ファイルもDrive上の内容を読み戻して元ファイルと全バイト一致し、元ファイルのSHA-256は`3D23F9726922531E6389311E0B0181B364E221770F00D28943507DD4456137B2`。Drive上の両ファイルは本人のみの権限で、公開リンクを作っていない。

中断・再開試験用に16 MiBの`nat-resume-20261126.bin`をイベント3として登録した。最初の試行では監視スクリプトが書き込み中のRTCログを開けず先に終了し、Windows側が通信を切る前に送信カウンタが約17.75 MBに達した。この試行は転送途中の中断と判定できない。監視を共有読み取りに修正し、経路を再起動した。

別名で同じ内容をイベント4として登録した試行では、受信側に保存済みの内容が重複排除され、ほぼメタデータだけの転送になった。中断試験には使わず、新しい乱数内容の16 MiB `nat-resume-unique-20261126.bin`をイベント5として登録した。改修した監視がWindows側の`route:direct sent=1947958 received=22008 relay_total=0`を検出した時点でWebRTCブリッジを停止した。これは16,777,216バイトのファイル本体より十分小さく、接続中に切断できた。iPhoneアプリは同期エラーを表示し、利用者がFilesで対象名の完成ファイルがないことを確認した。

同じ配信状態とiPhone設定を維持してWindowsのブリッジだけを起動し直し、iPhoneで同期を再実行した。利用者は`nat-resume-unique-20261126.bin`の完成と、先に取得した2 MiBのbinと自動同期のtxtが残ることを確認した。Windows側は再開セッションで`route:direct`、`relay_total=0`、最終送信カウンタ`16202718`を記録した。最初の完全な16 MiB転送の最終カウンタ`17751988`より少なく、保存済みチャンクの再利用と整合する。これらのカウンタだけでは再利用したチャンク数を確定できない。

利用者がiPhoneから本人専用のDriveフォルダーへ戻した`nat-resume-unique-20261126.bin`を読み戻した。送信元と受信後ファイルはともに16,777,216バイトで、SHA-256は`E08E90F316C1D0B6EED520B451E32AF2C65AE3C7435A5C40C9CB565B0F5C9254`で一致した。同じフォルダーへ中断・再開後に戻された既存の`nat-test-fc028bc9.bin`も2,097,152バイトで、送信元とのSHA-256一致を再確認した。自動同期の50バイトtxtも全バイト一致を再確認した。これにより今回のファイル転送、中断後の再実行、既存の2ファイルの内容保全をiPhone 11実機で確認した。TURN中継は使っていない。

接続後、アプリの同期・配信状態には「直接接続」または「TURN中継」と接続ごとの送受信量、起動中のTURN累計量（バイト）が表示される。転送終了時にも最終値を通知し、配信画面では再接続後も最後に確認した経路と量を残す。中継量の上限に達すると通知して切断する。候補がまだ確定していない間は経路を表示しない。

## 残る検証

- Cloudflare Realtime TURNでの別回線中継、直接接続が成立しない回線からのフォールバック、中継上限停止は未実測。課金が生じ得るサービスの作成・利用は別途明示的な承認があるまで実施しない。利用者ごとの上限と事業者側の請求制限は別途検証する。
- iOSのバックグラウンド継続、長時間の自動同期、別のNAT条件や回線切替からの復帰は未実測。今回の自動同期はアプリを前面に置いた試験。

Cloudflare Realtime TURNはSTUNと異なり、無料枠を超えると料金が生じ得る。現在の仕様では課金が生じ得るサービスを明示的承認なく作成・利用しない。
