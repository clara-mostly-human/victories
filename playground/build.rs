// The playground shows what the bot's tree search saw at the root. It reads that from a
// small hook in the bot's policy (`LAST_ROOT`). If the policy being built against has no
// such hook (an older commit), the playground still builds and shows the chosen turn only.
fn main() {
    let policy = "../field-marshal/crates/bot/src/policy.rs";
    println!("cargo:rerun-if-changed={policy}");
    println!("cargo:rustc-check-cfg=cfg(root_hook)");
    if std::fs::read_to_string(policy).map(|s| s.contains("pub static LAST_ROOT")).unwrap_or(false) {
        println!("cargo:rustc-cfg=root_hook");
    }
}
