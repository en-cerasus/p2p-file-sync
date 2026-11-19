import Foundation
import SwiftUI
import UniformTypeIdentifiers

/// Foreground writer controls for the independent app. The shared core owns
/// events and content; this view only chooses trusted paths and runs FFI work.
@MainActor
struct WriterView: View {
    @Environment(\.scenePhase) private var phase
    @AppStorage("writerConfigBookmark") private var configBookmark = Data()
    @State private var configFolder: URL?
    @State private var pickingFolder = false
    @State private var refreshing = false
    @State private var catchingUp = false
    @State private var catchUpStopping = false
    @State private var catchUpCancellation: SyncCore.Cancellation?
    @State private var inspecting = false
    @State private var serving = false
    @State private var cancellation: SyncCore.Cancellation?
    @State private var sourceFolder: URL?
    @State private var result = ""
    @State private var serverStatus = "配信停止中"
    @State private var historyEntries: [SyncCore.WriterHistoryEntry] = []
    @State private var conflictEntries: [SyncCore.WriterConflict] = []
    @State private var historyLoaded = false
    @State private var conflictsLoaded = false
    @State private var pendingHistory: SyncCore.WriterHistoryEntry?
    @State private var pendingDeletion: SyncCore.WriterHistoryEntry?
    @State private var pendingChoice: (SyncCore.WriterConflict, SyncCore.WriterConflictVersion)?
    @State private var confirmRestore = false
    @State private var confirmDelete = false
    @State private var confirmResolve = false

