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
use std::io::Read;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::slice;
use std::time::Duration;
use std::time::UNIX_EPOCH;

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
use buck2_common::file_ops::metadata::FileMetadata;
use buck2_common::file_ops::metadata::TrackedFileDigest;
use buck2_core::category::CategoryRef;
use buck2_directory::directory::entry::DirectoryEntry;
use buck2_error::BuckErrorContext;
use buck2_execute::artifact::artifact_dyn::ArtifactDyn;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::directory::ActionDirectoryBuilder;
use buck2_execute::directory::ActionDirectoryEntry;
use buck2_execute::directory::ActionDirectoryMember;
use buck2_execute::directory::INTERNER;
use buck2_execute::directory::new_symlink;
use buck2_execute::execute::command_executor::ActionExecutionTimingData;
use buck2_execute::materialize::materializer::DeclareArtifactPayload;
use buck2_fs::error::IoResultExt;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_fs::paths::file_name::FileNameBuf;
use buck2_hash::BuckIndexSet;
use dupe::Dupe;
use gazebo::prelude::*;
use pagable::Pagable;
use pagable::pagable_typetag;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use starlark::values::OwnedFrozenValue;

const CANONICAL_TREE_HASH_PREFIX: &str = "sha256:";
const ARCHIVE_MAGIC: &[u8] = b"BUCKPKGS-STORE-ARCHIVE-V1\0";
const RECORD_END: u8 = 0;
const RECORD_DIRECTORY: u8 = 1;
const RECORD_FILE: u8 = 2;
const RECORD_SYMLINK: u8 = 3;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum StoreImportActionError {
    #[error("store import action should have exactly 1 output, got {0}")]
    WrongNumberOfOutputs(usize),
    #[error("store import output does not carry a logical store path: `{0}`")]
    MissingLogicalStorePath(String),
    #[error("canonical store tree hash must be a `sha256:` digest: `{0}`")]
    InvalidCanonicalTreeHash(String),
    #[error("store import manifest input must resolve to exactly one artifact")]
    ManifestInputNotSingleton,
    #[error("failed to parse store manifest `{path}`: {error}")]
    InvalidManifest { path: String, error: String },
    #[error("store manifest metadata field `{field}` does not match its import declaration")]
    ManifestMetadataMismatch { field: &'static str },
    #[error("canonical store tree path is too long for the archive format: `{0}`")]
    CanonicalTreePathTooLong(String),
    #[error("hydrated store object contains unsupported filesystem entry: `{0}`")]
    UnsupportedStoreEntry(String),
    #[error(
        "hydrated store object `{store_path}` does not match canonical tree hash: expected {expected}, got {actual}"
    )]
    CanonicalTreeHashMismatch {
        store_path: String,
        expected: String,
        actual: String,
    },
}

#[derive(Clone, Debug, Allocative, Pagable)]
pub(crate) struct ExpectedStoreManifest {
    pub(crate) package_name: String,
    pub(crate) version: String,
    pub(crate) output: String,
    pub(crate) target_system: String,
    pub(crate) references: Vec<String>,
    pub(crate) runtime_store_outputs: Vec<String>,
    pub(crate) canonical_tree_hash: String,
}

/// Imports an immutable object that was already hydrated at its logical store path.
#[derive(Debug, Allocative, Pagable)]
pub(crate) struct UnregisteredStoreImportAction {
    pub(crate) missing_hint: Option<String>,
    pub(crate) manifest: ArtifactGroup,
    pub(crate) expected_manifest: ExpectedStoreManifest,
}

impl UnregisteredAction for UnregisteredStoreImportAction {
    fn register(
        self: Box<Self>,
        outputs: BuckIndexSet<BuildArtifact>,
        _starlark_data: Option<OwnedFrozenValue>,
        _error_handler: Option<OwnedFrozenValue>,
    ) -> buck2_error::Result<Box<dyn Action>> {
        Ok(Box::new(StoreImportAction::new(
            outputs,
            self.missing_hint,
            self.manifest,
            self.expected_manifest,
        )?))
    }
}

