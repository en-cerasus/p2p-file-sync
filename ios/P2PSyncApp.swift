import Foundation
import SwiftUI
import UIKit
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
private final class SyncExecutionTime {
    private var taskID: UIBackgroundTaskIdentifier = .invalid

    init() {
        taskID = UIApplication.shared.beginBackgroundTask(withName: "Finish P2P synchronization") { [weak self] in
            self?.end()
        }
    }

    func end() {
        let activeTask = taskID
        guard activeTask != .invalid else { return }
        taskID = .invalid
        UIApplication.shared.endBackgroundTask(activeTask)
    }
}

@MainActor
struct DirectSyncView: View {
    @Environment(\.scenePhase) private var phase
    @AppStorage("directConfigBookmark") private var configBookmark = Data()
    @AppStorage("directAutomaticSync") private var automaticSync = false
    @State private var configFolder: URL?
    @State private var pickingFolder = false
    @State private var syncing = false
    @State private var result = ""
    @State private var originals: [SyncCore.ProtectedOriginal] = []

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
            if !originals.isEmpty {
                DisclosureGroup("保護原本の一覧") {
                    ForEach(originals) { original in
                        VStack(alignment: .leading) {
                            LabeledContent(
                                original.names.isEmpty ? "名称不明" : original.names.joined(separator: "、"),
                                value: original.bytes.map { "\($0.formatted()) バイト" } ?? "本体未取得"
                            )
                            if original.conflict {
                                Text("競合未解決").font(.caption)
                            }
                            ForEach(original.waiting) { peer in
                                let status = peer.state == .events ? "変更の受信待ち" : "ピアの確認待ち"
                                Text("\(status)  \(String(peer.peer.prefix(12)))")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                        }
                    }
                }
            }
        }
        .navigationTitle("直接同期")
        .task(id: phase) {
            if phase == .active && configFolder == nil && !pickingFolder {
                restoreConfigFolder()
            }
        }
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
            case .success(let url):
                selectConfigFolder(url)
            case .failure: result = "接続設定フォルダーを開けませんでした。"
            }
        }
    }

    private enum ConfigAccessError: Error {
        case permissionUnavailable
    }

    private func bookmark(for url: URL) throws -> Data {
        guard url.startAccessingSecurityScopedResource() else {
            throw ConfigAccessError.permissionUnavailable
        }
        defer { url.stopAccessingSecurityScopedResource() }
        return try url.bookmarkData(options: .minimalBookmark, includingResourceValuesForKeys: nil, relativeTo: nil)
    }

    private func selectConfigFolder(_ url: URL) {
        do {
            configBookmark = try bookmark(for: url)
            configFolder = url
            originals = []
            result = ""
        } catch {
            result = "接続設定フォルダーへのアクセスを保存できませんでした。"
        }
    }

    private func restoreConfigFolder() {
        guard !configBookmark.isEmpty else { return }
        do {
            var stale = false
            let url = try URL(resolvingBookmarkData: configBookmark, options: [.withoutUI],
                              relativeTo: nil, bookmarkDataIsStale: &stale)
            if stale {
                configBookmark = try bookmark(for: url)
            } else {
                guard url.startAccessingSecurityScopedResource() else {
                    throw ConfigAccessError.permissionUnavailable
                }
                url.stopAccessingSecurityScopedResource()
            }
            configFolder = url
            result = ""
        } catch {
            result = "保存した接続設定フォルダーを開けませんでした。もう一度選択してください。"
        }
    }

    private func sync() async {
        guard let configFolder, !syncing, phase == .active, !pickingFolder else { return }
        syncing = true
        result = ""
        let executionTime = SyncExecutionTime()
        defer {
            executionTime.end()
            syncing = false
        }
        do {
            let summary = try await Task.detached(priority: .userInitiated) {
                guard configFolder.startAccessingSecurityScopedResource() else {
                    throw ConfigAccessError.permissionUnavailable
                }
                defer { configFolder.stopAccessingSecurityScopedResource() }

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
            originals = summary.originals
            result = "イベント \(summary.eventsAdded) 件、新規 \(summary.filesPublished) 件、更新 \(summary.filesUpdated) 件、削除 \(summary.filesRemoved) 件、変更なし \(summary.filesPresent) 件、保留 \(summary.filesBlocked) 件\n保護原本 \(summary.protectedFiles) 件 (\(summary.protectedBytes.formatted()) バイト)、未取得 \(summary.protectedMissing) 件"
        } catch ConfigAccessError.permissionUnavailable {
            self.configFolder = nil
            originals = []
            result = "接続設定フォルダーへのアクセスを失いました。もう一度選択してください。"
        } catch SyncCore.CoreError.pullFailed(let status) {
            switch status {
            case 1, 2: result = "接続設定または保存先を確認してください。"
            case 3: result = "設定ファイルや保存領域にアクセスできませんでした。"
            case 4: result = "TLS 証明書または秘密鍵を読み込めませんでした。"
            default: result = "同期に失敗しました。接続先、証明書、同期先を確認してください。"
            }
        } catch is DecodingError {
            result = "保護原本の一覧を読み込めませんでした。"
        } catch SyncCore.CoreError.missingOriginalsReport {
            result = "保護原本の一覧を読み込めませんでした。"
        } catch {
            result = "同期先を開けませんでした。"
        }
    }
}
