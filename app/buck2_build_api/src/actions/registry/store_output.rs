/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use buck2_artifact::artifact::artifact_type::DeclaredArtifact;
use buck2_core::fs::buck_out_path::BuckOutPathKind;
use buck2_core::fs::buck_out_path::BuildArtifactPath;
use buck2_execute::execute::request::OutputType;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use dupe::Dupe;
use starlark::codemap::FileSpan;
use starlark::values::Heap;

use super::ActionsRegistry;

impl<'v> ActionsRegistry<'v> {
    /// Declares a staged build output that carries a logical BuckPkgs store path.
    /// Executors still operate on the staged project-relative path.
    pub fn declare_store_artifact(
        &mut self,
        staged_path: ForwardRelativePathBuf,
        logical_store_path: AbsNormPathBuf,
        output_type: OutputType,
        path_resolution_method: BuckOutPathKind,
        declaration_location: Option<FileSpan>,
        heap: Heap<'v>,
    ) -> buck2_error::Result<DeclaredArtifact<'v>> {
        self.claim_output_path(&staged_path, declaration_location)?;
        let out_path = BuildArtifactPath::with_store_path(
            self.owner.dupe(),
            staged_path,
            path_resolution_method,
            logical_store_path.to_string(),
        );
        let declared = DeclaredArtifact::new(out_path, output_type, 0, heap);
        if !self.artifacts.insert(declared.dupe()) {
            panic!(
                "not expected duplicate store artifact after output path was successfully claimed"
            );
        }
        Ok(declared)
    }
}
