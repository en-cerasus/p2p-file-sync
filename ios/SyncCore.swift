import CP2PSync
import Foundation

enum SyncCore {
    enum CoreError: Error {
        case incompatibleABI(UInt32)
        case hashingFailed(Int32)
        case pullFailed(Int32)
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

    struct PullSummary: Sendable {
        let eventsAdded: UInt64
        let filesPublished: UInt64
        let filesPresent: UInt64
        let filesBlocked: UInt64
        let filesUpdated: UInt64
        let filesRemoved: UInt64
    }

    static func pull(configuration: URL, state: URL, destination: URL) throws -> PullSummary {
        var summary = p2p_sync_pull_summary_v2()
        let config = Array(configuration.path.utf8)
        let storage = Array(state.path.utf8)
        let output = Array(destination.path.utf8)
        let status = config.withUnsafeBufferPointer { configBytes in
            storage.withUnsafeBufferPointer { storageBytes in
                output.withUnsafeBufferPointer { outputBytes in
                    p2p_sync_pull_v2(
                        configBytes.baseAddress, configBytes.count,
                        storageBytes.baseAddress, storageBytes.count,
                        outputBytes.baseAddress, outputBytes.count,
                        &summary
                    )
                }
            }
        }
        guard status == P2P_SYNC_OK else { throw CoreError.pullFailed(status) }
        return PullSummary(
            eventsAdded: summary.events_added, filesPublished: summary.files_published,
            filesPresent: summary.files_present, filesBlocked: summary.files_blocked,
            filesUpdated: summary.files_updated, filesRemoved: summary.files_removed
        )
    }
}
