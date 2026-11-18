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

#ifdef __cplusplus
}
#endif

#endif
