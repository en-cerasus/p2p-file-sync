# P2P Sync

WindowsとiOS向けの独立P2P同期アプリ。手動設定した相手から、新規ファイルを相互TLSで受信する試験版です。

Windows・iOSの同期画面は、同期後に未解決の競合候補を表示します。候補の名前、発行元ピアIDの短縮形、版番号、取得済み本体の容量を確認できます。削除候補と本体未取得の候補も区別します。48時間の履歴は、同じ内容の本体を一度だけ数えた保存容量と、未取得の本体数を表示します。Windowsでは書き込みピア設定と状態フォルダーを選ぶと、履歴を確認し、現在の版を確認したうえで新しい同期変更として復元できます。iOSの画面からの復元は未実装です。競合の採用版を選ぶ操作は現在、書き込みピアの `resolve` コマンドで行います。

同期後は、その端末に保存された48時間以内の履歴も表示します。元のパス、容量、アーカイブ日時を確認できます。履歴からの復元は、現在は書き込みピアの `restore` コマンドを使います。

## Windows x64

RustのMSVCツールチェーン、Visual StudioのC++ビルドツールとWindows SDK、.NET 10 SDKが必要です。

```powershell
cargo test --locked --workspace
cargo build --locked --release
dotnet run --project windows/P2PSync.csproj -c Release
```

画面で接続設定のJSONと保存先フォルダーを選び、「同期を開始」を押します。「10 秒ごとに自動同期」を有効にする場合は、前回の同期完了から10秒後に次回の受信を開始します。最小化すると通知領域へ移り、自動同期を続けます。通知領域のメニューから画面を開く・同期する・終了する操作ができます。ウィンドウを閉じるとアプリは終了します。選んだ設定・保存先・自動同期の選択は次回起動時に復元します。

## iOS

Xcode搭載のMacで実行します。iPhoneOS向けの未署名IPAを `ios/out/P2PSync-unsigned.ipa` に出力します。

```bash
rustup target add aarch64-apple-ios
bash ios/build-ipa.sh
```

GitHub Actionsの「Build unsigned iOS IPA」も手動で実行できます。後署名、iPhone 11へのインストール、起動は別の確認手順です。

`core/include/p2p_sync_core.h` が共通C ABIです。Swiftの `SyncCore` はFoundationのみを使い、SwiftUIの画面を経由せず呼び出せます。

- [製品仕様](SPEC.md)
- [設計レビュー](DESIGN_REVIEW.md)
- [実装ロードマップ](IMPLEMENTATION_ROADMAP.md)

## 直接接続の試験設定

試験用の証明書・秘密鍵と接続設定は配信コマンドの `setup` で生成できます。WindowsはJSONを選択します。iOSはJSONと証明書類を入れたフォルダーを選択します。iOSのJSON名は `direct.json`、同期先はアプリのDocuments内の `P2PSync` です。iOSでは選んだ接続設定フォルダーと自動同期の設定を保存します。「自動同期」は画面を開いている間に10秒間隔で実行します。画面を離れたあともiOSがバックグラウンド更新を許可した時点で同期を試します。実行時刻はOSが決定します。同期中に画面を離れると、その回の同期を続けるための実行時間をiOSに要求します。期限切れの場合は通信の区切りで中止を試み、次回同期で保存済みのチャンクを再利用できます。

`setup` の末尾に読み取り専用ピア数と書き込みピア数を指定できます。例えば `1 2` は読み取り専用ピア1台と、個別の鍵・証明書を持つ書き込みピア2台を生成します。書き込み用フォルダーは `sender`、`writer-2` の順です。各ピアは自分の `server.json` で取得済みデータを配信できます。待受ポートは書き込みピア、読み取り専用ピアの順に指定値から連番になります。各フォルダーの `direct.json` には他の書き込み・読み取り専用ピアを最大9台まで接続候補として登録します。すべてのピアが同じIPアドレスで待ち受ける試験設定なので、別の端末へ配布するときは各候補のアドレスを実際の端末に合わせて変更してください。読み取り専用ピアで `catch-up` と `serve-peer` を使う場合も、そのピア自身の `server.json` を指定します。読み取り専用ピアによる変更イベントの発行は拒否されます。各端末に配布するのはその端末専用のフォルダーだけです。

|JSON項目|内容|
|---|---|
|`share`|共有IDを32桁の16進数で指定|
|`generation`|共有の世代番号。1以上|
|`members`|Ed25519公開鍵を64桁の16進数キーとし、値を `reader` または `writer` とするオブジェクト|
|`address`|配信ピアのIPアドレスとTCPポート|
|`server_name`|配信ピアの証明書に含まれる名前|
|`server_pin`|配信ピアの証明書DERのSHA-256を64桁の16進数で指定|
|`fallback_peers`|追加の接続先。各要素に `address`、`server_name`、`server_pin` と任意の `connect_timeout_ms`（100〜15000）を指定し、最大8件。各ピアは同じ共有・世代・メンバー設定を使用|
|`ca_der`、`certificate_der`、`private_key_der`|それぞれCA証明書、クライアント証明書、クライアント秘密鍵のDERファイル名。JSONからの相対パスを使用可能|
|`event_key`|確認の署名に使う受信側Ed25519秘密鍵の32バイトseed。対応する公開鍵は `members` の `reader` に登録。旧設定では省略可能|

