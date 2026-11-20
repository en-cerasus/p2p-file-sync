import BackgroundTasks
import Foundation
import SwiftUI
import UIKit
import UniformTypeIdentifiers

@main
struct P2PSyncApp: App {
    @Environment(\.scenePhase) private var phase

    var body: some Scene {
        WindowGroup {
            TabView {
                NavigationStack { DirectSyncView() }
                    .tabItem { Label("受信", systemImage: "arrow.down.circle") }
                NavigationStack { WriterView() }
                    .tabItem { Label("配信", systemImage: "arrow.up.circle") }
            }
        }
        .onChange(of: phase) { _, newPhase in
            if newPhase == .background {
                BackgroundSync.schedule()
            }
        }
        .backgroundTask(.appRefresh(BackgroundSync.identifier)) {
            await BackgroundSync.run()
        }
    }
}

@MainActor
private enum BackgroundSync {
    static let identifier = "io.github.en-cerasus.p2p-file-sync.refresh"
    static let bookmarkKey = "directConfigBookmark"
    static let enabledKey = "directAutomaticSync"

    static func schedule() {
        let defaults = UserDefaults.standard
        guard defaults.bool(forKey: enabledKey),
              let bookmark = defaults.data(forKey: bookmarkKey), !bookmark.isEmpty else {
            BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: identifier)
            return
        }
        let request = BGAppRefreshTaskRequest(identifier: identifier)
        request.earliestBeginDate = Date(timeIntervalSinceNow: 15 * 60)
        try? BGTaskScheduler.shared.submit(request)
    }

    static func run() async {
        let defaults = UserDefaults.standard
        guard defaults.bool(forKey: enabledKey),
              let bookmark = defaults.data(forKey: bookmarkKey), !bookmark.isEmpty else { return }
        schedule()
        var stale = false
        guard let folder = try? URL(resolvingBookmarkData: bookmark, options: [.withoutUI],
                                    relativeTo: nil, bookmarkDataIsStale: &stale) else { return }
        _ = try? await SyncRunner.shared.run(folder: folder, cancellation: SyncCore.Cancellation())
    }
}

private actor SyncRunner {
    static let shared = SyncRunner()
    private var running = false

    enum Failure: Error {
        case busy
        case permissionUnavailable
    }

    func run(folder: URL, cancellation: SyncCore.Cancellation) async throws -> SyncCore.PullSummary {
        guard !running else { throw Failure.busy }
        try Task.checkCancellation()
        running = true
        defer { running = false }
        return try await withTaskCancellationHandler {
            try await Task.detached(priority: .utility) {
                guard folder.startAccessingSecurityScopedResource() else {
                    throw Failure.permissionUnavailable
                }
                defer { folder.stopAccessingSecurityScopedResource() }
                let manager = FileManager.default
                let state = try manager.url(for: .applicationSupportDirectory, in: .userDomainMask,
                                            appropriateFor: nil, create: true)
                    .appendingPathComponent("P2PSync", isDirectory: true)
                let destination = try manager.url(for: .documentDirectory, in: .userDomainMask,
                                                  appropriateFor: nil, create: true)
                    .appendingPathComponent("P2PSync", isDirectory: true)
                try manager.createDirectory(at: destination, withIntermediateDirectories: true)
                return try SyncCore.pull(configuration: folder.appendingPathComponent("direct.json"),
                                         state: state, destination: destination, cancellation: cancellation)
            }.value
        } onCancel: {
            cancellation.cancel()
        }
    }
}

@MainActor
private final class SyncExecutionTime {
    private var taskID: UIBackgroundTaskIdentifier = .invalid

