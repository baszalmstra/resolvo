// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use anyhow::Context;
use std::path::{Path, PathBuf};

fn main() -> anyhow::Result<()> {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());

    println!("cargo:rerun-if-env-changed=RESOLVO_GENERATED_INCLUDE_DIR");
    let output_dir = std::env::var_os("RESOLVO_GENERATED_INCLUDE_DIR").unwrap_or_else(|| {
        Path::new(&std::env::var_os("OUT_DIR").unwrap())
            .join("generated_include")
            .into()
    });
    let output_dir = Path::new(&output_dir);

    println!("cargo:GENERATED_INCLUDE_DIR={}", output_dir.display());

    std::fs::create_dir_all(output_dir).context("Could not create the include directory")?;

    let mut config = cbindgen::Config::default();
    config.macro_expansion.bitflags = true;
    config.pragma_once = true;
    config.include_version = true;
    config.namespaces = Some(vec!["resolvo".into(), "cbindgen_private".into()]);
    config.line_length = 100;
    config.tab_width = 4;
    config.language = cbindgen::Language::Cxx;
    config.cpp_compat = true;
    config.documentation = true;
    config.documentation_style = cbindgen::DocumentationStyle::Doxy;
    config.structure.associated_constants_in_body = true;
    config.constant.allow_constexpr = true;

    //     cbindgen::Builder::new()
    //         .with_crate(manifest_dir)
    //         .with_config(config)
    //         .with_after_include(
    //             r"
    // namespace resolvo {
    //     namespace cbindgen_private {
    //         using SolvableId = uint32_t;
    //         using VersionSetId = uint32_t;
    //         using NameId = uint32_t;
    //     }
    // }",
    //         )
    //         .generate()
    //         .expect("Unable to generate bindings")
    //         .write_to_file(output_dir.join("resolvo_internal.h"));

    cbindgen::Builder::new()
        .with_config(config.clone())
        .with_src(manifest_dir.join("src/vector.rs"))
        .generate()
        .context("Unable to generate bindings for resolvo_vector_internal.h")?
        .write_to_file(output_dir.join("resolvo_vector_internal.h"));

    Ok(())
}
