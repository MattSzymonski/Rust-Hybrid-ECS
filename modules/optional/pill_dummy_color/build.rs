// REQUIREMENTS
//   Rust (stable). Run by Cargo as this crate's build script.
//
// DESCRIPTION
//   Emits this crate's function-address inventory so the host can redirect any
//   of its functions with nothing in the source annotated. The work lives in
//   `pill_hot_scan`, which the host also uses to decide what is patchable - the
//   two must agree byte for byte, so there is exactly one implementation.
//
// USAGE
//   cargo build -p pill_dummy_color
//
// --- SCRIPT ---

fn main() {
    pill_hot_scan::generate_function_inventory();
}
