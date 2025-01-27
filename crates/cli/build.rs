// Copyright 2024, 2025 New Vector Ltd.
//
// SPDX-License-Identifier: AGPL-3.0-only
// Please see LICENSE in the repository root for full details.

use vergen_gitcl::{Emitter, GitclBuilder, RustcBuilder};

fn main() -> anyhow::Result<()> {
    // At build time, we override the version through the environment variable
    // VERGEN_GIT_DESCRIBE. In some contexts, it means this variable is set but
    // empty, so we unset it here.
    if let Ok(ver) = std::env::var("VERGEN_GIT_DESCRIBE") {
        if ver.is_empty() {
            std::env::remove_var("VERGEN_GIT_DESCRIBE");
        }
    }

    let gitcl = GitclBuilder::default()
        .describe(true, false, Some("v*.*.*"))
        .build()?;
    let rustc = RustcBuilder::default().semver(true).build()?;

    Emitter::default()
        .add_instructions(&gitcl)?
        .add_instructions(&rustc)?
        .emit()?;

    Ok(())
}
