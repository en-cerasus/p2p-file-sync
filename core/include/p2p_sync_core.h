#ifndef P2P_SYNC_CORE_H
#define P2P_SYNC_CORE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define P2P_SYNC_SHA256_SIZE 32
#define P2P_SYNC_OK 0
#define P2P_SYNC_INVALID_ARGUMENT 1
#define P2P_SYNC_CANCELLED 7
#define P2P_SYNC_ROLLBACK_DETECTED 8

uint32_t p2p_sync_abi_version(void);

/* Buffers are borrowed only for the call. A null input is valid for length 0. */
int32_t p2p_sync_sha256(const uint8_t *input, size_t input_len,
                       uint8_t *output, size_t output_len);

typedef struct {
    uint64_t events_added;
    uint64_t files_published;
    uint64_t files_present;
    uint64_t files_blocked;
} p2p_sync_pull_summary;

/* Blocking call. Output is written only on success. Return codes 2–6
   indicate configuration, local storage, TLS setup, sync, or internal error.
   Code 8 means a deletion witness is absent from the restored local DB. */
int32_t p2p_sync_pull(const uint8_t *config, size_t config_len,
                      const uint8_t *state, size_t state_len,
                      const uint8_t *destination, size_t destination_len,
                      p2p_sync_pull_summary *output);

typedef struct {
    uint64_t events_added;
    uint64_t files_published;
    uint64_t files_present;
    uint64_t files_blocked;
    uint64_t files_updated;
    uint64_t files_removed;
} p2p_sync_pull_summary_v2;

int32_t p2p_sync_pull_v2(const uint8_t *config, size_t config_len,
                         const uint8_t *state, size_t state_len,
                         const uint8_t *destination, size_t destination_len,
                         p2p_sync_pull_summary_v2 *output);

typedef struct {
    p2p_sync_pull_summary_v2 sync;
    uint64_t protected_files;
    uint64_t protected_bytes;
    uint64_t protected_missing;
} p2p_sync_pull_summary_v3;

int32_t p2p_sync_pull_v3(const uint8_t *config, size_t config_len,
                         const uint8_t *state, size_t state_len,
                         const uint8_t *destination, size_t destination_len,
                         p2p_sync_pull_summary_v3 *output);

/* On success the optional callback receives one UTF-8 JSON array on
   the calling thread. Copy the bytes before returning from the callback.
   Entries have hash (32 bytes), names (strings), bytes (integer or null),
   conflict (boolean) and waiting (objects with peer in hex and state
   "confirmation" or "events"). */
typedef void (*p2p_sync_originals_callback)(void *context, const uint8_t *json, size_t len);
int32_t p2p_sync_pull_v4(const uint8_t *config, size_t config_len,
                         const uint8_t *state, size_t state_len,
                         const uint8_t *destination, size_t destination_len,
                         p2p_sync_pull_summary_v3 *output,
                         p2p_sync_originals_callback originals, void *context);

/* The optional cancellation callback runs on the calling thread. Return a
   nonzero value to stop at the next I/O boundary. The output stays unchanged
   on cancellation. Both callbacks borrow the same context. */
typedef uint8_t (*p2p_sync_cancel_callback)(void *context);
int32_t p2p_sync_pull_v5(const uint8_t *config, size_t config_len,
                         const uint8_t *state, size_t state_len,
                         const uint8_t *destination, size_t destination_len,
                         p2p_sync_pull_summary_v3 *output,
                         p2p_sync_originals_callback originals, void *context,
                         p2p_sync_cancel_callback cancellation);

/* Same call contract as v5. The callback JSON is an object with originals,
   conflicts and history arrays, plus history_bytes and history_missing.
   History entries include nullable path
   (components), content (hash and length), and archived_at (Unix seconds).
   A conflict has object (hex ID) and versions. Each
   version has peer (hex ID), sequence, nullable name, kind (file, directory,
   deleted) and nullable bytes (missing local file contents when null). */
int32_t p2p_sync_pull_v6(const uint8_t *config, size_t config_len,
                         const uint8_t *state, size_t state_len,
                         const uint8_t *destination, size_t destination_len,
                         p2p_sync_pull_summary_v3 *output,
                         p2p_sync_originals_callback details, void *context,
                         p2p_sync_cancel_callback cancellation);

/* iOS receiver: list staged changes to existing files as JSON. Each item has
   object, kind (update/delete/move), source components, and optional
   destination components. All paths are relative to the selected destination.
   Fetching is finished before these calls. The apply call must run inside an
   NSFileCoordinator accessor covering source and destination write URLs.
   outcome: 0 no pending, 1 applied, 2 already applied, 3 diverged,
   4 superseded. Reviewed JSON must match the current staged item. */
int32_t p2p_sync_ios_pending_v1(const uint8_t *config, size_t config_len,
                                 const uint8_t *state, size_t state_len,
                                 const uint8_t *destination, size_t destination_len,
                                 p2p_sync_originals_callback callback, void *context);
int32_t p2p_sync_ios_apply_v1(const uint8_t *config, size_t config_len,
                               const uint8_t *state, size_t state_len,
                               const uint8_t *destination, size_t destination_len,
                               const uint8_t *reviewed, size_t reviewed_len,
                               int32_t *outcome);
/* Recompute protected-original counts and details after local recovery.
   The sync counters in output are zero; callback JSON matches pull_v6. */
int32_t p2p_sync_ios_snapshot_v1(const uint8_t *config, size_t config_len,
                                  const uint8_t *state, size_t state_len,
                                  const uint8_t *destination, size_t destination_len,
                                  p2p_sync_pull_summary_v3 *output,
                                  p2p_sync_originals_callback details, void *context);

