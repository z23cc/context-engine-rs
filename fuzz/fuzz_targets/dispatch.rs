#![no_main]

use ctx_core::{FsCatalogProvider, RootPolicy, ScanOptions, handle_tool_call_json};
use libfuzzer_sys::fuzz_target;
use std::{env, fs, sync::OnceLock};

fn provider() -> &'static FsCatalogProvider {
    static PROVIDER: OnceLock<FsCatalogProvider> = OnceLock::new();
    PROVIDER.get_or_init(|| {
        let root = env::temp_dir().join("ctx-core-fuzz-dispatch-root");
        fs::create_dir_all(&root).expect("create fuzz corpus root");
        fs::write(root.join("text.txt"), b"needle\nsecond line\n").expect("write fuzz text");
        fs::write(root.join("lib.rs"), b"pub fn needle() {}\n").expect("write fuzz rust");
        FsCatalogProvider::new(
            RootPolicy::new(vec![root]).expect("root policy"),
            ScanOptions::default(),
        )
    })
}

fuzz_target!(|data: &[u8]| {
    let request = String::from_utf8_lossy(data);
    let _ = handle_tool_call_json(provider(), &request);
});