#[derive(Debug, Allocative, Pagable)]
struct StoreImportAction {
    output: BuildArtifact,
    missing_hint: Option<String>,
    manifest: ArtifactGroup,
    expected_manifest: ExpectedStoreManifest,
}

impl StoreImportAction {
    fn new(
        outputs: BuckIndexSet<BuildArtifact>,
        missing_hint: Option<String>,
        manifest: ArtifactGroup,
        expected_manifest: ExpectedStoreManifest,
    ) -> buck2_error::Result<Self> {
        let outputs_len = outputs.len();
        let mut outputs = outputs.into_iter();
        let output = match (outputs.next(), outputs.next()) {
            (Some(output), None) => output,
            _ => return Err(StoreImportActionError::WrongNumberOfOutputs(outputs_len).into()),
        };
        if output.get_path().logical_store_path().is_none() {
            return Err(StoreImportActionError::MissingLogicalStorePath(
                output.get_path().to_string(),
            )
            .into());
        }
        Ok(Self {
            output,
            missing_hint,
            manifest,
            expected_manifest,
        })
    }
}

#[pagable_typetag]
#[async_trait]
impl Action for StoreImportAction {
    fn kind(&self) -> buck2_data::ActionKind {
        buck2_data::ActionKind::StoreImport
    }

    fn inputs(&self) -> buck2_error::Result<Cow<'_, [ArtifactGroup]>> {
        Ok(Cow::Borrowed(slice::from_ref(&self.manifest)))
    }

    fn outputs(&self) -> Cow<'_, [BuildArtifact]> {
        Cow::Borrowed(slice::from_ref(&self.output))
    }

    fn first_output(&self) -> &BuildArtifact {
        &self.output
    }

    fn category(&self) -> CategoryRef<'_> {
        CategoryRef::unchecked_new("store_import")
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
                buck2_error::Error::from(StoreImportActionError::MissingLogicalStorePath(
                    self.output.get_path().to_string(),
                ))
            })?
            .to_owned();
        let store_path = AbsNormPathBuf::try_from(logical_store_path.clone())?;
        let (manifest, manifest_value) = ctx
            .artifact_values(&self.manifest)
            .iter()
            .into_singleton()
            .ok_or_else(|| {
                buck2_error::Error::from(StoreImportActionError::ManifestInputNotSingleton)
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
        let store_path_for_hashing = store_path.clone();
        let logical_store_path_for_verification = logical_store_path.clone();
        let expected_manifest = self.expected_manifest.clone();
        let cas_digest_config = ctx.digest_config().cas_digest_config();
        let entry = tokio::task::spawn_blocking(move || {
            verify_store_manifest(
                &manifest_path,
                &logical_store_path_for_verification,
                &expected_manifest,
            )?;
            build_verified_store_entry(
                &store_path_for_hashing,
                &expected_manifest.canonical_tree_hash,
                cas_digest_config,
            )
        })
        .await
        .buck_error_context("waiting for hydrated store verification")??;
        let entry = entry.ok_or_else(|| {
            let hint = self
                .missing_hint
                .as_deref()
                .map(|hint| format!(" {hint}"))
                .unwrap_or_default();
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Hydrated store object `{}` is absent.{}",
                store_path.display(),
                hint,
            )
        })?;
        let entry = entry.map_dir(|dir| {
            dir.fingerprint(ctx.digest_config().as_directory_serializer())
                .shared(&*INTERNER)
        });
        let value = ArtifactValue::new(entry, None);
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

        Ok((
            ActionOutputs::from_single(self.output.get_path().dupe(), value),
            ActionExecutionMetadata {
                execution_kind: ActionExecutionKind::Simple,
                timing: ActionExecutionTimingData::default(),
                input_files_bytes: None,
                waiting_data,
            },
        ))
    }
}

