# P2P Sync

WindowsとiOS向けの独立P2P同期アプリ。手動設定した相手から、新規ファイルを相互TLSで受信する試験版です。

## Windows x64

RustのMSVCツールチェーン、Visual StudioのC++ビルドツールとWindows SDK、.NET 10 SDKが必要です。

```powershell
cargo test --locked --workspace
cargo build --locked --release
dotnet run --project windows/P2PSync.csproj -c Release
```

画面で接続設定のJSONと保存先フォルダーを選び、「同期を開始」を押します。

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

試験用の証明書・秘密鍵と接続設定は配信コマンドの `setup` で生成できます。WindowsはJSONを選択します。iOSはJSONと証明書類を入れたフォルダーを選択します。iOSのJSON名は `direct.json`、同期先はアプリのDocuments内の `P2PSync` です。

|JSON項目|内容|
|---|---|
|`share`|共有IDを32桁の16進数で指定|
|`generation`|共有の世代番号。1以上|
|`members`|Ed25519公開鍵を64桁の16進数キーとし、値を `reader` または `writer` とするオブジェクト|
|`address`|配信ピアのIPアドレスとTCPポート|
|`server_name`|配信ピアの証明書に含まれる名前|
|`server_pin`|配信ピアの証明書DERのSHA-256を64桁の16進数で指定|
|`ca_der`、`certificate_der`、`private_key_der`|それぞれCA証明書、クライアント証明書、クライアント秘密鍵のDERファイル名。JSONからの相対パスを使用可能|

鍵や接続設定は信頼できるローカルファイルを使用してください。接続先の登録・配信開始・証明書発行の画面と、同期先の既存ファイルの更新・削除は未実装です。

### Windowsからの試験配信

同じPCで試す場合は、アクセス権を管理できる作業フォルダーで、まだ存在しない `trial-setup` を指定します。

```powershell
cargo run --locked --release -p p2p-sync-core --bin p2p-send -- setup .\trial-setup 127.0.0.1:8765 127.0.0.1:8765
```

`sender/server.json` と `receiver/direct.json` が生成されます。別端末で試すときは、受信端末から到達可能な配信PCのIPアドレスを、待受と接続先に指定してください。受信端末へ移すのは `receiver` フォルダーだけです。どちらにも秘密鍵があるため、公開フォルダーやGitに追加しないでください。試験証明書の有効期間は30日です。

手動で設定する場合は、
受信設定と同じ `share`、`generation`、`members` を持つ `server.json` を用意します。配信には次のフィールドを指定します。

|JSON項目|内容|
|---|---|
|`bind`|TCP待受のアドレスとポート。同一PCでの試験は `127.0.0.1:8765`|
|`client_pin`|受信側証明書DERのSHA-256を64桁の16進数で指定|
|`ca_der`、`certificate_der`、`private_key_der`|CA証明書、配信側証明書、配信側秘密鍵のDERファイル名|
|`event_key`|配信側Ed25519秘密鍵の32バイトseedを保存したファイル。対応する公開鍵を `members` に `writer` として登録|

ルート直下に共有するファイルを登録してから配信を開始します。同じ状態フォルダーを両コマンドに渡します。停止はCtrl+Cです。

```powershell
cargo run --locked --release -p p2p-sync-core --bin p2p-send -- publish .\trial-setup\sender\server.json .\server-state .\song.tja
cargo run --locked --release -p p2p-sync-core --bin p2p-send -- serve .\trial-setup\sender\server.json .\server-state
```

現在は受信側証明書を1件指定する手動接続です。配信開始のGUIは未実装です。
