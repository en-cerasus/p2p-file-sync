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
    }

    private struct PullContext {
        let cancellation: Cancellation
        var json: Data?
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
            history: details.history
        )
    }
}
