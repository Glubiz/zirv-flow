#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    zirv_flow_connectors::manifest::check_manifest(data);
});
