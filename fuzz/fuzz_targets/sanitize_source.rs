#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    zirv_flow_templates::check_sanitize_source(data);
});
