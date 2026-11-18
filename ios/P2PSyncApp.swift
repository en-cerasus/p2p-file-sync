import Foundation
import SwiftUI
import UniformTypeIdentifiers

@main
struct P2PSyncApp: App {
    var body: some Scene {
        WindowGroup {
            NavigationStack { DirectSyncView() }
        }
    }
}

@MainActor
struct DirectSyncView: View {
    @Environment(\.scenePhase) private var phase
    @State private var configFolder: URL?
    @State private var pickingFolder = false
    @State private var syncing = false
    @State private var automaticSync = false
    @State private var result = ""

    private var shouldPoll: Bool {
        automaticSync && configFolder != nil && phase == .active && !pickingFolder
    }

    var body: some View {
        Form {
            Section {
                Button("接続設定フォルダーを選択") { pickingFolder = true }
                if let configFolder {
                    Text(configFolder.lastPathComponent)
                        .foregroundStyle(.secondary)
                }
                Button("同期を開始") { Task { await sync() } }
                    .disabled(configFolder == nil)
            }
            .disabled(syncing)

            Toggle("10 秒ごとに自動同期", isOn: $automaticSync)
                .disabled(configFolder == nil)

            if syncing {
                ProgressView("同期中…")
            }
            if !result.isEmpty {
                Text(result)
            }
        }
        .navigationTitle("直接同期")
        .task(id: shouldPoll) {
            guard shouldPoll else { return }
            while !Task.isCancelled && shouldPoll {
                await sync()
                do {
                    try await Task.sleep(for: .seconds(10))
                } catch {
                    return
                }
            }
        }
        .fileImporter(isPresented: $pickingFolder, allowedContentTypes: [.folder]) { selection in
            switch selection {
            case .success(let url): configFolder = url
            case .failure: result = "接続設定フォルダーを開けませんでした。"
            }
        }
    }

    private func sync() async {
        guard let configFolder, !syncing, phase == .active, !pickingFolder else { return }
        syncing = true
        result = ""
        defer { syncing = false }
        do {
            let summary = try await Task.detached(priority: .userInitiated) {
                let accessing = configFolder.startAccessingSecurityScopedResource()
                defer {
                    if accessing { configFolder.stopAccessingSecurityScopedResource() }
                }

                let manager = FileManager.default
                let state = try manager.url(for: .applicationSupportDirectory, in: .userDomainMask,
                                            appropriateFor: nil, create: true)
                    .appendingPathComponent("P2PSync", isDirectory: true)
                let destination = try manager.url(for: .documentDirectory, in: .userDomainMask,
                                                  appropriateFor: nil, create: true)
                    .appendingPathComponent("P2PSync", isDirectory: true)
                try manager.createDirectory(at: destination, withIntermediateDirectories: true)
                return try SyncCore.pull(
                    configuration: configFolder.appendingPathComponent("direct.json"),
                    state: state, destination: destination
                )
            }.value
            result = "イベント \(summary.eventsAdded) 件、新規 \(summary.filesPublished) 件、更新 \(summary.filesUpdated) 件、削除 \(summary.filesRemoved) 件、変更なし \(summary.filesPresent) 件、保留 \(summary.filesBlocked) 件"
        } catch SyncCore.CoreError.pullFailed(let status) {
            switch status {
            case 1, 2: result = "接続設定または保存先を確認してください。"
            case 3: result = "設定ファイルや保存領域にアクセスできませんでした。"
            case 4: result = "TLS 証明書または秘密鍵を読み込めませんでした。"
            default: result = "同期に失敗しました。接続先、証明書、同期先を確認してください。"
            }
        } catch {
            result = "同期先を開けませんでした。"
        }
    }
}
