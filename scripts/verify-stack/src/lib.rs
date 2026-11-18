#![no_std]
use middleware::loader::trust::{PublicKey, Signature, verify_signature};
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }
#[unsafe(no_mangle)]
pub extern "C" fn vs_p256(key: &[u8; 33], msg: &[u8; 54], sig: &[u8; 64]) -> bool {
    verify_signature(&PublicKey::P256(*key), msg, &Signature(*sig))
}
#[unsafe(no_mangle)]
pub extern "C" fn vs_ed25519(key: &[u8; 32], msg: &[u8; 54], sig: &[u8; 64]) -> bool {
    verify_signature(&PublicKey::Ed25519(*key), msg, &Signature(*sig))
}
