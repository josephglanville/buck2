/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::borrow::Cow;
use std::slice;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use buck2_artifact::artifact::build_artifact::BuildArtifact;
use buck2_build_api::actions::Action;
use buck2_build_api::actions::ActionExecutionCtx;
use buck2_build_api::actions::UnregisteredAction;
use buck2_build_api::actions::execute::action_executor::ActionExecutionKind;
use buck2_build_api::actions::execute::action_executor::ActionExecutionMetadata;
use buck2_build_api::actions::execute::action_executor::ActionOutputs;
use buck2_build_api::actions::execute::error::ExecuteError;
use buck2_build_api::artifact_groups::ArtifactGroup;
use buck2_build_signals::env::WaitingData;
use buck2_common::file_ops::metadata::FileDigest;
use buck2_core::category::CategoryRef;
use buck2_core::execution_types::executor_config::RemoteExecutorUseCase;
use buck2_error::BuckErrorContext;
use buck2_error::internal_error;
use buck2_execute::artifact::artifact_dyn::ArtifactDyn;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::digest::CasDigestToReExt;
use buck2_execute::directory::ActionDirectoryEntry;
use buck2_execute::directory::INTERNER;
use buck2_execute::directory::re_directory_to_re_tree;
use buck2_execute::directory::re_tree_to_directory;
use buck2_execute::execute::command_executor::ActionExecutionTimingData;
use buck2_execute::materialize::materializer::CasDownloadInfo;
use buck2_execute::materialize::materializer::DeclareArtifactPayload;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_hash::BuckIndexSet;
use chrono::TimeZone;
use chrono::Utc;
use dupe::Dupe;
use gazebo::prelude::*;
use pagable::Pagable;
use pagable::pagable_typetag;
use remote_execution as RE;
use starlark::values::OwnedFrozenValue;

use crate::actions::impls::store_import::ExpectedStoreManifest;
use crate::actions::impls::store_import::build_verified_store_entry;
use crate::actions::impls::store_import::has_normalized_sealed_store_metadata;
use crate::actions::impls::store_import::verify_cas_store_manifest;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum CasStoreImportActionError {
    #[error("CAS store import action should have exactly 1 output, got {0}")]
    WrongNumberOfOutputs(usize),
    #[error("CAS store import output does not carry a logical store path: `{0}`")]
    MissingLogicalStorePath(String),
    #[error("CAS store import manifest input must resolve to exactly one artifact")]
    ManifestInputNotSingleton,
}

/// Imports a published BuckPkgs store object whose payload is retained in REAPI CAS.
#[derive(Debug, Allocative, Pagable)]
pub(crate) struct UnregisteredCasStoreImportAction {
    pub(crate) manifest: ArtifactGroup,
    pub(crate) expected_manifest: ExpectedStoreManifest,
    pub(crate) root_digest: FileDigest,
    pub(crate) re_use_case: RemoteExecutorUseCase,
}

impl UnregisteredAction for UnregisteredCasStoreImportAction {
    fn register(
        self: Box<Self>,
        outputs: BuckIndexSet<BuildArtifact>,
        _starlark_data: Option<OwnedFrozenValue>,
        _error_handler: Option<OwnedFrozenValue>,
    ) -> buck2_error::Result<Box<dyn Action>> {
        Ok(Box::new(CasStoreImportAction::new(outputs, *self)?))
    }
}

#[derive(Debug, Allocative, Pagable)]
struct CasStoreImportAction {
    output: BuildArtifact,
    inner: UnregisteredCasStoreImportAction,
}

impl CasStoreImportAction {
    fn new(
        outputs: BuckIndexSet<BuildArtifact>,
        inner: UnregisteredCasStoreImportAction,
    ) -> buck2_error::Result<Self> {
        let outputs_len = outputs.len();
        let mut outputs = outputs.into_iter();
        let output = match (outputs.next(), outputs.next()) {
            (Some(output), None) => output,
            _ => return Err(CasStoreImportActionError::WrongNumberOfOutputs(outputs_len).into()),
        };
        if output.get_path().logical_store_path().is_none() {
            return Err(CasStoreImportActionError::MissingLogicalStorePath(
                output.get_path().to_string(),
            )
            .into());
        }
        Ok(Self { output, inner })
    }
}

#[pagable_typetag]
#[async_trait]
impl Action for CasStoreImportAction {
    fn kind(&self) -> buck2_data::ActionKind {
        buck2_data::ActionKind::StoreImport
    }

