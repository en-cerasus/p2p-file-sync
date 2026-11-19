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
   indicate configuration, local storage, TLS setup, sync, or internal error. */
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

/* Writer-only archive operations. History callback receives a JSON array
   with archive, current_heads, path, bytes and archived_at. The restore
   call accepts archive and reviewed-current IDs as 64-hex-peer:sequence.
   It records a new signed revision only when the reviewed head is current. */
int32_t p2p_sync_writer_history(const uint8_t *config, size_t config_len,
                                const uint8_t *state, size_t state_len,
                                p2p_sync_originals_callback callback, void *context);
int32_t p2p_sync_writer_restore(const uint8_t *config, size_t config_len,
                                const uint8_t *state, size_t state_len,
                                const uint8_t *archive, size_t archive_len,
                                const uint8_t *reviewed, size_t reviewed_len);

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

#ifdef __cplusplus
}
#endif

#endif