受信側は追加の接続先も設定順に試し、イベントとファイルキャッシュを引き継ぎます。署名付き確認は接続できた各ピアへ送信します。`setup` が生成する追加候補は接続開始だけ1.5秒で打ち切り、接続後の通信は通常の15秒待機を使います。別のネットワークで必要なら `connect_timeout_ms` を変更できます。

鍵や接続設定は信頼できるローカルファイルを使用してください。接続先の登録・配信開始・証明書発行の画面と、Windowsは、同期先で反映済みの版と一致するファイルを更新・削除できます。

### Windowsからの試験配信

同じPCで試す場合は、アクセス権を管理できる作業フォルダーで、まだ存在しない `trial-setup` を指定します。

```powershell
cargo run --locked --release -p p2p-sync-core --bin p2p-send -- setup .\trial-setup 127.0.0.1:8765 127.0.0.1:8765
```

`sender/server.json` と `receiver/direct.json` が生成されます。別端末で試すときは、受信端末から到達可能な配信PCのIPアドレスを、待受と接続先に指定してください。受信端末へ移すのは `receiver` フォルダーだけです。どちらにも秘密鍵があるため、公開フォルダーやGitに追加しないでください。試験証明書の有効期間は30日です。複数台で試す場合はsetupコマンドの末尾に台数（1〜32）を指定し、各受信端末に異なる receiver-N フォルダーを移します。

手動で設定する場合は、
受信設定と同じ `share`、`generation`、`members` を持つ `server.json` を用意します。配信には次のフィールドを指定します。

|JSON項目|内容|
|---|---|
|`bind`|TCP待受のアドレスとポート。同一PCでの試験は `127.0.0.1:8765`|
|`client_pin`|受信側証明書DERのSHA-256を64桁の16進数で指定。複数台の場合は client_pins 配列を指定|
|`ca_der`、`certificate_der`、`private_key_der`|CA証明書、配信側証明書、配信側秘密鍵のDERファイル名|
|`event_key`|配信側Ed25519秘密鍵の32バイトseedを保存したファイル。対応する公開鍵を `members` に `writer` として登録|

ルート直下に共有するファイルを登録してから配信を開始します。同じ状態フォルダーを両コマンドに渡します。停止はCtrl+Cです。

```powershell
cargo run --locked --release -p p2p-sync-core --bin p2p-send -- publish .\trial-setup\sender\server.json .\server-state .\song.tja
cargo run --locked --release -p p2p-sync-core --bin p2p-send -- serve .\trial-setup\sender\server.json .\server-state
```

現在は受信端末の証明書を事前登録する手動接続です。配信開始のGUIは未実装です。

配信側と受信側は同じ共有ID・世代を設定し、両方のアプリを更新してください。共有ID・世代が異なる接続と、従来の世代照合を行わない接続方式は、TLS接続時に拒否されます。

`p2p-send prepare-generation <server.json> <state-dir> <next-server.json> <administrator-seed-file> <new-dir>` で移行用データを作成できます。次の設定には同じ共有IDと1つ進んだ世代、新しいピア一覧を指定します。管理鍵には、同期ピアの署名鍵と別の32バイトのEd25519シードを使います。出力には署名済みの `transition.bin`、`checkpoint.bin`、検証済みの `files` が含まれます。出力フォルダーには共有ファイル本体が含まれるため、共有先と同様に管理してください。作成だけでは世代は切り替わりません。

`p2p-send verify-generation <previous-server.json> <next-server.json> <administrator-public-key> <bundle-dir>` は、信頼済みの管理公開鍵（Ed25519の生32バイト）を使って、署名、世代、ピア一覧、履歴、ファイル本体を検証します。検証は読み取り専用です。現段階のチェックポイント上限は128 MiBで、ファイル本体はこの上限に含みません。

新世代の配信・受信設定には `migration` を追加し、`previous_server`、`administrator_public_key`、`bundle` にそれぞれ旧世代のピア構成、信頼済みの管理公開鍵、検証済みの移行データのパスを指定します。パスは各設定ファイルからの相対パスを使えます。移行データは配信元・同期先・アプリのキャッシュ外に置き、再起動後も保持します。新世代へ移った端末同士は同じ署名済み移行データを使います。同じローカル保存領域では、有効化後の新しい配信・受信操作に新世代が必要になります。別の保存領域や、移行をまだ適用していない端末の旧世代セッションには影響しません。

ピアを登録解除する場合は、新世代の `members` から署名公開鍵を外し、各配信設定の `client_pin` または `client_pins` から同ピアの証明書も外します。どちらの一覧も現段階では手動で更新します。

