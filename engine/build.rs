// Task 2.8 — fail at BUILD time (not first page load) if the vendored htmx is
// missing. It is COMMITTED at engine/assets/htmx.min.js on purpose: the binary
// embeds it (include_bytes! in ui.rs) and must stay self-contained offline.
fn main() {
    println!("cargo:rerun-if-changed=assets/htmx.min.js");
    if !std::path::Path::new("assets/htmx.min.js").exists() {
        panic!(
            "engine/assets/htmx.min.js is missing — vendor it once with:\n  \
             curl -sL https://unpkg.com/htmx.org@2/dist/htmx.min.js -o engine/assets/htmx.min.js\n\
             and commit it (SPEC.md Task 2.8)."
        );
    }
    // v3.4 (R.3): same self-contained rule for the favicon (status.md §R).
    println!("cargo:rerun-if-changed=assets/favicon.ico");
    if !std::path::Path::new("assets/favicon.ico").exists() {
        panic!("engine/assets/favicon.ico is missing — it is committed; restore it from git.");
    }
}
