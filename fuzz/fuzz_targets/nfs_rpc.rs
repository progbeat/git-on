#![no_main]

use libfuzzer_sys::fuzz_target;

#[allow(dead_code)]
#[path = "../../src/backend/nfs/protocol.rs"]
mod protocol;
#[allow(dead_code)]
#[path = "../../src/backend/nfs/xdr.rs"]
mod xdr;

fuzz_target!(|data: &[u8]| {
    if let Some(header) = data.first_chunk::<4>() {
        let _ = xdr::fragment(*header, data.len());
    }
    let unwrapped = match protocol::rpc(data) {
        Ok((_, protocol::Rpc::Compound(_, body))) => body,
        _ => data,
    };
    if let Ok(mut compound) = protocol::compound(unwrapped) {
        for _ in 0..(compound.count as usize).min(protocol::MAX_OPS) {
            let Ok(code) = compound.ops.u32() else { break };
            if protocol::operation(code, &mut compound.ops).is_err() {
                break;
            }
        }
    }
    // Reach operation decoders without depending on the fuzzer to construct
    // an authenticated RPC wrapper or a complete COMPOUND first.
    if let Some((&code, rest)) = data.split_first() {
        let _ = protocol::operation(u32::from(code), &mut xdr::Reader::new(rest));
    }
});