pub(crate) fn build_verified_store_entry(
    store_path: &AbsNormPathBuf,
    expected_hash: &str,
    cas_digest_config: buck2_common::cas_digest::CasDigestConfig,
) -> buck2_error::Result<Option<ActionDirectoryEntry<ActionDirectoryBuilder>>> {
    if !is_sha256_hash(expected_hash) {
        return Err(
            StoreImportActionError::InvalidCanonicalTreeHash(expected_hash.to_owned()).into(),
        );
    }
    if fs_util::symlink_metadata_if_exists(store_path)?.is_none() {
        return Ok(None);
    }

    let mut canonical = Sha256::new();
    canonical.update(ARCHIVE_MAGIC);
    let entry = build_verified_store_entry_inner(
        store_path,
        Path::new(""),
        cas_digest_config,
        &mut canonical,
    )?;
    canonical.update([RECORD_END]);
    let actual_hash = format!("{CANONICAL_TREE_HASH_PREFIX}{:x}", canonical.finalize());
    if actual_hash != expected_hash {
        return Err(StoreImportActionError::CanonicalTreeHashMismatch {
            store_path: store_path.to_string(),
            expected: expected_hash.to_owned(),
            actual: actual_hash,
        }
        .into());
    }
    Ok(Some(entry))
}

pub(crate) fn has_normalized_sealed_store_metadata(
    store_path: &AbsNormPathBuf,
) -> buck2_error::Result<bool> {
    if fs_util::symlink_metadata_if_exists(store_path)?.is_none() {
        return Ok(false);
    }
    has_normalized_sealed_store_metadata_inner(store_path)
}

