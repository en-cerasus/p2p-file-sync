import CP2PSync
import Foundation

enum SyncCore {
    final class Cancellation: @unchecked Sendable {
        private let lock = NSLock()
        private var cancelled = false

        func cancel() {
            lock.lock()
            defer { lock.unlock() }
            cancelled = true
        }

        func isCancelled() -> Bool {
            lock.lock()
            defer { lock.unlock() }
            return cancelled
        }
    }

    enum CoreError: Error {
        case incompatibleABI(UInt32)
        case hashingFailed(Int32)
        case pullFailed(Int32)
        case missingOriginalsReport
        case writerFailed(Int32)
        case missingWriterReport
    }

    static var abiVersion: UInt32 { p2p_sync_abi_version() }

    static func sha256(_ data: Data) throws -> [UInt8] {
        let version = abiVersion
        guard version == 1 else { throw CoreError.incompatibleABI(version) }

        var digest = [UInt8](repeating: 0, count: Int(P2P_SYNC_SHA256_SIZE))
        let result = data.withUnsafeBytes { input in
            digest.withUnsafeMutableBufferPointer { output in
                p2p_sync_sha256(
                    input.bindMemory(to: UInt8.self).baseAddress, input.count,
                    output.baseAddress, output.count
                )
            }
        }
        guard result == P2P_SYNC_OK else { throw CoreError.hashingFailed(result) }
        return digest
    }

    struct ProtectedOriginal: Decodable, Identifiable, Sendable {
        struct WaitingPeer: Decodable, Identifiable, Sendable {
            enum State: String, Decodable, Sendable {
                case confirmation
                case events
            }

            let peer: String
            let state: State
            var id: String { peer }
        }

        let hash: [UInt8]
        let names: [String]
        let bytes: UInt64?
        let conflict: Bool
        let waiting: [WaitingPeer]

        var id: [UInt8] { hash }
    }

    struct PullSummary: Sendable {
        let eventsAdded: UInt64
        let filesPublished: UInt64
        let filesPresent: UInt64
        let filesBlocked: UInt64
        let filesUpdated: UInt64
        let filesRemoved: UInt64
        let protectedFiles: UInt64
        let protectedBytes: UInt64
        let protectedMissing: UInt64
        let originals: [ProtectedOriginal]
        let conflicts: [FileConflict]
        let history: [HistoryEntry]
        let historyBytes: UInt64
        let historyMissing: UInt64
    }

    struct FileConflict: Decodable, Identifiable, Sendable {
        struct Version: Decodable, Identifiable, Sendable {
            let peer: String
            let sequence: UInt64
            let name: String?
            let kind: String
            let bytes: UInt64?
            var id: String { "\(peer):\(sequence)" }
        }
        let object: String
        let versions: [Version]
        var id: String { object }
    }

    struct HistoryEntry: Decodable, Sendable {
        struct Content: Decodable, Sendable {
            let length: UInt64
        }
        let path: [String]?
        let content: Content
        let archived_at: Int64
    }

    private struct SyncDetails: Decodable {
        let originals: [ProtectedOriginal]
        let conflicts: [FileConflict]
        let history: [HistoryEntry]
        let history_bytes: UInt64
        let history_missing: UInt64
    }

    private struct PullContext {
        let cancellation: Cancellation
        var json: Data?
    }

    struct WriterSummary: Decodable, Sendable {
        let files: UInt64
        let directories: UInt64
        let skipped: UInt64
    }

    struct WriterProtectedReport: Decodable, Sendable {
        let files: UInt64
        let bytes: UInt64
        let missing: UInt64
        let originals: [ProtectedOriginal]
    }

    struct WriterHistoryEntry: Decodable, Identifiable, Sendable {
        let id: Int64
        let archive: String
        let replaced_by: String
        let current_heads: [String]
        let path: [String]?
        let bytes: UInt64
        let archived_at: Int64
    }

    private struct WriterArchiveSelection: Encodable {
        let id: Int64
        let archive: String
        let replaced_by: String
        let archived_at: Int64

        init(_ entry: WriterHistoryEntry) {
            id = entry.id
            archive = entry.archive
            replaced_by = entry.replaced_by
            archived_at = entry.archived_at
        }
    }

    struct WriterConflictVersion: Decodable, Identifiable, Sendable {
        let id: String
        let name: String?
        let kind: String
    }

    struct WriterConflict: Decodable, Identifiable, Sendable {
        let versions: [WriterConflictVersion]
        var id: String { versions.map(\.id).joined(separator: "|") }
    }

    private struct WriterReportContext {
        var json: Data?
    }

