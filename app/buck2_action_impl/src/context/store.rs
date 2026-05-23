/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use buck2_build_api::interpreter::rule_defs::artifact::associated::AssociatedArtifacts;
use buck2_build_api::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsInputArtifactLike;
use buck2_build_api::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use buck2_build_api::interpreter::rule_defs::context::AnalysisActions;
use buck2_build_api::interpreter::rule_defs::store_path::StarlarkStorePath;
use buck2_common::cas_digest::CasDigest;
use buck2_core::execution_types::executor_config::RemoteExecutorUseCase;
use buck2_error::BuckErrorContext;
use buck2_execute::execute::request::OutputType;
use buck2_hash::buck_indexset;
use starlark::environment::MethodsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::ValueTyped;
use starlark::values::list_or_tuple::UnpackListOrTuple;

use crate::actions::impls::cas_store_import::UnregisteredCasStoreImportAction;
use crate::actions::impls::store_import::ExpectedStoreManifest;
use crate::actions::impls::store_import::UnregisteredStoreImportAction;

#[starlark_module]
pub(crate) fn analysis_actions_methods_store(builder: &mut MethodsBuilder) {
    /// Declares a BuckPkgs store-backed output. The output still executes
    /// through Buck2's normal staged artifact path; its logical absolute store
    /// path is retained for store-aware consumers.
    fn store_path(
        this: &AnalysisActions,
        #[starlark(require = named)] store_path_key: &str,
        #[starlark(require = named)] store_name: &str,
    ) -> starlark::Result<StarlarkStorePath> {
        let _unused = this;
        Ok(StarlarkStorePath::from_identity(
            store_path_key,
            store_name,
        )?)
    }

    /// Declares a BuckPkgs store-backed output. The output still executes
    /// through Buck2's normal staged artifact path; its logical absolute store
    /// path is retained for store-aware consumers.
    fn declare_store_output<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = named)] store_path: ValueTyped<'v, StarlarkStorePath>,
        #[starlark(require = named, default = false)] dir: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkDeclaredArtifact<'v>> {
        let output_type = if dir {
            OutputType::Directory
        } else {
            OutputType::FileOrDirectory
        };
        let artifact = this.state()?.declare_store_output(
            store_path.path().clone(),
            output_type,
            eval.call_stack_top_location(),
            eval.heap(),
        )?;

        Ok(StarlarkDeclaredArtifact::new(
            eval.call_stack_top_location(),
            artifact,
            AssociatedArtifacts::new(),
        ))
    }

    /// Imports an immutable BuckPkgs object already hydrated at its logical
    /// store path without creating a staged copy of its payload.
    fn import_store_output<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = named)] store_path: ValueTyped<'v, StarlarkStorePath>,
        #[starlark(require = named, default = false)] dir: bool,
        #[starlark(require = named)] missing_hint: Option<&str>,
        #[starlark(require = named)] manifest: ValueAsInputArtifactLike<'v>,
        #[starlark(require = named)] package_name: &str,
        #[starlark(require = named)] version: &str,
        #[starlark(require = named)] output: &str,
        #[starlark(require = named)] target_system: &str,
        #[starlark(require = named)] references: UnpackListOrTuple<&'v str>,
        #[starlark(require = named)] runtime_store_outputs: UnpackListOrTuple<&'v str>,
        #[starlark(require = named)] canonical_tree_hash: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkDeclaredArtifact<'v>> {
        let output_type = if dir {
            OutputType::Directory
        } else {
            OutputType::FileOrDirectory
        };
        let mut state = this.state()?;
        let artifact = state.declare_imported_store_output(
            store_path.path().clone(),
            output_type,
            eval.call_stack_top_location(),
            eval.heap(),
        )?;
        state.register_action(
            buck_indexset![artifact.as_output()],
            UnregisteredStoreImportAction {
                missing_hint: missing_hint.map(str::to_owned),
                manifest: manifest.0.get_artifact_group()?,
                expected_manifest: ExpectedStoreManifest {
                    package_name: package_name.to_owned(),
                    version: version.to_owned(),
                    output: output.to_owned(),
                    target_system: target_system.to_owned(),
                    references: references.items.into_iter().map(str::to_owned).collect(),
                    runtime_store_outputs: runtime_store_outputs
                        .items
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    canonical_tree_hash: canonical_tree_hash.to_owned(),
                },
            },
            None,
            None,
        )?;

        Ok(StarlarkDeclaredArtifact::new(
            eval.call_stack_top_location(),
            artifact,
            AssociatedArtifacts::new(),
        ))
    }

    /// Imports an immutable BuckPkgs object published as a REAPI directory
    /// graph in CAS and atomically realizes it at its logical store path.
    fn import_cas_store_output<'v>(
        this: &AnalysisActions<'v>,
        #[starlark(require = named)] store_path: ValueTyped<'v, StarlarkStorePath>,
        #[starlark(require = named, default = false)] dir: bool,
        #[starlark(require = named)] manifest: ValueAsInputArtifactLike<'v>,
        #[starlark(require = named)] cas_root_digest: &str,
        #[starlark(require = named)] re_use_case: &str,
        #[starlark(require = named)] package_name: &str,
        #[starlark(require = named)] version: &str,
        #[starlark(require = named)] output: &str,
        #[starlark(require = named)] target_system: &str,
        #[starlark(require = named)] references: UnpackListOrTuple<&'v str>,
        #[starlark(require = named)] runtime_store_outputs: UnpackListOrTuple<&'v str>,
        #[starlark(require = named)] canonical_tree_hash: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkDeclaredArtifact<'v>> {
        let output_type = if dir {
            OutputType::Directory
        } else {
            OutputType::FileOrDirectory
        };
        let root_digest =
            CasDigest::parse_digest(cas_root_digest, this.digest_config.cas_digest_config())
                .with_buck_error_context(|| {
                    format!("Not a valid RE store root digest: `{}`", cas_root_digest)
                })?
                .0;
        let mut state = this.state()?;
        let artifact = state.declare_imported_store_output(
            store_path.path().clone(),
            output_type,
            eval.call_stack_top_location(),
            eval.heap(),
        )?;
        state.register_action(
            buck_indexset![artifact.as_output()],
            UnregisteredCasStoreImportAction {
                manifest: manifest.0.get_artifact_group()?,
                expected_manifest: ExpectedStoreManifest {
                    package_name: package_name.to_owned(),
                    version: version.to_owned(),
                    output: output.to_owned(),
                    target_system: target_system.to_owned(),
                    references: references.items.into_iter().map(str::to_owned).collect(),
                    runtime_store_outputs: runtime_store_outputs
                        .items
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    canonical_tree_hash: canonical_tree_hash.to_owned(),
                },
                root_digest,
                re_use_case: RemoteExecutorUseCase::new(re_use_case.to_owned()),
            },
            None,
            None,
        )?;

        Ok(StarlarkDeclaredArtifact::new(
            eval.call_stack_top_location(),
            artifact,
            AssociatedArtifacts::new(),
        ))
    }
}