fn has_normalized_sealed_store_metadata_inner(path: &AbsNormPathBuf) -> buck2_error::Result<bool> {
    let metadata = fs_util::symlink_metadata(path)
        .categorize_input()
        .buck_error_context(format!(
            "inspecting sealed store entry `{}`",
            path.display()
        ))?;
    if metadata.modified()? != UNIX_EPOCH + Duration::from_secs(1) {
        return Ok(false);
    }
    if metadata.file_type().is_symlink() {
        return Ok(true);
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o222 != 0 {
        return Ok(false);
    }
    #[cfg(not(unix))]
    if !metadata.permissions().readonly() {
        return Ok(false);
    }
    if metadata.is_dir() {
        for entry in fs_util::read_dir(path)
            .categorize_input()
            .buck_error_context(format!(
                "reading sealed store directory `{}`",
                path.display()
            ))?
        {
            if !has_normalized_sealed_store_metadata_inner(&entry?.path())? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

pub(crate) fn verify_store_manifest(
    manifest_path: &AbsNormPathBuf,
    logical_store_path: &str,
    expected: &ExpectedStoreManifest,
) -> buck2_error::Result<Value> {
    let contents = fs_util::read_to_string(manifest_path)
        .categorize_input()
        .buck_error_context(format!(
            "reading store manifest `{}`",
            manifest_path.display()
        ))?;
    let manifest: Value = serde_json::from_str(&contents).map_err(|error| {
        StoreImportActionError::InvalidManifest {
            path: manifest_path.to_string(),
            error: error.to_string(),
        }
    })?;

    verify_manifest_string(&manifest, &["format"], "buckpkgs-store-object-v1", "format")?;
    verify_manifest_string(&manifest, &["store_path"], logical_store_path, "store_path")?;
    verify_manifest_string(
        &manifest,
        &["package", "name"],
        &expected.package_name,
        "package.name",
    )?;
    verify_manifest_string(
        &manifest,
        &["package", "version"],
        &expected.version,
        "package.version",
    )?;
    verify_manifest_string(
        &manifest,
        &["package", "output"],
        &expected.output,
        "package.output",
    )?;
    verify_manifest_string(
        &manifest,
        &["target_system"],
        &expected.target_system,
        "target_system",
    )?;
    verify_manifest_strings(
        &manifest,
        &["references"],
        &expected.references,
        "references",
    )?;
    verify_manifest_strings(
        &manifest,
        &["runtime_store_outputs"],
        &expected.runtime_store_outputs,
        "runtime_store_outputs",
    )?;
    verify_manifest_string(
        &manifest,
        &["canonical_tree_hash"],
        &expected.canonical_tree_hash,
        "canonical_tree_hash",
    )?;
    Ok(manifest)
}

pub(crate) fn verify_cas_store_manifest(
    manifest_path: &AbsNormPathBuf,
    logical_store_path: &str,
    expected: &ExpectedStoreManifest,
    root_digest: &str,
) -> buck2_error::Result<()> {
    let manifest = verify_store_manifest(manifest_path, logical_store_path, expected)?;
    verify_manifest_string(
        &manifest,
        &["cas", "format"],
        "reapi-directory-v1",
        "cas.format",
    )?;
    verify_manifest_string(
        &manifest,
        &["cas", "digest_function"],
        "sha256",
        "cas.digest_function",
    )?;
    verify_manifest_string(
        &manifest,
        &["cas", "root_digest"],
        root_digest,
        "cas.root_digest",
    )
}

fn verify_manifest_string(
    manifest: &Value,
    path: &[&str],
    expected: &str,
    field: &'static str,
) -> buck2_error::Result<()> {
    if manifest_field(manifest, path).and_then(Value::as_str) != Some(expected) {
        return Err(StoreImportActionError::ManifestMetadataMismatch { field }.into());
    }
    Ok(())
}

fn verify_manifest_strings(
    manifest: &Value,
    path: &[&str],
    expected: &[String],
    field: &'static str,
) -> buck2_error::Result<()> {
    let Some(actual) = manifest_field(manifest, path).and_then(Value::as_array) else {
        return Err(StoreImportActionError::ManifestMetadataMismatch { field }.into());
    };
    if actual.len() != expected.len()
        || actual
            .iter()
            .zip(expected)
            .any(|(actual, expected)| actual.as_str() != Some(expected))
    {
        return Err(StoreImportActionError::ManifestMetadataMismatch { field }.into());
    }
    Ok(())
}

fn manifest_field<'a>(manifest: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(manifest, |value, key| value.as_object()?.get(*key))
}

fn build_verified_store_entry_inner(
    path: &AbsNormPathBuf,
    relative: &Path,
    cas_digest_config: buck2_common::cas_digest::CasDigestConfig,
    canonical: &mut Sha256,
) -> buck2_error::Result<ActionDirectoryEntry<ActionDirectoryBuilder>> {
    let metadata = fs_util::symlink_metadata(path)
        .categorize_input()
        .buck_error_context(format!(
            "inspecting hydrated store entry `{}`",
            path.display()
        ))?;
    if metadata.is_dir() {
        write_path_record(canonical, RECORD_DIRECTORY, relative)?;
        let mut entries = fs_util::read_dir(path)
            .categorize_input()
            .buck_error_context(format!(
                "reading hydrated store directory `{}`",
                path.display()
            ))?
            .collect::<Result<Vec<_>, _>>()
            .buck_error_context(format!(
                "reading hydrated store directory `{}`",
                path.display()
            ))?;
        entries.sort_by_key(|entry| entry.file_name());
        let mut builder = ActionDirectoryBuilder::empty();
        for entry in entries {
            let name = entry.file_name();
            let name_string = name
                .to_str()
                .ok_or_else(|| {
                    buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "hydrated store object contains a non-UTF-8 entry below `{}`",
                        path.display()
                    )
                })?
                .to_owned();
            let name = FileNameBuf::try_from(name_string.clone())?;
            let child_path = entry.path();
            let child_relative = relative.join(name_string);
            let child = build_verified_store_entry_inner(
                &child_path,
                &child_relative,
                cas_digest_config,
                canonical,
            )?;
            builder.insert(name, child)?;
        }
        return Ok(DirectoryEntry::Dir(builder));
    }
    if metadata.file_type().is_symlink() {
        write_path_record(canonical, RECORD_SYMLINK, relative)?;
        let target = fs_util::read_link(path)
            .categorize_input()
            .buck_error_context(format!(
                "reading hydrated store symlink `{}`",
                path.display()
            ))?;
        write_bytes(canonical, os_bytes(target.as_path())?)?;
        return Ok(DirectoryEntry::Leaf(new_symlink(target)?));
    }
    if metadata.is_file() {
        write_path_record(canonical, RECORD_FILE, relative)?;
        canonical.update([is_executable(&metadata) as u8]);
        canonical.update(metadata.len().to_be_bytes());
        let mut file = fs_util::open_file(path)
            .categorize_input()
            .buck_error_context(format!("reading hydrated store file `{}`", path.display()))?;
        let mut buck_digest = FileDigest::digester(cas_digest_config);
        let mut buffer = [0; 16 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .buck_error_context(format!("reading hydrated store file `{}`", path.display()))?;
            if count == 0 {
                break;
            }
            buck_digest.update(&buffer[..count]);
            canonical.update(&buffer[..count]);
        }
        let digest = buck_digest.finalize();
        if digest.size() != metadata.len() {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "hydrated store file `{}` changed while it was imported",
                path.display()
            ));
        }
        return Ok(DirectoryEntry::Leaf(ActionDirectoryMember::File(
            FileMetadata {
                digest: TrackedFileDigest::new(digest, cas_digest_config),
                is_executable: is_executable(&metadata),
            },
        )));
    }

    Err(StoreImportActionError::UnsupportedStoreEntry(path.to_string()).into())
}

