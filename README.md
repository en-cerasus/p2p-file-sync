# P2P Sync

WindowsとiOS向けの独立P2P同期アプリ。現在はRust共通コアの初期PoCで、両アプリからABIバージョンの取得とSHA-256計算を呼び出せます。ピア接続とファイル同期は未実装です。

## Windows x64

RustのMSVCツールチェーン、Visual StudioのC++ビルドツールとWindows SDK、.NET 10 SDKが必要です。

```powershell
cargo test --locked --workspace
cargo build --locked --release
dotnet run --project windows/P2PSync.csproj -c Release
```

画面で `abc` を計算すると `ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad` が表示されます。

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
