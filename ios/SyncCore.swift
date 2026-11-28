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
        case pullFailedAfterRecovery(Int32, UInt64, UInt64)
        case missingOriginalsReport
        case writerFailed(Int32)
        case missingWriterReport
        case pendingFailed(Int32)
        case snapshotFailed(Int32)
        case coordinatedPathChanged
        case missingPendingDestination
        case rtcFailed(Int32)
    }

    private struct RTCContext {
        let cancellation: Cancellation
        let onStatus: @Sendable (String) -> Void
    }

    static func runRTCBridge(configuration: URL, cancellation: Cancellation,
                             onStatus: @escaping @Sendable (String) -> Void) throws {
        let config = Array(configuration.path.utf8)
        var callbacks = RTCContext(cancellation: cancellation, onStatus: onStatus)
        let status = withUnsafeMutablePointer(to: &callbacks) { context in
            config.withUnsafeBufferPointer { bytes in
                p2p_sync_rtc_bridge_run_v1(bytes.baseAddress, bytes.count,
                    { context, data, length in
                        guard let context, let data else { return }
                        let callbacks = context.assumingMemoryBound(to: RTCContext.self).pointee
                        if let value = String(data: Data(bytes: data, count: length), encoding: .utf8) {
                            callbacks.onStatus(value)
                        }
                    },
                    { context in
                        guard let context else { return 0 }
                        return context.assumingMemoryBound(to: RTCContext.self)
                            .pointee.cancellation.isCancelled() ? 1 : 0
                    }, UnsafeMutableRawPointer(context))
            }
        }
        guard status == P2P_SYNC_OK || status == 7 else { throw CoreError.rtcFailed(status) }
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

    private struct PendingFile: Codable {
        enum Kind: String, Codable, Equatable {
            case update, delete, move
        }
        let object: String
        let kind: Kind
        let source: [String]
        let destination: [String]?
    }

    private struct PendingCounts {
        var updated: UInt64 = 0
        var removed: UInt64 = 0
        var blocked: UInt64 = 0
    }

    private static func pendingFiles(configuration: URL, state: URL, destination: URL) throws -> [PendingFile] {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let output = Array(destination.path.utf8)
        var context = PullContext(cancellation: Cancellation(), json: nil)
        let status = withUnsafeMutablePointer(to: &context) { pointer in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { stateBytes in
                    output.withUnsafeBufferPointer { outputBytes in
                        p2p_sync_ios_pending_v1(
                            configBytes.baseAddress, configBytes.count,
                            stateBytes.baseAddress, stateBytes.count,
                            outputBytes.baseAddress, outputBytes.count,
                            { context, data, length in
                                guard let context, let data else { return }
                                context.assumingMemoryBound(to: PullContext.self).pointee.json =
                                    Data(bytes: data, count: length)
                            }, UnsafeMutableRawPointer(pointer)
                        )
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.pendingFailed(status) }
        guard let json = context.json else { throw CoreError.missingOriginalsReport }
        return try JSONDecoder().decode([PendingFile].self, from: json)
    }

    private static func pendingURL(root: URL, components: [String]) throws -> URL {
        guard !components.isEmpty else { throw CoreError.coordinatedPathChanged }
        var url = root
        for component in components {
            guard !component.isEmpty, component != ".", component != "..",
                  !component.contains("/"), !component.contains("\\"), !component.contains("\0") else {
                throw CoreError.coordinatedPathChanged
            }
            url.appendPathComponent(component)
        }
        return url
    }

    private static func applyPending(_ item: PendingFile, configuration: URL, state: URL,
                                     destination: URL) throws -> Int32 {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let output = Array(destination.path.utf8)
        let reviewed = Array(try JSONEncoder().encode(item))
        var outcome: Int32 = 0
        let status = config.withUnsafeBufferPointer { configBytes in
            storage.withUnsafeBufferPointer { stateBytes in
                output.withUnsafeBufferPointer { outputBytes in
                    reviewed.withUnsafeBufferPointer { reviewBytes in
                        p2p_sync_ios_apply_v1(
                            configBytes.baseAddress, configBytes.count,
                            stateBytes.baseAddress, stateBytes.count,
                            outputBytes.baseAddress, outputBytes.count,
                            reviewBytes.baseAddress, reviewBytes.count, &outcome
                        )
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.pendingFailed(status) }
        return outcome
    }

    private static func recoverPending(configuration: URL, state: URL, destination: URL,
                                       cancellation: Cancellation) throws -> PendingCounts {
        var counts = PendingCounts()
        for item in try pendingFiles(configuration: configuration, state: state, destination: destination) {
            if cancellation.isCancelled() { break }
            let source = try pendingURL(root: destination, components: item.source)
            let coordinator = NSFileCoordinator(filePresenter: nil)
            var coordinationError: NSError?
            var operationError: Error?
            var outcome: Int32?
            if item.kind == .move {
                guard let components = item.destination else { throw CoreError.missingPendingDestination }
                let target = try pendingURL(root: destination, components: components)
                coordinator.coordinate(writingItemAt: source, options: .forMoving,
                                       writingItemAt: target, options: [], error: &coordinationError) {
                    actualSource, actualTarget in
                    guard actualSource.standardizedFileURL == source.standardizedFileURL,
                          actualTarget.standardizedFileURL == target.standardizedFileURL else {
                        operationError = CoreError.coordinatedPathChanged
                        return
                    }
                    do {
                        outcome = try applyPending(item, configuration: configuration,
                                                   state: state, destination: destination)
                    } catch { operationError = error }
                }
            } else {
                let options: NSFileCoordinator.WritingOptions = item.kind == .delete ? .forDeleting : .forReplacing
                coordinator.coordinate(writingItemAt: source, options: options,
                                       error: &coordinationError) { actualSource in
                    guard actualSource.standardizedFileURL == source.standardizedFileURL else {
                        operationError = CoreError.coordinatedPathChanged
                        return
                    }
                    do {
                        outcome = try applyPending(item, configuration: configuration,
                                                   state: state, destination: destination)
                    } catch { operationError = error }
                }
            }
            if let error = operationError as? CoreError,
               case .pendingFailed(8) = error { throw error }
            if coordinationError != nil || operationError != nil {
                counts.blocked += 1
            } else if outcome == 1 || outcome == 2 {
                if item.kind == .delete { counts.removed += 1 } else { counts.updated += 1 }
            } else if outcome != 0 {
                counts.blocked += 1
            }
        }
        return counts
    }

    private static func receiverSnapshot(configuration: URL, state: URL, destination: URL)
        throws -> (p2p_sync_pull_summary_v3, SyncDetails) {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let output = Array(destination.path.utf8)
        var summary = p2p_sync_pull_summary_v3()
        var context = PullContext(cancellation: Cancellation(), json: nil)
        let status = withUnsafeMutablePointer(to: &context) { pointer in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { stateBytes in
                    output.withUnsafeBufferPointer { outputBytes in
                        p2p_sync_ios_snapshot_v1(
                            configBytes.baseAddress, configBytes.count,
                            stateBytes.baseAddress, stateBytes.count,
                            outputBytes.baseAddress, outputBytes.count, &summary,
                            { context, data, length in
                                guard let context, let data else { return }
                                context.assumingMemoryBound(to: PullContext.self).pointee.json =
                                    Data(bytes: data, count: length)
                            }, UnsafeMutableRawPointer(pointer)
                        )
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.snapshotFailed(status) }
        guard let json = context.json else { throw CoreError.missingOriginalsReport }
        return (summary, try JSONDecoder().decode(SyncDetails.self, from: json))
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

    struct MissingSourceFile: Decodable, Identifiable, Sendable {
        let path: [String]
        let head: String
        var id: String { head }
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

    struct IsolatedArchiveReport: Decodable, Sendable {
        let share: String
        let previous_generation: UInt64
        let next_generation: UInt64
        let events: [IsolatedEvent]
        let copied_blobs: UInt64
        let missing_blobs: [String]
    }

    struct SourceArchiveReport: Decodable, Sendable {
        struct ArchivedFile: Decodable, Sendable {
            let path: [String]
            let bytes: UInt64
            let sha256: String
        }

        let candidates: [[String]]
        let files: [ArchivedFile]
        let directories: [[String]]
        let missing: [[String]]
    }

    struct IsolatedEvent: Decodable, Identifiable, Sendable {
        let peer: String
        let sequence: UInt64
        let object: String
        let path_hint: String?
        let change: String
        let operation: String
        let content_sha256: String?
        var id: String { "\(peer):\(sequence)" }
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

    static func verifyIsolatedArchive(configuration: URL, archive: URL) throws -> IsolatedArchiveReport {
        let config = Array(configuration.path.utf8)
        let folder = Array(archive.path.utf8)
        var report = WriterReportContext(json: nil)
        let status = withUnsafeMutablePointer(to: &report) { context in
            config.withUnsafeBufferPointer { configBytes in
                folder.withUnsafeBufferPointer { folderBytes in
                    p2p_sync_verify_isolated_archive_v1(
                        configBytes.baseAddress, configBytes.count,
                        folderBytes.baseAddress, folderBytes.count,
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
        return try JSONDecoder().decode(IsolatedArchiveReport.self, from: json)
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

    static func writerRecoveryCandidates(configuration: URL, state: URL, source: URL) throws -> [[String]] {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let input = Array(source.path.utf8)
        var report = WriterReportContext(json: nil)
        let status = withUnsafeMutablePointer(to: &report) { context in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { stateBytes in
                    input.withUnsafeBufferPointer { sourceBytes in
                        p2p_sync_writer_recovery_candidates_v1(
                            configBytes.baseAddress, configBytes.count,
                            stateBytes.baseAddress, stateBytes.count,
                            sourceBytes.baseAddress, sourceBytes.count,
                            { context, data, length in
                                guard let context, let data else { return }
                                context.assumingMemoryBound(to: WriterReportContext.self).pointee.json =
                                    Data(bytes: data, count: length)
                            }, UnsafeMutableRawPointer(context))
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
        guard let json = report.json else { throw CoreError.missingWriterReport }
        return try JSONDecoder().decode([[String]].self, from: json)
    }

    static func archiveWriterRecoveredSource(configuration: URL, state: URL,
                                             source: URL, output: URL) throws -> SourceArchiveReport {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let input = Array(source.path.utf8)
        let target = Array(output.path.utf8)
        var report = WriterReportContext(json: nil)
        let status = withUnsafeMutablePointer(to: &report) { context in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { stateBytes in
                    input.withUnsafeBufferPointer { sourceBytes in
                        target.withUnsafeBufferPointer { outputBytes in
                            p2p_sync_writer_archive_recovered_source_v1(
                                configBytes.baseAddress, configBytes.count,
                                stateBytes.baseAddress, stateBytes.count,
                                sourceBytes.baseAddress, sourceBytes.count,
                                outputBytes.baseAddress, outputBytes.count,
                                { context, data, length in
                                    guard let context, let data else { return }
                                    context.assumingMemoryBound(to: WriterReportContext.self).pointee.json =
                                        Data(bytes: data, count: length)
                                }, UnsafeMutableRawPointer(context))
                        }
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
        guard let json = report.json else { throw CoreError.missingWriterReport }
        return try JSONDecoder().decode(SourceArchiveReport.self, from: json)
    }

    static func verifyRecoveredSourceArchive(_ archive: URL) throws -> SourceArchiveReport {
        let folder = Array(archive.path.utf8)
        var report = WriterReportContext(json: nil)
        let status = withUnsafeMutablePointer(to: &report) { context in
            folder.withUnsafeBufferPointer { bytes in
                p2p_sync_verify_recovered_source_archive_v1(
                    bytes.baseAddress, bytes.count,
                    { context, data, length in
                        guard let context, let data else { return }
                        context.assumingMemoryBound(to: WriterReportContext.self).pointee.json =
                            Data(bytes: data, count: length)
                    }, UnsafeMutableRawPointer(context))
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
        guard let json = report.json else { throw CoreError.missingWriterReport }
        return try JSONDecoder().decode(SourceArchiveReport.self, from: json)
    }

    static func resumeWriterAfterRecovery(configuration: URL, state: URL, source: URL) throws {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let input = Array(source.path.utf8)
        let status = config.withUnsafeBufferPointer { configBytes in
            storage.withUnsafeBufferPointer { stateBytes in
                input.withUnsafeBufferPointer { sourceBytes in
                    p2p_sync_writer_resume_after_recovery_v1(
                        configBytes.baseAddress, configBytes.count,
                        stateBytes.baseAddress, stateBytes.count,
                        sourceBytes.baseAddress, sourceBytes.count)
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

    static func writerMissingFiles(configuration: URL, state: URL, source: URL) throws -> [MissingSourceFile] {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let input = Array(source.path.utf8)
        var report = WriterReportContext(json: nil)
        let status = withUnsafeMutablePointer(to: &report) { context in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { stateBytes in
                    input.withUnsafeBufferPointer { sourceBytes in
                        p2p_sync_writer_missing_files(
                            configBytes.baseAddress, configBytes.count,
                            stateBytes.baseAddress, stateBytes.count,
                            sourceBytes.baseAddress, sourceBytes.count,
                            { context, data, length in
                                guard let context, let data else { return }
                                context.assumingMemoryBound(to: WriterReportContext.self).pointee.json =
                                    Data(bytes: data, count: length)
                            }, UnsafeMutableRawPointer(context))
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
        guard let json = report.json else { throw CoreError.missingWriterReport }
        return try JSONDecoder().decode([MissingSourceFile].self, from: json)
    }

    static func deleteMissingSourceFile(configuration: URL, state: URL, source: URL,
                                        file: MissingSourceFile) throws {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let input = Array(source.path.utf8)
        let path = Array(file.path.joined(separator: "/").utf8)
        let head = Array(file.head.utf8)
        let status = config.withUnsafeBufferPointer { configBytes in
            storage.withUnsafeBufferPointer { stateBytes in
                input.withUnsafeBufferPointer { sourceBytes in
                    path.withUnsafeBufferPointer { pathBytes in
                        head.withUnsafeBufferPointer { headBytes in
                            p2p_sync_writer_delete_missing_file(
                                configBytes.baseAddress, configBytes.count,
                                stateBytes.baseAddress, stateBytes.count,
                                sourceBytes.baseAddress, sourceBytes.count,
                                pathBytes.baseAddress, pathBytes.count,
                                headBytes.baseAddress, headBytes.count)
                        }
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
    }

    static func relocateMissingSourceFile(configuration: URL, state: URL, source: URL,
                                          file: MissingSourceFile, destination: URL) throws {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let input = Array(source.path.utf8)
        let path = Array(file.path.joined(separator: "/").utf8)
        let head = Array(file.head.utf8)
        let target = Array(destination.path.utf8)
        let status = config.withUnsafeBufferPointer { configBytes in
            storage.withUnsafeBufferPointer { stateBytes in
                input.withUnsafeBufferPointer { sourceBytes in
                    path.withUnsafeBufferPointer { pathBytes in
                        head.withUnsafeBufferPointer { headBytes in
                            target.withUnsafeBufferPointer { targetBytes in
                                p2p_sync_writer_relocate_missing_file(
                                    configBytes.baseAddress, configBytes.count,
                                    stateBytes.baseAddress, stateBytes.count,
                                    sourceBytes.baseAddress, sourceBytes.count,
                                    pathBytes.baseAddress, pathBytes.count,
                                    headBytes.baseAddress, headBytes.count,
                                    targetBytes.baseAddress, targetBytes.count)
                            }
                        }
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.writerFailed(status) }
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

    private static func preflightReceiverAnchor(configuration: URL, state: URL, destination: URL,
                                                 cancellation: Cancellation) throws {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let output = Array(destination.path.utf8)
        var callbacks = PullContext(cancellation: cancellation, json: nil)
        let status = withUnsafeMutablePointer(to: &callbacks) { context in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { storageBytes in
                    output.withUnsafeBufferPointer { outputBytes in
                        p2p_sync_receiver_anchor_preflight_v1(
                            configBytes.baseAddress, configBytes.count,
                            storageBytes.baseAddress, storageBytes.count,
                            outputBytes.baseAddress, outputBytes.count,
                            { context in
                                guard let context else { return 0 }
                                return context.assumingMemoryBound(to: PullContext.self)
                                    .pointee.cancellation.isCancelled() ? 1 : 0
                            }, UnsafeMutableRawPointer(context)
                        )
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.pullFailed(status) }
    }

    static func pull(configuration: URL, state: URL, destination: URL,
                     cancellation: Cancellation = Cancellation()) throws -> PullSummary {
        // Listing pending work is read-only. Check the remote anchor only when
        // a saved file operation may run before the normal pull checks it.
        do {
            let pending = try pendingFiles(configuration: configuration, state: state,
                                           destination: destination)
            if !pending.isEmpty {
                try preflightReceiverAnchor(configuration: configuration, state: state,
                                            destination: destination, cancellation: cancellation)
            }
        } catch CoreError.pendingFailed(2) {
            // A staged generation may have no activation marker yet.
        }
        let resumed: PendingCounts
        do {
            resumed = try recoverPending(configuration: configuration, state: state,
                                         destination: destination, cancellation: cancellation)
        } catch CoreError.pendingFailed(2) {
            // A new generation may have a staged baseline but no activation
            // marker yet. The pull below performs the transition checks.
            resumed = PendingCounts()
        }
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
                            nil,
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
        guard status == P2P_SYNC_OK else {
            if resumed.updated != 0 || resumed.removed != 0 {
                throw CoreError.pullFailedAfterRecovery(status, resumed.updated, resumed.removed)
            }
            throw CoreError.pullFailed(status)
        }
        let recovered = try recoverPending(configuration: configuration, state: state,
                                           destination: destination, cancellation: cancellation)
        let (snapshot, details) = try receiverSnapshot(configuration: configuration,
                                                       state: state, destination: destination)
        let publishedLocally = recovered.updated + recovered.removed
        return PullSummary(
            eventsAdded: summary.sync.events_added, filesPublished: summary.sync.files_published,
            filesPresent: summary.sync.files_present,
            filesBlocked: max(summary.sync.files_blocked - min(summary.sync.files_blocked, publishedLocally),
                              recovered.blocked + resumed.blocked),
            filesUpdated: summary.sync.files_updated + resumed.updated + recovered.updated,
            filesRemoved: summary.sync.files_removed + resumed.removed + recovered.removed,
            protectedFiles: snapshot.protected_files, protectedBytes: snapshot.protected_bytes,
            protectedMissing: snapshot.protected_missing,
            originals: details.originals,
            conflicts: details.conflicts,
            history: details.history,
            historyBytes: details.history_bytes,
            historyMissing: details.history_missing
        )
    }

    static func recoverReceiver(configuration: URL, state: URL, destination: URL,
                                cancellation: Cancellation) throws -> UInt64 {
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let output = Array(destination.path.utf8)
        var added: UInt64 = 0
        var callbacks = PullContext(cancellation: cancellation, json: nil)
        let status = withUnsafeMutablePointer(to: &callbacks) { context in
            config.withUnsafeBufferPointer { configBytes in
                storage.withUnsafeBufferPointer { stateBytes in
                    output.withUnsafeBufferPointer { outputBytes in
                        p2p_sync_recover_receiver_v1(
                            configBytes.baseAddress, configBytes.count,
                            stateBytes.baseAddress, stateBytes.count,
                            outputBytes.baseAddress, outputBytes.count,
                            &added,
                            { context in
                                guard let context else { return 0 }
                                return context.assumingMemoryBound(to: PullContext.self)
                                    .pointee.cancellation.isCancelled() ? 1 : 0
                            }, UnsafeMutableRawPointer(context))
                    }
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.pullFailed(status) }
        return added
    }
}