fn is_sha256_hash(value: &str) -> bool {
    value
        .strip_prefix(CANONICAL_TREE_HASH_PREFIX)
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn write_path_record(canonical: &mut Sha256, kind: u8, path: &Path) -> buck2_error::Result<()> {
    canonical.update([kind]);
    write_bytes(canonical, os_bytes(path)?)
}

fn write_bytes(canonical: &mut Sha256, bytes: &[u8]) -> buck2_error::Result<()> {
    let length: u32 = bytes.len().try_into().map_err(|_| {
        StoreImportActionError::CanonicalTreePathTooLong(
            String::from_utf8_lossy(bytes).into_owned(),
        )
    })?;
    canonical.update(length.to_be_bytes());
    canonical.update(bytes);
    Ok(())
}

#[cfg(unix)]
fn os_bytes(path: &Path) -> buck2_error::Result<&[u8]> {
    Ok(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn os_bytes(path: &Path) -> buck2_error::Result<&[u8]> {
    path.to_str().map(str::as_bytes).ok_or_else(|| {
        buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "hydrated store path is not UTF-8: `{}`",
            path.display()
        )
    })
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::fs::FileTimes;

    use buck2_common::cas_digest::CasDigestConfig;
    use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
    use tempfile::tempdir;

    use super::*;

    const PAYLOAD_HASH: &str =
        "sha256:d3fdd16043fcc20f4d79ac637049d63911db7c0c431e8ad663a3b7e4b55c0672";

    #[test]
    fn store_manifest_rejects_declared_provider_mismatch() -> buck2_error::Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("manifest.json");
        std::fs::write(
            &path,
            format!(
                r#"{{
                    "format": "buckpkgs-store-object-v1",
                    "store_path": "/pkgs/store/object",
                    "package": {{ "name": "tool", "version": "1", "output": "out" }},
                    "target_system": "x86_64-linux",
                    "references": [],
                    "runtime_store_outputs": ["/pkgs/store/object"],
                    "canonical_tree_hash": "{PAYLOAD_HASH}"
                }}"#
            ),
        )?;
        let path = AbsNormPathBuf::try_from(path)?;
        let mut expected = ExpectedStoreManifest {
            package_name: "tool".to_owned(),
            version: "1".to_owned(),
            output: "out".to_owned(),
            target_system: "x86_64-linux".to_owned(),
            references: Vec::new(),
            runtime_store_outputs: vec!["/pkgs/store/object".to_owned()],
            canonical_tree_hash: PAYLOAD_HASH.to_owned(),
        };

        verify_store_manifest(&path, "/pkgs/store/object", &expected)?;
        expected.package_name = "different".to_owned();
        let error = verify_store_manifest(&path, "/pkgs/store/object", &expected).unwrap_err();
        assert!(error.to_string().contains("package.name"));
        Ok(())
    }

    #[test]
    fn cas_store_manifest_binds_the_published_root_digest() -> buck2_error::Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("manifest.json");
        std::fs::write(
            &path,
            format!(
                r#"{{
                    "format": "buckpkgs-store-object-v1",
                    "store_path": "/pkgs/store/object",
                    "package": {{ "name": "tool", "version": "1", "output": "out" }},
                    "target_system": "x86_64-linux",
                    "references": [],
                    "runtime_store_outputs": ["/pkgs/store/object"],
                    "canonical_tree_hash": "{PAYLOAD_HASH}",
                    "cas": {{
                        "format": "reapi-directory-v1",
                        "digest_function": "sha256",
                        "root_digest": "abcdef:7"
                    }}
                }}"#
            ),
        )?;
        let path = AbsNormPathBuf::try_from(path)?;
        let expected = ExpectedStoreManifest {
            package_name: "tool".to_owned(),
            version: "1".to_owned(),
            output: "out".to_owned(),
            target_system: "x86_64-linux".to_owned(),
            references: Vec::new(),
            runtime_store_outputs: vec!["/pkgs/store/object".to_owned()],
            canonical_tree_hash: PAYLOAD_HASH.to_owned(),
        };

        verify_cas_store_manifest(&path, "/pkgs/store/object", &expected, "abcdef:7")?;
        let error =
            verify_cas_store_manifest(&path, "/pkgs/store/object", &expected, "different:7")
                .unwrap_err();
        assert!(error.to_string().contains("cas.root_digest"));
        Ok(())
    }

    #[test]
    fn normalized_sealed_store_metadata_rejects_legacy_writable_tree() -> buck2_error::Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("store");
        std::fs::create_dir(&path)?;
        std::fs::write(path.join("tool"), b"tool")?;
        let path = AbsNormPathBuf::try_from(path)?;
        assert!(!has_normalized_sealed_store_metadata(&path)?);

        let modified = UNIX_EPOCH + Duration::from_secs(1);
        for entry in [path.join(ForwardRelativePath::new("tool")?), path.clone()] {
            std::fs::File::open(entry.as_maybe_relativized())?
                .set_times(FileTimes::new().set_modified(modified))?;
            let mut permissions = std::fs::metadata(entry.as_maybe_relativized())?.permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(entry.as_maybe_relativized(), permissions)?;
        }

        assert!(has_normalized_sealed_store_metadata(&path)?);
        Ok(())
    }

    #[test]
    fn verified_store_entry_rejects_modified_payload() -> buck2_error::Result<()> {
        let temp = tempdir()?;
        let root = temp.path().join("store-object");
        std::fs::create_dir(&root)?;
        std::fs::write(root.join("payload"), b"payload\n")?;
        let root = AbsNormPathBuf::try_from(root)?;

        assert!(
            build_verified_store_entry(&root, PAYLOAD_HASH, CasDigestConfig::testing_default())?
                .is_some()
        );

        std::fs::write(root.as_path().join("payload"), b"changed\n")?;
        let error =
            build_verified_store_entry(&root, PAYLOAD_HASH, CasDigestConfig::testing_default())
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match canonical tree hash")
        );
        Ok(())
    }

    #[test]
    fn verified_store_entry_preserves_missing_object_result() -> buck2_error::Result<()> {
        let temp = tempdir()?;
        let missing = AbsNormPathBuf::try_from(temp.path().join("missing"))?;

        assert!(
            build_verified_store_entry(&missing, PAYLOAD_HASH, CasDigestConfig::testing_default())?
                .is_none()
        );
        Ok(())
    }
}