`p2p-send catch-up <server.json> <state-dir> <direct.json>` は、同じ共有・世代で登録された別の配信先から、署名付きイベントと取得可能なファイル本体を配信用キャッシュへ取り込みます。取り込んだデータは `serve` で再配信でき、作業フォルダーには書き込みません。接続には `direct.json` の主接続先と追加の接続先を設定順に使います。

同じ端末の受信状態にある署名付き確認を配信キャッシュへ取り込むには、`p2p-send import-confirmations <server.json> <server-state-dir> <direct.json> <receiver-state-dir> <destination>` を使います。`destination` は受信アプリが同期したフォルダーです。共有・世代と署名を検証し、配信キャッシュがまだ持たない変更イベントへの確認は次回の取り込みまで保留します。

`p2p-send serve-peer <server.json> <state-dir> <direct.json> <間隔秒> [receiver-state-dir destination]` は、配信しながら取り込みを繰り返します。末尾の2項目を指定すると、毎周期、同じ端末の受信状態から確認通知も取り込みます。接続に失敗した周期でもローカル通知の取り込みは試みます。間隔は1〜86400秒で、接続に失敗した場合も次の周期で再試行します。

配信側は、更新・削除から48時間を過ぎた旧版を、全登録ピアの確認がそろい、現行版・未解決の競合・期限内の履歴からも参照されなくなった時点で整理します。削除イベントは同期に必要なため保持します。同期接続では署名付きの反映確認を双方で交換し、中継ピア経由で元の配信元にも届けます。確認対象の変更イベントがまだ届いていないピアは確認を適用せず、次回接続で再送します。同じ端末の受信状態からCLIの配信キャッシュへの取り込みは、上記の `serve-peer` の引数を指定したときに定期実行します。

配信側の状態に競合がある場合は、`p2p-send conflicts <server.json> <state-dir>` で候補IDを確認できます。`p2p-send resolve <server.json> <state-dir> <採用する候補ID> <ほかの候補ID> ...` では、確認した候補をすべて指定します。候補が増減した場合は再確認が必要です。削除候補を採用すると共有上の削除を維持します。

`p2p-send history <server.json> <state-dir>` は48時間以内の履歴と、同じファイルの現在の版IDを表示します。`p2p-send restore <server.json> <state-dir> <archive-id> <reviewed-current-id>` は、選んだ履歴を新しい署名付き変更として復元します。確認した現在の版が変わった場合、競合中の場合、復元先に別の共有項目がある場合は保留します。ファイルIDを維持し、置き換える現在の内容も履歴へ保存します。フォルダー自体の復元とGUIからの復元は今後の対応です。

譜面集をまとめて登録する場合は、`publish` の代わりに `publish-tree` を使います。

```powershell
cargo run --locked --release -p p2p-sync-core --bin p2p-send -- publish-tree .\trial-setup\sender\server.json .\server-state .\譜面集
```

指定フォルダー内の階層と空フォルダーも登録します。同じ内容の再登録は既存イベントを再利用します。

登録済みファイルを更新・削除・移動するには、共有内の相対パスをスラッシュ区切りで指定します。

```powershell
cargo run --locked --release -p p2p-sync-core --bin p2p-send -- update .\trial-setup\sender\server.json .\server-state '譜面/song.tja' .\最新版.tja
cargo run --locked --release -p p2p-sync-core --bin p2p-send -- delete .\trial-setup\sender\server.json .\server-state '譜面/song.tja'
cargo run --locked --release -p p2p-sync-core --bin p2p-send -- move .\trial-setup\sender\server.json .\server-state '譜面/song.tja' '譜面/renamed.tja'
```

各コマンドは共有上のイベントを記録します。配信時に指定した元ファイルは引き続きローカルに残ります。`move` は共有上のファイルIDと内容を維持し、既に共有に登録されたフォルダーへ移動できます。初回同期では移動後のパスに配置されます。Windowsの反映済みファイルは、内容が一致し、移動先が空いている場合に移動します。Windowsでは大文字・小文字だけが違う改名も反映します。Windowsでは移動と内容更新をまとめて受信した場合も反映します。iOSでの反映済みファイルの移動は保留になります。Windowsの受信側は、最後に反映した版と一致し、他のアプリが開いていない場合に更新・削除します。更新中は旧版を一時退避してから新版を配置し、途中停止時は次回同期で再開します。ローカルで変更済みのファイル、iOSでの更新・削除は保留と表示されます。

配信元フォルダーを再スキャンして同じパスの変更をまとめて送る場合は、`refresh-tree` を使います。新しいファイルは追加し、見つからないファイルは共有に残します。削除済みパスにファイルが残っている場合は、競合として報告します。

配信中に自動で再スキャンする場合は、末尾に間隔を秒で指定します。

```powershell
cargo run --locked --release -p p2p-sync-core --bin p2p-send -- serve-tree .\trial-setup\sender\server.json .\server-state .\譜面集 10
```

起動時と指定間隔で配信元を読み取り、追加と変更を登録します。設定・秘密鍵・配信状態のフォルダーは、共有するフォルダーの外に配置してください。