/* Writer-only archive operations. History callback receives a JSON array
   with archive, current_heads, path, bytes and archived_at. The restore
   call accepts archive and reviewed-current IDs as 64-hex-peer:sequence.
   It records a new signed revision only when the reviewed head is current. */
int32_t p2p_sync_writer_history(const uint8_t *config, size_t config_len,
                                const uint8_t *state, size_t state_len,
                                p2p_sync_originals_callback callback, void *context);
/* Writer-side protected common originals, deduplicated by content hash.
   Callback JSON has files, bytes, missing and originals. */
int32_t p2p_sync_writer_protected_originals(const uint8_t *config, size_t config_len,
                                            const uint8_t *state, size_t state_len,
                                            p2p_sync_originals_callback callback, void *context);
int32_t p2p_sync_writer_restore(const uint8_t *config, size_t config_len,
                                const uint8_t *state, size_t state_len,
                                const uint8_t *archive, size_t archive_len,
                                const uint8_t *reviewed, size_t reviewed_len);
/* Delete selected history entries. Selection is a nonempty JSON array of
   id, archive, replaced_by and archived_at from the listing. Protected
   originals, current versions, pending recovery and other archives retain
   their content. Stale selections reject the whole request. */
int32_t p2p_sync_writer_delete_history(const uint8_t *config, size_t config_len,
                                       const uint8_t *state, size_t state_len,
                                       const uint8_t *selection, size_t selection_len);

/* Conflict listing returns groups with versions (id, name, kind). Resolve
   accepts the chosen ID and a JSON array of every reviewed candidate ID.
   It records a signed resolution only if the complete candidate set remains
   current. Directory conflicts cannot currently be resolved. */
int32_t p2p_sync_writer_conflicts(const uint8_t *config, size_t config_len,
                                  const uint8_t *state, size_t state_len,
                                  p2p_sync_originals_callback callback, void *context);
int32_t p2p_sync_writer_resolve(const uint8_t *config, size_t config_len,
                                const uint8_t *state, size_t state_len,
                                const uint8_t *choice, size_t choice_len,
                                const uint8_t *reviewed, size_t reviewed_len);

/* Runs an authenticated writer listener until cancellation returns nonzero.
   The ready callback receives the bound address as borrowed UTF-8 bytes.
   Invalid peer sessions do not stop the listener. Returns 7 when stopped. */
int32_t p2p_sync_writer_serve(const uint8_t *config, size_t config_len,
                              const uint8_t *state, size_t state_len,
                              p2p_sync_originals_callback ready,
                              p2p_sync_cancel_callback cancellation, void *context);

/* Publish new and changed regular files from a selected source folder.
   The source must not contain private keys or the state directory. Missing
   source files do not become deletions. Callback JSON has files,
   directories and skipped counts. */
int32_t p2p_sync_writer_refresh_tree(const uint8_t *config, size_t config_len,
                                     const uint8_t *state, size_t state_len,
                                     const uint8_t *source, size_t source_len,
                                     p2p_sync_originals_callback callback, void *context);

/* Review locally issued files absent from an accessible source parent. The callback JSON is
   an array of {path: [components], head: "<peer>:<sequence>"}. A missing
   source root or parent is not treated as proof of deletion. */
int32_t p2p_sync_writer_missing_files(const uint8_t *config, size_t config_len,
                                      const uint8_t *state, size_t state_len,
                                      const uint8_t *source, size_t source_len,
                                      p2p_sync_originals_callback callback, void *context);

/* Publish a tombstone only while the selected head is current and its source
   name remains absent. This leaves the local source tree untouched. */
int32_t p2p_sync_writer_delete_missing_file(const uint8_t *config, size_t config_len,
                                            const uint8_t *state, size_t state_len,
                                            const uint8_t *source, size_t source_len,
                                            const uint8_t *shared_path, size_t shared_path_len,
                                            const uint8_t *reviewed, size_t reviewed_len);

/* Preserve a reviewed file ID after the user selects its new physical file
   inside the source root. The old name must remain absent, the new file must
   match the cached prior contents, and the destination must be unoccupied in
   the shared index. The call never moves the physical source file. */
int32_t p2p_sync_writer_relocate_missing_file(const uint8_t *config, size_t config_len,
                                              const uint8_t *state, size_t state_len,
                                              const uint8_t *source, size_t source_len,
                                              const uint8_t *shared_path, size_t shared_path_len,
                                              const uint8_t *reviewed, size_t reviewed_len,
                                              const uint8_t *destination, size_t destination_len);

/* Fetch signed peer changes and verified contents into a writer's serving
   cache. Peer endpoints come from the selected direct.json. On success,
   events_added receives the number of newly imported events. */
int32_t p2p_sync_writer_catch_up(const uint8_t *config, size_t config_len,
                                const uint8_t *state, size_t state_len,
                                const uint8_t *peers, size_t peers_len,
                                uint64_t *events_added);
/* The cancellable form returns 7 when interrupted. Completed chunks and
   accepted events remain available for the next attempt. */
int32_t p2p_sync_writer_catch_up_v2(const uint8_t *config, size_t config_len,
                                   const uint8_t *state, size_t state_len,
                                   const uint8_t *peers, size_t peers_len,
                                   uint64_t *events_added,
                                   p2p_sync_cancel_callback cancellation, void *context);

#ifdef __cplusplus
}
#endif

#endif