    init(cancellation: SyncCore.Cancellation) {
        taskID = UIApplication.shared.beginBackgroundTask(withName: "Finish P2P synchronization") { [weak self] in
            cancellation.cancel()
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
    @AppStorage(BackgroundSync.bookmarkKey) private var configBookmark = Data()
    @AppStorage(BackgroundSync.enabledKey) private var automaticSync = false
    @State private var configFolder: URL?
    @State private var pickingFolder = false
    @State private var syncing = false
    @State private var syncCancellation: SyncCore.Cancellation?
    @State private var stopping = false
    @State private var result = ""
    @State private var originals: [SyncCore.ProtectedOriginal] = []
    @State private var conflicts: [SyncCore.FileConflict] = []
    @State private var history: [SyncCore.HistoryEntry] = []

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

            Toggle("自動同期", isOn: $automaticSync)
                .disabled(configFolder == nil || stopping)

            if syncing {
                ProgressView(stopping ? "同期を中断しています…" : "同期中…")
                Button("同期を中断", role: .cancel) {
                    automaticSync = false
                    stopping = true
                    syncCancellation?.cancel()
                }
                .disabled(stopping)
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
            if !history.isEmpty {
                Section("48時間の履歴") {
                    ForEach(history.indices, id: \.self) { index in
                        let entry = history[index]
                        VStack(alignment: .leading) {
                            Text(entry.path?.joined(separator: "/") ?? "パス不明")
                            Text("\(entry.content.length.formatted()) バイト")
                            Text(Date(timeIntervalSince1970: TimeInterval(entry.archived_at)), style: .date)
                                .font(.caption).foregroundStyle(.secondary)
                            Text(Date(timeIntervalSince1970: TimeInterval(entry.archived_at)), style: .time)
                                .font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }
            }
            if !conflicts.isEmpty {
                Section("未解決の競合") {
                    ForEach(conflicts) { conflict in
                        DisclosureGroup(conflict.versions.compactMap(\.name).first ?? "削除との競合") {
                            ForEach(conflict.versions) { version in
                                VStack(alignment: .leading) {
                                    Text(version.name ?? "削除候補")
                                    Text("\(String(version.peer.prefix(12))) / \(version.sequence)")
                                        .font(.caption).foregroundStyle(.secondary)
                                    if version.kind == "file" {
                                        Text(version.bytes.map { "\($0.formatted()) バイト" } ?? "本体未取得")
                                            .font(.caption)
                                    } else if version.kind == "directory" {
                                        Text("フォルダー").font(.caption)
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        .navigationTitle("直接同期")
        .onChange(of: automaticSync) { _, _ in BackgroundSync.schedule() }
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

    private func bookmark(for url: URL) throws -> Data {
        guard url.startAccessingSecurityScopedResource() else {
            throw SyncRunner.Failure.permissionUnavailable
        }
        defer { url.stopAccessingSecurityScopedResource() }
        return try url.bookmarkData(options: .minimalBookmark, includingResourceValuesForKeys: nil, relativeTo: nil)
    }

    private func selectConfigFolder(_ url: URL) {
        do {
            configBookmark = try bookmark(for: url)
            configFolder = url
            originals = []
            conflicts = []
            history = []
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
                    throw SyncRunner.Failure.permissionUnavailable
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
        let cancellation = SyncCore.Cancellation()
        syncCancellation = cancellation
        stopping = false
        let executionTime = SyncExecutionTime(cancellation: cancellation)
        defer {
            executionTime.end()
            syncing = false
            syncCancellation = nil
            stopping = false
        }
        do {
            // Leaving the foreground stops polling, while UIKit may still
            // grant time for this already-started sync to finish.
            let operation = Task {
                try await SyncRunner.shared.run(folder: configFolder, cancellation: cancellation)
            }
            let summary = try await operation.value
            originals = summary.originals
            conflicts = summary.conflicts
            history = summary.history
            result = "イベント \(summary.eventsAdded) 件、新規 \(summary.filesPublished) 件、更新 \(summary.filesUpdated) 件、削除 \(summary.filesRemoved) 件、変更なし \(summary.filesPresent) 件、保留 \(summary.filesBlocked) 件\n保護原本 \(summary.protectedFiles) 件 (\(summary.protectedBytes.formatted()) バイト)、未取得 \(summary.protectedMissing) 件\n履歴の保存容量 \(summary.historyBytes.formatted()) バイト、未取得 \(summary.historyMissing) 件"
        } catch SyncRunner.Failure.busy {
            result = "別の同期が進行中です。"
        } catch SyncRunner.Failure.permissionUnavailable {
            self.configFolder = nil
            originals = []
            conflicts = []
            history = []
            result = "接続設定フォルダーへのアクセスを失いました。もう一度選択してください。"
        } catch SyncCore.CoreError.pullFailedAfterRecovery(let status, let updated, let removed) {
            result = "保存済みの変更を反映しました（更新 \(updated) 件、削除 \(removed) 件）。\n" + pullFailureMessage(status)
        } catch SyncCore.CoreError.pullFailed(let status) {
            result = pullFailureMessage(status)
        } catch SyncCore.CoreError.pendingFailed(let status) {
            switch status {
            case 8: result = "古い同期状態が復元された可能性があります。共有元を変更せず、状態の復旧が必要です。"
            case 2: result = "接続設定または保存先を確認してください。"
            case 3: result = "保存領域にアクセスできないため、保存済みの変更を反映できませんでした。"
            default: result = "保存済みの変更を反映できませんでした。同期先を確認してください。"
            }
        } catch SyncCore.CoreError.snapshotFailed(let status) {
            switch status {
            case 8: result = "古い同期状態が復元された可能性があります。共有元を変更せず、状態の復旧が必要です。"
            default: result = "同期後の保護原本と履歴を読み込めませんでした。"
            }
        } catch is DecodingError {
            result = "保護原本の一覧を読み込めませんでした。"
        } catch SyncCore.CoreError.missingOriginalsReport {
            result = "保護原本の一覧を読み込めませんでした。"
        } catch {
            result = "同期先を開けませんでした。"
        }
    }

    private func pullFailureMessage(_ status: Int32) -> String {
        switch status {
        case 1, 2: return "接続設定または保存先を確認してください。"
        case 3: return "設定ファイルや保存領域にアクセスできませんでした。"
        case 4: return "TLS 証明書または秘密鍵を読み込めませんでした。"
        case 7: return "同期を中断しました。次回は保存済みのデータを再利用します。"
        case 8: return "古い同期状態が復元された可能性があります。共有元を変更せず、状態の復旧が必要です。"
        default: return "同期に失敗しました。接続先、証明書、同期先を確認してください。"
        }
    }
}
