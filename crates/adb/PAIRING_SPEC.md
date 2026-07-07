# adb wireless pairing — byte-exact wire spec

Ported from first-hand AOSP source (`packages/modules/adb`, read directly) so the
Rust implementation matches `adbd` byte-for-byte. File refs are to that tree.

We are the **Client** (SPAKE2 *Alice*); `adbd` is the **Server** (*Bob*).

## Flow (`pairing_connection.cpp:407-436` StartWorker)

```
TCP connect to <host>:<pairing-port>        # port + 6-digit code from the "Pair
                                            # device with pairing code" dialog
TLS 1.3 handshake (client):
  - present a client cert (PEM) built from our adb RSA key
  - accept ANY server cert (adbd sets verify-callback → 1)
pswd = code_ascii_bytes ++ TLS_exporter(64)  # channel binding, see below
build SPAKE2 ctx (Alice) with pswd
State ExchangingMsgs:
  send  SPAKE2_MSG packet (our 33-byte msg)
  recv  SPAKE2_MSG packet → SPAKE2_process_msg → 64B key → HKDF → AES-128-GCM
State ExchangingPeerInfo:
  send  PEER_INFO packet (AES-GCM(our 8192B PeerInfo))
  recv  PEER_INFO packet → decrypt → their PeerInfo
done → adbd has stored our pubkey as authorized
```
Client **writes then reads** in both states (`:304/311`, `:351/365`).

## PairingPacketHeader — 6 bytes, packed (`pairing_connection.cpp:46-50`)

| off | field   | type | notes |
|-----|---------|------|-------|
| 0   | version | u8   | `1` (kCurrentKeyHeaderVersion) |
| 1   | type    | u8   | `0`=SPAKE2_MSG, `1`=PEER_INFO (`proto/pairing.proto:27-28`) |
| 2   | payload | u32  | **big-endian** (htonl/ntohl), payload byte count |

Payload bytes follow the header immediately. Reject on read if `version != 1` or
`payload == 0 || payload > kMaxPayloadSize` (`= kMaxPeerInfoSize*2 = 16384`).

## TLS exporter (`tls/tls_connection.cpp:36,185`)

```
SSL_export_keying_material(out, 64,
    label = "adb-label", label_len = sizeof = 10 (INCLUDES trailing NUL),
    context = NULL, use_context = false)
```
rustls: `conn.export_keying_material(&mut out64, b"adb-label\0", None)` — label is
the 10 bytes `adb-label\0`; `None` context (not `Some(&[])`).

## SPAKE2 (`pairing_auth/pairing_auth.cpp`) — BoringSSL spake25519 (ed25519), NOT P-256

RustCrypto `spake2` is wire-incompatible; MUST FFI BoringSSL/AWS-LC.

```
Client: SPAKE2_CTX_new(spake2_role_alice,
                       "adb pair client", 16,   # sizeof — INCLUDES the NUL
                       "adb pair server", 16)
SPAKE2_generate_msg(ctx, out, &len, SPAKE2_MAX_MSG_SIZE(=32), pswd, pswd_len)  # emits 33B
SPAKE2_process_msg(ctx, key, &klen, SPAKE2_MAX_KEY_SIZE(=64), their_msg, their_len)  # 64B key
```
Password fed raw (code ascii ++ 64B exporter), unhashed.

## HKDF → AES key (`pairing_auth/aes_128_gcm.cpp:41-45`)

```
HKDF(out=16B, SHA-256, ikm = 64B SPAKE2 key, salt = none,
     info = "adb pairing_auth aes-128-gcm key", info_len = 32 (EXCLUDES NUL))
```
Note: exporter label includes its NUL (10); HKDF info excludes its NUL (32).

## AES-128-GCM framing (`pairing_auth/aes_128_gcm.cpp:50-80`, `.h:57-59`)

- 12-byte nonce = `u64 seq` little-endian in bytes[0..8], bytes[8..12] = 0.
- Separate `enc_sequence_`/`dec_sequence_`, both start 0, **post-increment** per op.
- No AAD. Tag = 16 bytes, appended (default GCM tag). Ciphertext = plaintext + 16.
- In pairing each direction encrypts exactly once, so both seqs are 0 in practice.

## PeerInfo — 8192 bytes, packed (`pairing_connection.h:40-46`)

```
struct { u8 type; u8 data[8191]; }   // type = ADB_RSA_PUB_KEY = 0
```
`data` = adb public-key line, zero-padded. The **whole 8192-byte struct** is
encrypted (→ 8208B ciphertext). adb key line = base64(ANDROID_PUBKEY 524B struct)
+ " " + "user@host" (`client/adb_wifi.cpp:207-212`, `crypto_utils/android_pubkey`).

## Connect (A_STLS) — later, stage 4

`A_STLS = 0x534C5453` ("STLS"), `A_STLS_VERSION = 0x01000000`. After the banner,
adbd requests TLS; client authenticates with an X.509 cert wrapping the SAME adb
RSA key registered at pairing. (Confirm exact version bytes in `adb.h` before use.)
```
