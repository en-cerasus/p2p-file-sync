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

#ifdef __cplusplus
}
#endif

#endif