    static func writerProtectedOriginals(configuration: URL, state: URL) throws -> WriterProtectedReport {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        var report = WriterReportContext(json: nil)
        let status = withUnsafeMutablePointer(to: &report) { context in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { stateBytes in
                    p2p_sync_writer_protected_originals(
                        configBytes.baseAddress, configBytes.count,
                        stateBytes.baseAddress, stateBytes.count,
                        { context, data, length in
                            guard let context, let data else { return }
                            context.assumingMemoryBound(to: WriterReportContext.self).pointee.json =
                                Data(bytes: data, count: length)
                        }, UnsafeMutableRawPointer(context))
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
        guard let json = report.json else { throw CoreError.missingWriterReport }
        return try JSONDecoder().decode(WriterProtectedReport.self, from: json)
    }

    static func writerHistory(configuration: URL, state: URL) throws -> [WriterHistoryEntry] {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        var report = WriterReportContext(json: nil)
        let status = withUnsafeMutablePointer(to: &report) { context in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { stateBytes in
                    p2p_sync_writer_history(configBytes.baseAddress, configBytes.count,
                                            stateBytes.baseAddress, stateBytes.count,
                                            { context, data, length in
                                                guard let context, let data else { return }
                                                context.assumingMemoryBound(to: WriterReportContext.self).pointee.json =
                                                    Data(bytes: data, count: length)
                                            }, UnsafeMutableRawPointer(context))
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
        guard let json = report.json else { throw CoreError.missingWriterReport }
        return try JSONDecoder().decode([WriterHistoryEntry].self, from: json)
    }

    static func writerConflicts(configuration: URL, state: URL) throws -> [WriterConflict] {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        var report = WriterReportContext(json: nil)
        let status = withUnsafeMutablePointer(to: &report) { context in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { stateBytes in
                    p2p_sync_writer_conflicts(configBytes.baseAddress, configBytes.count,
                                              stateBytes.baseAddress, stateBytes.count,
                                              { context, data, length in
                                                  guard let context, let data else { return }
                                                  context.assumingMemoryBound(to: WriterReportContext.self).pointee.json =
                                                      Data(bytes: data, count: length)
                                              }, UnsafeMutableRawPointer(context))
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
        guard let json = report.json else { throw CoreError.missingWriterReport }
        return try JSONDecoder().decode([WriterConflict].self, from: json)
    }

    static func restoreWriterHistory(configuration: URL, state: URL,
                                     archive: String, reviewed: String) throws {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let archived = Array(archive.utf8)
        let current = Array(reviewed.utf8)
        let status = config.withUnsafeBufferPointer { configBytes in
            storage.withUnsafeBufferPointer { stateBytes in
                archived.withUnsafeBufferPointer { archiveBytes in
                    current.withUnsafeBufferPointer { currentBytes in
                        p2p_sync_writer_restore(
                            configBytes.baseAddress, configBytes.count,
                            stateBytes.baseAddress, stateBytes.count,
                            archiveBytes.baseAddress, archiveBytes.count,
                            currentBytes.baseAddress, currentBytes.count)
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
    }

    static func deleteWriterHistory(configuration: URL, state: URL,
                                    entry: WriterHistoryEntry) throws {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let selected = try JSONEncoder().encode([WriterArchiveSelection(entry)])
        let status = config.withUnsafeBufferPointer { configBytes in
            storage.withUnsafeBufferPointer { stateBytes in
                selected.withUnsafeBytes { selectedBytes in
                    let bytes = selectedBytes.bindMemory(to: UInt8.self)
                    return p2p_sync_writer_delete_history(
                        configBytes.baseAddress, configBytes.count,
                        stateBytes.baseAddress, stateBytes.count,
                        bytes.baseAddress, bytes.count)
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
    }

    static func resolveWriterConflict(configuration: URL, state: URL,
                                      choice: String, reviewed: [String]) throws {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let chosen = Array(choice.utf8)
        let candidates = try JSONEncoder().encode(reviewed)
        let status = config.withUnsafeBufferPointer { configBytes in
            storage.withUnsafeBufferPointer { stateBytes in
                chosen.withUnsafeBufferPointer { choiceBytes in
                    candidates.withUnsafeBytes { candidateBytes in
                        let bytes = candidateBytes.bindMemory(to: UInt8.self)
                        return p2p_sync_writer_resolve(
                            configBytes.baseAddress, configBytes.count,
                            stateBytes.baseAddress, stateBytes.count,
                            choiceBytes.baseAddress, choiceBytes.count,
                            bytes.baseAddress, bytes.count)
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
    }

    static func refreshWriter(configuration: URL, state: URL, source: URL) throws -> WriterSummary {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let input = Array(source.path.utf8)
        var report = WriterReportContext(json: nil)
        let status = withUnsafeMutablePointer(to: &report) { context in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { stateBytes in
                    input.withUnsafeBufferPointer { sourceBytes in
                        p2p_sync_writer_refresh_tree(
                            configBytes.baseAddress, configBytes.count,
                            stateBytes.baseAddress, stateBytes.count,
                            sourceBytes.baseAddress, sourceBytes.count,
                            { context, data, length in
                                guard let context, let data else { return }
                                context.assumingMemoryBound(to: WriterReportContext.self).pointee.json =
                                    Data(bytes: data, count: length)
                            },
                            UnsafeMutableRawPointer(context)
                        )
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
        guard let json = report.json else { throw CoreError.missingWriterReport }
        return try JSONDecoder().decode(WriterSummary.self, from: json)
    }

    private struct WriterCatchUpContext {
        let cancellation: Cancellation
    }

    static func catchUpWriter(configuration: URL, state: URL, peers: URL,
                              cancellation: Cancellation) throws -> UInt64 {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let peerConfig = Array(peers.path.utf8)
        var added: UInt64 = 0
        var callbacks = WriterCatchUpContext(cancellation: cancellation)
        let status = withUnsafeMutablePointer(to: &callbacks) { context in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { stateBytes in
                    peerConfig.withUnsafeBufferPointer { peerBytes in
                        p2p_sync_writer_catch_up_v2(
                            configBytes.baseAddress, configBytes.count,
                            stateBytes.baseAddress, stateBytes.count,
                            peerBytes.baseAddress, peerBytes.count, &added,
                            { context in
                                guard let context else { return 0 }
                                return context.assumingMemoryBound(to: WriterCatchUpContext.self)
                                    .pointee.cancellation.isCancelled() ? 1 : 0
                            }, UnsafeMutableRawPointer(context)
                        )
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
        return added
    }

    private struct WriterServeContext {
        let cancellation: Cancellation
        let onReady: @Sendable (String) -> Void
    }

    static func serveWriter(configuration: URL, state: URL, cancellation: Cancellation,
                            onReady: @escaping @Sendable (String) -> Void) throws {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        var callbacks = WriterServeContext(cancellation: cancellation, onReady: onReady)
        let status = withUnsafeMutablePointer(to: &callbacks) { context in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { stateBytes in
                    p2p_sync_writer_serve(
                        configBytes.baseAddress, configBytes.count,
                        stateBytes.baseAddress, stateBytes.count,
                        { context, data, length in
                            guard let context, let data else { return }
                            let callbacks = context.assumingMemoryBound(to: WriterServeContext.self).pointee
                            if let address = String(data: Data(bytes: data, count: length), encoding: .utf8) {
                                callbacks.onReady(address)
                            }
                        },
                        { context in
                            guard let context else { return 0 }
                            return context.assumingMemoryBound(to: WriterServeContext.self)
                                .pointee.cancellation.isCancelled() ? 1 : 0
                        },
                        UnsafeMutableRawPointer(context)
                    )
                }
            }
        }
        guard status == 7 else { throw CoreError.writerFailed(status) }
    }

    static func pull(configuration: URL, state: URL, destination: URL,
                     cancellation: Cancellation = Cancellation()) throws -> PullSummary {
        var summary = p2p_sync_pull_summary_v3()
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let output = Array(destination.path.utf8)
        var callbacks = PullContext(cancellation: cancellation, json: nil)
        let status = withUnsafeMutablePointer(to: &callbacks) { context in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { storageBytes in
                    output.withUnsafeBufferPointer { outputBytes in
                        p2p_sync_pull_v6(
                            configBytes.baseAddress, configBytes.count,
                            storageBytes.baseAddress, storageBytes.count,
                            outputBytes.baseAddress, outputBytes.count,
                            &summary,
                            { context, data, length in
                                guard let context, let data else { return }
                                context.assumingMemoryBound(to: PullContext.self).pointee.json =
                                    Data(bytes: data, count: length)
                            },
                            UnsafeMutableRawPointer(context),
                            { context in
                                guard let context else { return 0 }
                                return context.assumingMemoryBound(to: PullContext.self)
                                    .pointee.cancellation.isCancelled() ? 1 : 0
                            }
                        )
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.pullFailed(status) }
        guard let json = callbacks.json else { throw CoreError.missingOriginalsReport }
        let details = try JSONDecoder().decode(SyncDetails.self, from: json)
        return PullSummary(
            eventsAdded: summary.sync.events_added, filesPublished: summary.sync.files_published,
            filesPresent: summary.sync.files_present, filesBlocked: summary.sync.files_blocked,
            filesUpdated: summary.sync.files_updated, filesRemoved: summary.sync.files_removed,
            protectedFiles: summary.protected_files, protectedBytes: summary.protected_bytes,
            protectedMissing: summary.protected_missing,
            originals: details.originals,
            conflicts: details.conflicts,
            history: details.history,
            historyBytes: details.history_bytes,
            historyMissing: details.history_missing
        )
    }
}
