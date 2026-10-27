#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    zirv_flow_segments::check_segment_ast(data);
});
