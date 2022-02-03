use std::{collections::HashSet, env, path::PathBuf};

use bindgen::callbacks::{MacroParsingBehavior, ParseCallbacks};

fn main() {
    // Invalidate the built crate if any of the build files change
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=wrapper.h");

    // Link dynamically for now
    pkg_config::Config::new()
        .cargo_metadata(true)
        .print_system_libs(false)
        .atleast_version("0.9")
        .probe("libssh")
        .expect("libssh not found");

    // Some C macros collide with equally named enums. Use bindgen's callbacks to ignore them.
    let callbacks = LibsshCallbacks(vec!["IPPORT_RESERVED".into()].into_iter().collect());

    // Generate C to Rust bindings
    let bindings = bindgen::builder()
        .header("wrapper.h")
        .layout_tests(false) // avoid null pointer dereference warnings
        .parse_callbacks(Box::new(callbacks))
        .generate()
        .expect("Unable to generate bindings");

    // Write bindings to file
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Could not write bindings");
}

#[derive(Debug)]
struct LibsshCallbacks(HashSet<String>);

impl ParseCallbacks for LibsshCallbacks {
    fn include_file(&self, filename: &str) {
        println!("cargo:rerun-if-changed={}", filename);
    }

    fn will_parse_macro(&self, name: &str) -> MacroParsingBehavior {
        if self.0.contains(name) {
            MacroParsingBehavior::Ignore
        } else {
            MacroParsingBehavior::Default
        }
    }
}
