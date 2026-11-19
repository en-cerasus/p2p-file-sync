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
    @State private var serving = false
    @State private var cancellation: SyncCore.Cancellation?
    @State private var sourceFolder: URL?
    @State private var result = ""
    @State private var serverStatus = "配信停止中"

    var body: some View {
        Form {
            Section("書き込みピア") {
                Button("配信設定フォルダーを選択") { pickingFolder = true }
                Text(configFolder?.lastPathComponent ?? "設定フォルダーが未選択です")
                    .foregroundStyle(.secondary)
            }
            Section("共有元") {
                Text("このiPhone内の P2PSync / P2PShared")
                Text("Filesアプリでこのフォルダーに譜面・音源を入れます。")
                    .font(.caption).foregroundStyle(.secondary)
                Button("新規・変更ファイルを登録") { Task { await refresh() } }
                    .disabled(configFolder == nil || sourceFolder == nil || refreshing)
                if refreshing { ProgressView("共有元を確認しています…") }
                if !result.isEmpty { Text(result) }
            }
            Section("配信") {
                Button("配信を開始") { startServer() }
                    .disabled(configFolder == nil || serving || phase != .active)
                Button("配信を停止") { stopServer() }
                    .disabled(!serving || cancellation?.isCancelled() == true)
                Text(serverStatus).foregroundStyle(.secondary)
                Text("配信は画面を開いている間に動作します。アプリがバックグラウンドへ移ると停止します。")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        .navigationTitle("共有・配信")
        .task {
            restoreConfigFolder()
            prepareSourceFolder()
        }
        .onChange(of: phase) { _, newPhase in
            if newPhase != .active { stopServer() }
        }
        .onDisappear { stopServer() }
        .fileImporter(isPresented: $pickingFolder, allowedContentTypes: [.folder]) { selection in
            switch selection {
            case .success(let url): selectConfigFolder(url)
            case .failure: result = "配信設定フォルダーを開けませんでした。"
            }
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
        do {
            guard url.startAccessingSecurityScopedResource() else { throw WriterFailure.permissionUnavailable }
            defer { url.stopAccessingSecurityScopedResource() }
            configBookmark = try url.bookmarkData(options: .minimalBookmark,
                                                   includingResourceValuesForKeys: nil, relativeTo: nil)
            configFolder = url
            result = ""
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

    private func refresh() async {
        guard !refreshing, let configFolder, let sourceFolder else { return }
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
        } catch {
            result = writerError(error)
        }
    }

    private func startServer() {
        guard !serving, phase == .active, let configFolder else { return }
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
            default: return "配信処理に失敗しました。"
            }
        }
        if error is WriterFailure { return "配信設定フォルダーへのアクセスを失いました。選び直してください。" }
        return "配信処理を開始できませんでした。"
    }
}
