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
use buck2_error::buck2_error;
use buck2_execute::execute::request::OutputType;
use buck2_fs::paths::abs_norm_path::AbsNormPath;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use starlark::codemap::FileSpan;
use starlark::values::Heap;

use super::AnalysisRegistry;

impl<'v> AnalysisRegistry<'v> {
    pub fn declare_store_output(
        &mut self,
        logical_store_path: AbsNormPathBuf,
        output_type: OutputType,
        declaration_location: Option<FileSpan>,
        heap: Heap<'v>,
    ) -> buck2_error::Result<DeclaredArtifact<'v>> {
        self.declare_store_output_with_resolution(
            logical_store_path,
            output_type,
            BuckOutPathKind::Configuration,
            declaration_location,
            heap,
        )
    }

    pub fn declare_imported_store_output(
        &mut self,
        logical_store_path: AbsNormPathBuf,
        output_type: OutputType,
        declaration_location: Option<FileSpan>,
        heap: Heap<'v>,
    ) -> buck2_error::Result<DeclaredArtifact<'v>> {
        self.declare_store_output_with_resolution(
            logical_store_path,
            output_type,
            BuckOutPathKind::ContentHash,
            declaration_location,
            heap,
        )
    }

    fn declare_store_output_with_resolution(
        &mut self,
        logical_store_path: AbsNormPathBuf,
        output_type: OutputType,
        path_resolution_method: BuckOutPathKind,
        declaration_location: Option<FileSpan>,
        heap: Heap<'v>,
    ) -> buck2_error::Result<DeclaredArtifact<'v>> {
        let store_root = AbsNormPath::new("/pkgs/store")?;

        if !logical_store_path.starts_with(store_root) {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "store output path must be under `/pkgs/store`: `{}`",
                logical_store_path
            ));
        }

        let relative_store_path = logical_store_path.strip_prefix(store_root)?;
        if relative_store_path.is_empty() {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "store output path must name an entry below `/pkgs/store`"
            ));
        }

        let staged_path = ForwardRelativePathBuf::try_from(format!(
            "__buckpkgs_store__/{}",
            relative_store_path
        ))?;
        self.actions.declare_store_artifact(
            staged_path,
            logical_store_path,
            output_type,
            path_resolution_method,
            declaration_location,
            heap,
        )
    }
}