    fn inputs(&self) -> buck2_error::Result<Cow<'_, [ArtifactGroup]>> {
        Ok(Cow::Borrowed(slice::from_ref(&self.inner.manifest)))
    }

    fn outputs(&self) -> Cow<'_, [BuildArtifact]> {
        Cow::Borrowed(slice::from_ref(&self.output))
    }

    fn first_output(&self) -> &BuildArtifact {
        &self.output
    }

    fn category(&self) -> CategoryRef<'_> {
        CategoryRef::unchecked_new("cas_store_import")
    }

    fn identifier(&self) -> Option<&str> {
        Some(self.output.get_path().path().as_str())
    }

    async fn execute(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        waiting_data: WaitingData,
    ) -> Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError> {
        let logical_store_path = self
            .output
            .get_path()
            .logical_store_path()
            .ok_or_else(|| {
                buck2_error::Error::from(CasStoreImportActionError::MissingLogicalStorePath(
                    self.output.get_path().to_string(),
                ))
            })?
            .to_owned();
        let store_path = AbsNormPathBuf::try_from(logical_store_path.clone())?;
        let (manifest, manifest_value) = ctx
            .artifact_values(&self.inner.manifest)
            .iter()
            .into_singleton()
            .ok_or_else(|| {
                buck2_error::Error::from(CasStoreImportActionError::ManifestInputNotSingleton)
            })?;
        let manifest_path = manifest.resolve_path(
            ctx.fs(),
            if manifest.path_resolution_requires_artifact_value() {
                Some(manifest_value.content_based_path_hash())
            } else {
                None
            }
            .as_ref(),
        )?;
        let manifest_path = ctx.fs().fs().resolve(manifest_path);
        verify_cas_store_manifest(
            &manifest_path,
            &logical_store_path,
            &self.inner.expected_manifest,
            &self.inner.root_digest.to_string(),
        )?;

        let store_path_for_verification = store_path.clone();
        let expected_hash = self.inner.expected_manifest.canonical_tree_hash.clone();
        let cas_digest_config = ctx.digest_config().cas_digest_config();
        if let Some(entry) = tokio::task::spawn_blocking(move || -> buck2_error::Result<_> {
            let entry = build_verified_store_entry(
                &store_path_for_verification,
                &expected_hash,
                cas_digest_config,
            )?;
            if entry.is_some()
                && has_normalized_sealed_store_metadata(&store_path_for_verification)?
            {
                Ok(entry)
            } else {
                Ok(None)
            }
        })
        .await
        .buck_error_context("waiting for existing CAS store verification")??
        {
            let value = ArtifactValue::new(
                entry.map_dir(|dir| {
                    dir.fingerprint(ctx.digest_config().as_directory_serializer())
                        .shared(&*INTERNER)
                }),
                None,
            );
            let staged_path = ctx.fs().resolve_build(
                self.output.get_path(),
                Some(&value.content_based_path_hash()),
            )?;
            ctx.materializer()
                .declare_imported_store(DeclareArtifactPayload {
                    path: staged_path,
                    artifact: value.dupe(),
                    configuration_path: None,
                    logical_store_path: Some(logical_store_path),
                })
                .await?;
            return Ok((
                ActionOutputs::from_single(self.output.get_path().dupe(), value),
                ActionExecutionMetadata {
                    execution_kind: ActionExecutionKind::Simple,
                    timing: ActionExecutionTimingData::default(),
                    input_files_bytes: None,
                    waiting_data,
                },
            ));
        }

        let re_client = ctx.re_client().with_use_case(self.inner.re_use_case);
        let root_directory = re_client
            .download_typed_blobs::<RE::Directory>(None, vec![self.inner.root_digest.to_re()])
            .await
            .and_then(|directories| {
                directories
                    .into_iter()
                    .next()
                    .ok_or_else(|| internal_error!("RE response was empty"))
            })
            .with_buck_error_context(|| {
                format!(
                    "Error downloading CAS store root directory: {}",
                    self.inner.root_digest
                )
            })?;
        let tree = re_directory_to_re_tree(root_directory, &re_client).await?;
        let dir = re_tree_to_directory(
            &tree,
            &Utc.timestamp_opt(0, 0).unwrap(),
            ctx.digest_config(),
            ctx.output_trees_download_config()
                .fingerprint_re_output_trees_eagerly(),
        )
        .buck_error_context("Invalid CAS store directory")?;
        let value = ArtifactValue::new(
            ActionDirectoryEntry::Dir(
                dir.fingerprint(ctx.digest_config().as_directory_serializer())
                    .shared(&*INTERNER),
            ),
            None,
        );
        let staged_path = ctx.fs().resolve_build(
            self.output.get_path(),
            Some(&value.content_based_path_hash()),
        )?;
        ctx.materializer()
            .declare_cas_many(
                Arc::new(CasDownloadInfo::new_declared(self.inner.re_use_case)),
                vec![DeclareArtifactPayload {
                    path: staged_path,
                    artifact: value.dupe(),
                    configuration_path: None,
                    logical_store_path: Some(logical_store_path),
                }],
            )
            .await?;

        Ok((
            ActionOutputs::from_single(self.output.get_path().dupe(), value),
            ActionExecutionMetadata {
                execution_kind: ActionExecutionKind::Deferred,
                timing: ActionExecutionTimingData::default(),
                input_files_bytes: None,
                waiting_data,
            },
        ))
    }
}