    var body: some View {
        Form {
            Section("書き込みピア") {
                Button("配信設定フォルダーを選択") { pickingFolder = true }
                    .disabled(serving || refreshing || catchingUp || inspecting)
                Text(configFolder?.lastPathComponent ?? "設定フォルダーが未選択です")
                    .foregroundStyle(.secondary)
            }
            Section("共有元") {
                Text("このiPhone内の P2PSync / P2PShared")
                Text("Filesアプリでこのフォルダーに譜面・音源を入れます。")
                    .font(.caption).foregroundStyle(.secondary)
                Button("新規・変更ファイルを登録") { Task { await refresh() } }
                    .disabled(configFolder == nil || sourceFolder == nil || refreshing || catchingUp || inspecting || serving)
                if refreshing { ProgressView("共有元を確認しています…") }
            }
            Section("配信") {
                Button("他のピアから取得") { Task { await catchUp() } }
                    .disabled(configFolder == nil || serving || refreshing || catchingUp || inspecting)
                Button("取得を中断") {
                    catchUpCancellation?.cancel()
                    catchUpStopping = true
                }
                    .disabled(!catchingUp || catchUpStopping)
                if catchingUp { ProgressView("ピアから取得しています…") }
                Button("配信を開始") { startServer() }
                    .disabled(configFolder == nil || serving || refreshing || catchingUp || inspecting || phase != .active)
                Button("配信を停止") { stopServer() }
                    .disabled(!serving || cancellation?.isCancelled() == true)
                Text(serverStatus).foregroundStyle(.secondary)
                Text("取得と共有元の登録・更新は配信を停止してから行います。配信は画面を開いている間に動作し、アプリがバックグラウンドへ移ると停止します。")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("履歴") {
                Button("復元できる履歴を確認") { Task { await loadHistory() } }
                    .disabled(configFolder == nil || serving || refreshing || catchingUp || inspecting)
                if historyLoaded && historyEntries.isEmpty { Text("復元できる履歴はありません。") }
                ForEach(historyEntries) { entry in
                    VStack(alignment: .leading, spacing: 4) {
                        Text(entry.path?.joined(separator: "/") ?? "パス不明")
                        Text("\(entry.bytes.formatted()) バイト · \(Date(timeIntervalSince1970: TimeInterval(entry.archived_at)).formatted())")
                            .font(.caption).foregroundStyle(.secondary)
                        Button("この版を復元") {
                            pendingHistory = entry
                            confirmRestore = true
                        }
                        .disabled(entry.current_heads.count != 1 || serving || refreshing || catchingUp || inspecting)
                        Button("履歴を削除", role: .destructive) {
                            pendingDeletion = entry
                            confirmDelete = true
                        }
                        .disabled(serving || refreshing || catchingUp || inspecting)
                    }
                }
            }
            Section("競合") {
                Button("競合を確認") { Task { await loadConflicts() } }
                    .disabled(configFolder == nil || serving || refreshing || catchingUp || inspecting)
                if conflictsLoaded && conflictEntries.isEmpty { Text("未解決の競合はありません。") }
                ForEach(conflictEntries) { group in
                    VStack(alignment: .leading, spacing: 8) {
                        Text("競合候補 \(group.versions.count) 件").font(.headline)
                        ForEach(group.versions) { version in
                            HStack {
                                VStack(alignment: .leading) {
                                    Text(version.name ?? "削除候補")
                                    Text("\(version.kind) · \(String(version.id.prefix(12)))")
                                        .font(.caption).foregroundStyle(.secondary)
                                }
                                Spacer()
                                Button("採用") {
                                    pendingChoice = (group, version)
                                    confirmResolve = true
                                }
                                .disabled(group.versions.count < 2 ||
                                          group.versions.contains(where: { $0.kind == "directory" }) ||
                                          serving || refreshing || catchingUp || inspecting)
                            }
                        }
                    }
                }
            }
            if inspecting { ProgressView("履歴と競合を確認しています…") }
            if !result.isEmpty { Section("結果") { Text(result) } }
        }
        .navigationTitle("共有・配信")
        .task {
            restoreConfigFolder()
            prepareSourceFolder()
        }
        .onChange(of: phase) { _, newPhase in
            if newPhase != .active {
                stopServer()
                catchUpCancellation?.cancel()
                if catchingUp { catchUpStopping = true }
            }
        }
        .onDisappear {
            stopServer()
            catchUpCancellation?.cancel()
            if catchingUp { catchUpStopping = true }
        }
        .fileImporter(isPresented: $pickingFolder, allowedContentTypes: [.folder]) { selection in
            switch selection {
            case .success(let url): selectConfigFolder(url)
            case .failure: result = "配信設定フォルダーを開けませんでした。"
            }
        }
        .confirmationDialog("履歴を復元", isPresented: $confirmRestore, titleVisibility: .visible) {
            Button("新しい同期変更として復元") {
                if let pendingHistory { Task { await restoreHistory(pendingHistory) } }
            }
        } message: {
            Text("現在の版が変わっていれば復元は拒否されます。")
        }
        .confirmationDialog("履歴を削除", isPresented: $confirmDelete, titleVisibility: .visible) {
            Button("この履歴を削除", role: .destructive) {
                if let pendingDeletion { Task { await deleteHistory(pendingDeletion) } }
            }
        } message: {
            Text("この履歴は復元に使えなくなります。未同期ピアのために保護中の共通原本は保持されます。")
        }
        .confirmationDialog("競合の解決", isPresented: $confirmResolve, titleVisibility: .visible) {
            Button("選択した版を採用") {
                if let pendingChoice { Task { await resolveConflict(pendingChoice.0, choice: pendingChoice.1) } }
            }
        } message: {
            Text("表示した候補が変わっていれば解決は拒否されます。")
        }
    }

    private func prepareSourceFolder() {
        do {
            let documents = try FileManager.default.url(for: .documentDirectory, in: .userDomainMask,
                                                        appropriateFor: nil, create: true)
            let folder = documents.appendingPathComponent("P2PShared", isDirectory: true)
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            sourceFolder = folder
        } catch {
            result = "共有元フォルダーを作成できませんでした。"
        }
    }

    private func stateFolder() throws -> URL {
        let support = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask,
                                                  appropriateFor: nil, create: true)
        let folder = support.appendingPathComponent("P2PSync/WriterState", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        return folder
    }

    private func selectConfigFolder(_ url: URL) {
        guard !serving, !refreshing, !catchingUp, !inspecting else { return }
        do {
            guard url.startAccessingSecurityScopedResource() else { throw WriterFailure.permissionUnavailable }
            defer { url.stopAccessingSecurityScopedResource() }
            configBookmark = try url.bookmarkData(options: .minimalBookmark,
                                                   includingResourceValuesForKeys: nil, relativeTo: nil)
            configFolder = url
            result = ""
            invalidateLists()
        } catch {
            result = "配信設定フォルダーへのアクセスを保存できませんでした。"
        }
    }

    private func restoreConfigFolder() {
        guard !configBookmark.isEmpty else { return }
        do {
            var stale = false
            let url = try URL(resolvingBookmarkData: configBookmark, options: [.withoutUI],
                              relativeTo: nil, bookmarkDataIsStale: &stale)
            guard url.startAccessingSecurityScopedResource() else { throw WriterFailure.permissionUnavailable }
            defer { url.stopAccessingSecurityScopedResource() }
            if stale {
                configBookmark = try url.bookmarkData(options: .minimalBookmark,
                                                       includingResourceValuesForKeys: nil, relativeTo: nil)
            }
            configFolder = url
        } catch {
            configFolder = nil
            result = "保存した配信設定フォルダーを開けませんでした。もう一度選択してください。"
        }
    }

    private enum WriterFailure: Error { case permissionUnavailable }

    private func invalidateLists() {
        historyEntries = []
        conflictEntries = []
        historyLoaded = false
        conflictsLoaded = false
    }

    private func runWriter<T: Sendable>(
        _ operation: @escaping @Sendable (URL, URL) throws -> T
    ) async throws -> T {
        guard let configFolder else { throw WriterFailure.permissionUnavailable }
        let state = try stateFolder()
        return try await Task.detached(priority: .utility) {
            guard configFolder.startAccessingSecurityScopedResource() else {
                throw WriterFailure.permissionUnavailable
            }
            defer { configFolder.stopAccessingSecurityScopedResource() }
            return try operation(configFolder.appendingPathComponent("server.json"), state)
        }.value
    }

    private func refresh() async {
        guard !refreshing, !catchingUp, !inspecting, !serving,
              let configFolder, let sourceFolder else { return }
        refreshing = true
        result = ""
        defer { refreshing = false }
        do {
            let state = try stateFolder()
            let summary = try await Task.detached(priority: .utility) {
                guard configFolder.startAccessingSecurityScopedResource() else {
                    throw WriterFailure.permissionUnavailable
                }
                defer { configFolder.stopAccessingSecurityScopedResource() }
                return try SyncCore.refreshWriter(configuration: configFolder.appendingPathComponent("server.json"),
                                                  state: state, source: sourceFolder)
            }.value
            result = "ファイル \(summary.files) 件、フォルダー \(summary.directories) 件を確認しました。対象外 \(summary.skipped) 件。元フォルダーから消えたファイルは削除しません。"
            invalidateLists()
        } catch {
            result = writerError(error)
        }
    }

    private func catchUp() async {
        guard !catchingUp, !refreshing, !inspecting, !serving, let configFolder else { return }
        catchingUp = true
        catchUpStopping = false
        let stop = SyncCore.Cancellation()
        catchUpCancellation = stop
        result = ""
        defer {
            catchingUp = false
            catchUpStopping = false
            catchUpCancellation = nil
        }
        do {
            let state = try stateFolder()
            let added = try await Task.detached(priority: .utility) {
                guard configFolder.startAccessingSecurityScopedResource() else {
                    throw WriterFailure.permissionUnavailable
                }
                defer { configFolder.stopAccessingSecurityScopedResource() }
                return try SyncCore.catchUpWriter(
                    configuration: configFolder.appendingPathComponent("server.json"),
                    state: state,
                    peers: configFolder.appendingPathComponent("direct.json"), cancellation: stop
                )
            }.value
            result = "他のピアから変更 \(added) 件を取得しました。"
            invalidateLists()
        } catch {
            result = writerError(error)
        }
    }

    private func loadHistory() async {
        guard !serving, !refreshing, !catchingUp, !inspecting, configFolder != nil else { return }
        inspecting = true
        defer { inspecting = false }
        do {
            historyEntries = try await runWriter { config, state in
                try SyncCore.writerHistory(configuration: config, state: state)
            }
            historyLoaded = true
            result = "復元できる履歴 \(historyEntries.count) 件を読み込みました。"
        } catch {
            historyEntries = []
            historyLoaded = false
            result = writerError(error)
        }
    }

    private func loadConflicts() async {
        guard !serving, !refreshing, !catchingUp, !inspecting, configFolder != nil else { return }
        inspecting = true
        defer { inspecting = false }
        do {
            conflictEntries = try await runWriter { config, state in
                try SyncCore.writerConflicts(configuration: config, state: state)
            }
            conflictsLoaded = true
            result = "未解決の競合 \(conflictEntries.count) 件を読み込みました。"
        } catch {
            conflictEntries = []
            conflictsLoaded = false
            result = writerError(error)
        }
    }

    private func restoreHistory(_ entry: SyncCore.WriterHistoryEntry) async {
        guard !serving, !refreshing, !catchingUp, !inspecting,
              entry.current_heads.count == 1 else { return }
        inspecting = true
        defer { inspecting = false }
        do {
            try await runWriter { config, state in
                try SyncCore.restoreWriterHistory(configuration: config, state: state,
                                                  archive: entry.archive, reviewed: entry.current_heads[0])
            }
            invalidateLists()
            result = "履歴を新しい同期変更として復元しました。"
        } catch {
            result = writerError(error)
        }
    }

    private func deleteHistory(_ entry: SyncCore.WriterHistoryEntry) async {
        guard !serving, !refreshing, !catchingUp, !inspecting else { return }
        inspecting = true
        defer { inspecting = false }
        do {
            try await runWriter { config, state in
                try SyncCore.deleteWriterHistory(configuration: config, state: state, entry: entry)
            }
            invalidateLists()
            result = "履歴を削除しました。保護中の共通原本は保持されます。"
        } catch {
            result = writerError(error)
        }
    }

    private func resolveConflict(_ group: SyncCore.WriterConflict,
                                 choice: SyncCore.WriterConflictVersion) async {
        guard !serving, !refreshing, !catchingUp, !inspecting,
              group.versions.count >= 2,
              !group.versions.contains(where: { $0.kind == "directory" }),
              group.versions.contains(where: { $0.id == choice.id }) else { return }
        inspecting = true
        defer { inspecting = false }
        do {
            try await runWriter { config, state in
                try SyncCore.resolveWriterConflict(configuration: config, state: state,
                                                   choice: choice.id,
                                                   reviewed: group.versions.map(\.id))
            }
            invalidateLists()
            result = "選択した版を採用する同期変更を記録しました。"
        } catch {
            result = writerError(error)
        }
    }

    private func startServer() {
        guard !serving, !refreshing, !catchingUp, !inspecting, phase == .active,
              let configFolder else { return }
        let state: URL
        do { state = try stateFolder() }
        catch { serverStatus = "配信状態を開けませんでした。"; return }
        let stop = SyncCore.Cancellation()
        cancellation = stop
        serving = true
        serverStatus = "配信を開始しています…"
        let (ready, continuation) = AsyncStream.makeStream(of: String.self)
        let worker = Task.detached(priority: .utility) {
            defer { continuation.finish() }
            guard configFolder.startAccessingSecurityScopedResource() else {
                throw WriterFailure.permissionUnavailable
            }
            defer { configFolder.stopAccessingSecurityScopedResource() }
            try SyncCore.serveWriter(configuration: configFolder.appendingPathComponent("server.json"),
                                     state: state, cancellation: stop,
                                     onReady: { address in continuation.yield(address) })
        }
        Task {
            for await address in ready where serving && !stop.isCancelled() {
                serverStatus = "配信中: \(address)"
            }
        }
        Task {
            do {
                try await worker.value
                serverStatus = "配信を停止しました。"
            } catch {
                serverStatus = writerError(error)
            }
            serving = false
            cancellation = nil
        }
    }

    private func stopServer() {
        guard serving, cancellation?.isCancelled() == false else { return }
        cancellation?.cancel()
        serverStatus = "配信を停止しています…"
    }

    private func writerError(_ error: Error) -> String {
        if let error = error as? SyncCore.CoreError {
            switch error {
            case .writerFailed(let code) where code == 2: return "配信設定または共有元を確認してください。"
            case .writerFailed(let code) where code == 3: return "配信状態またはファイルへアクセスできませんでした。"
            case .writerFailed(let code) where code == 4: return "証明書または秘密鍵を読み込めませんでした。"
            case .writerFailed(let code) where code == 5: return "表示後に状態が変わったか、本体が不足しています。再読み込みしてください。"
            case .writerFailed(let code) where code == 7: return "取得を中断しました。次回は保存済みのデータを再利用します。"
            default: return "配信処理に失敗しました。"
            }
        }
        if error is WriterFailure { return "配信設定フォルダーへのアクセスを失いました。選び直してください。" }
        return "配信処理を開始できませんでした。"
    }
}
