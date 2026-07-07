//! Thin safe wrapper over BoringSSL/AWS-LC's SPAKE2 (`spake25519`), the exact
//! PAKE `adbd` uses. RustCrypto's `spake2` is a different construction and is
//! wire-incompatible, so we FFI the real thing (see `PAIRING_SPEC.md`).

use aws_lc_sys as ffi;

/// BoringSSL `SPAKE2_MAX_MSG_SIZE`.
const MAX_MSG: usize = 32;
/// BoringSSL `SPAKE2_MAX_KEY_SIZE`.
const MAX_KEY: usize = 64;

/// A SPAKE2 handshake context. One-shot: generate our message, then process the
/// peer's to derive the shared key.
pub struct Spake2 {
    ctx: *mut ffi::spake2_ctx_st,
}

impl Spake2 {
    /// Creates the client (Alice) context with adb's role/name constants. Names
    /// are passed with their length **including** the trailing NUL, matching
    /// adb's `sizeof(kClientName)`.
    pub fn new_client() -> Self {
        // "adb pair client\0" / "adb pair server\0" — len includes the NUL.
        let my = b"adb pair client\0";
        let their = b"adb pair server\0";
        let ctx = unsafe {
            ffi::SPAKE2_CTX_new(
                ffi::spake2_role_t_spake2_role_alice,
                my.as_ptr(),
                my.len(),
                their.as_ptr(),
                their.len(),
            )
        };
        assert!(!ctx.is_null(), "SPAKE2_CTX_new returned null");
        Self { ctx }
    }

    /// Generates our SPAKE2 message for `password` (adb's augmented password:
    /// code bytes ++ 64-byte TLS exporter). ~33 bytes.
    pub fn generate_msg(&mut self, password: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; MAX_MSG];
        let mut out_len = 0usize;
        let ok = unsafe {
            ffi::SPAKE2_generate_msg(
                self.ctx,
                out.as_mut_ptr(),
                &mut out_len,
                out.len(),
                password.as_ptr(),
                password.len(),
            )
        };
        assert_eq!(ok, 1, "SPAKE2_generate_msg failed");
        out.truncate(out_len);
        out
    }

    /// Processes the peer's message and returns the derived shared key (64 bytes).
    /// After this the context is spent.
    pub fn process_msg(&mut self, their_msg: &[u8]) -> Option<Vec<u8>> {
        let mut key = vec![0u8; MAX_KEY];
        let mut key_len = 0usize;
        let ok = unsafe {
            ffi::SPAKE2_process_msg(
                self.ctx,
                key.as_mut_ptr(),
                &mut key_len,
                key.len(),
                their_msg.as_ptr(),
                their_msg.len(),
            )
        };
        if ok != 1 {
            return None;
        }
        key.truncate(key_len);
        Some(key)
    }
}

impl Drop for Spake2 {
    fn drop(&mut self) {
        unsafe { ffi::SPAKE2_CTX_free(self.ctx) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_sys as ffi;

    /// A server (Bob) context, for the in-process roundtrip test only.
    fn new_server() -> Spake2 {
        let my = b"adb pair server\0";
        let their = b"adb pair client\0";
        let ctx = unsafe {
            ffi::SPAKE2_CTX_new(
                ffi::spake2_role_t_spake2_role_bob,
                my.as_ptr(),
                my.len(),
                their.as_ptr(),
                their.len(),
            )
        };
        assert!(!ctx.is_null());
        Spake2 { ctx }
    }

    #[test]
    fn same_password_agrees_on_key() {
        let pswd = b"123456-some-exporter-material";
        let mut alice = Spake2::new_client();
        let mut bob = new_server();
        let a_msg = alice.generate_msg(pswd);
        let b_msg = bob.generate_msg(pswd);
        assert!(a_msg.len() >= 32, "msg len {}", a_msg.len());
        let a_key = alice.process_msg(&b_msg).expect("alice derives key");
        let b_key = bob.process_msg(&a_msg).expect("bob derives key");
        assert_eq!(a_key, b_key, "SPAKE2 keys must match for equal passwords");
        assert_eq!(a_key.len(), 64);
    }

    #[test]
    fn different_password_disagrees() {
        let mut alice = Spake2::new_client();
        let mut bob = new_server();
        let a_msg = alice.generate_msg(b"password-A");
        let b_msg = bob.generate_msg(b"password-B");
        let a_key = alice.process_msg(&b_msg).expect("derive");
        let b_key = bob.process_msg(&a_msg).expect("derive");
        assert_ne!(a_key, b_key, "different passwords must not agree");
    }
}
