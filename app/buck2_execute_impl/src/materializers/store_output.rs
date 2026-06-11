/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fs::FileTimes;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use buck2_common::file_ops::metadata::FileDigestConfig;
use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_directory::directory::entry::DirectoryEntry;
use buck2_error::ErrorTag;
use buck2_error::buck2_error;
use buck2_execute::artifact_value::ArtifactValue;
use buck2_execute::digest_config::DigestConfig;
use buck2_execute::directory::ActionDirectoryBuilder;
use buck2_execute::directory::ActionDirectoryEntry;
use buck2_execute::directory::ActionDirectoryMember;
use buck2_execute::directory::ActionDirectoryRef;
use buck2_execute::directory::ActionSharedDirectory;
use buck2_execute::directory::extract_artifact_value;
use buck2_execute::directory::insert_entry;
use buck2_execute::entry::build_entry_from_disk;
use buck2_execute::execute::blocking::BlockingExecutor;
use buck2_execute::execute::blocking::IoRequest;
use buck2_fs::error::IoResultExt;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_norm_path::AbsNormPath;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use dice_futures::cancellation::CancellationContext;
use dupe::Dupe;

use crate::materializers::io::materialize_dirs_and_syms;
use crate::materializers::io::materialize_files;

pub struct MaterializeStoreOutput {
    pub staged_path: ProjectRelativePathBuf,
    pub store_path: AbsNormPathBuf,
    pub entry: ActionDirectoryEntry<ActionSharedDirectory>,
    /// REAPI directories preserve contents and executable bits, but not the
    /// sealed modes or normalized mtimes required by BuckPkgs store objects.
    pub seal_cas_transport: bool,
}

impl IoRequest for MaterializeStoreOutput {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> buck2_error::Result<()> {
        let lock_path = AbsNormPathBuf::from(format!("{}.buck2.lock", self.store_path))?;
        if let Some(parent) = lock_path.parent() {
            fs_util::create_dir_all(parent)?;
        }
        let lock_file = std::fs::File::create(lock_path.as_abs_path().as_maybe_relativized())?;
        fs4::fs_std::FileExt::lock_exclusive(&lock_file)?;

        if fs_util::try_exists(&self.store_path)? {
            if self.seal_cas_transport {
                normalize_cas_store_output_metadata(
                    self.entry.as_ref(),
                    &mut self.store_path.clone(),
                )?;
            }
            return Ok(());
        }

        let staged_path = project_fs.root().join(&self.staged_path);
        let temp_store_path = AbsNormPathBuf::from(format!("{}.buck2.tmp", self.store_path))?;
        remove_store_temp_tree(&temp_store_path)?;

        let materialize_result = (|| -> buck2_error::Result<()> {
            materialize_dirs_and_syms(self.entry.as_ref(), &temp_store_path)?;
            materialize_files(self.entry.as_ref(), &staged_path, &temp_store_path, None)?;
            if self.seal_cas_transport {
                normalize_cas_store_output_metadata(
                    self.entry.as_ref(),
                    &mut temp_store_path.clone(),
                )?;
            } else {
                preserve_store_output_metadata(
                    self.entry.as_ref(),
                    &mut staged_path.clone(),
                    &mut temp_store_path.clone(),
                )?;
            }
            Ok(())
        })();

        if let Err(err) = materialize_result {
            let _ = remove_store_temp_tree(&temp_store_path);
            return Err(err);
        }

        if let Err(err) = fs_util::rename(&temp_store_path, &self.store_path).categorize_internal()
        {
            let _ = remove_store_temp_tree(&temp_store_path);
            return Err(err);
        }

        Ok(())
    }
}

pub(crate) async fn materialize_store_output(
    io_executor: &dyn BlockingExecutor,
    project_fs: &ProjectRoot,
    digest_config: DigestConfig,
    staged_path: ProjectRelativePathBuf,
    store_path: &str,
    artifact: ArtifactValue,
    seal_cas_transport: bool,
) -> buck2_error::Result<()> {
    let store_path = AbsNormPathBuf::try_from(store_path.to_owned())?;
    if fs_util::try_exists(&store_path)? {
        verify_store_output(
            io_executor,
            project_fs,
            digest_config,
            &store_path,
            &artifact,
        )
        .await?;
        if !seal_cas_transport {
            return Ok(());
        }
    }

    io_executor
        .execute_io(
            Box::new(MaterializeStoreOutput {
                staged_path,
                store_path: store_path.clone(),
                entry: artifact.entry().dupe(),
                seal_cas_transport,
            }),
            CancellationContext::never_cancelled(),
        )
        .await?;

    verify_store_output(
        io_executor,
        project_fs,
        digest_config,
        &store_path,
        &artifact,
    )
    .await
}

async fn verify_store_output(
    io_executor: &dyn BlockingExecutor,
    project_fs: &ProjectRoot,
    digest_config: DigestConfig,
    store_path: &AbsNormPathBuf,
    artifact: &ArtifactValue,
) -> buck2_error::Result<()> {
    let (entry, _) = build_entry_from_disk(
        store_path.clone(),
        FileDigestConfig::build(digest_config.cas_digest_config()),
        io_executor,
        project_fs.root(),
        false,
    )
    .await?;
    let entry = entry.ok_or_else(|| {
        buck2_error!(
            ErrorTag::MaterializationError,
            "Store output disappeared while verifying `{}`",
            store_path.display()
        )
    })?;
    let verification_path =
        ProjectRelativePathBuf::unchecked_new("__buckpkgs_store_verify__".to_owned());
    let mut builder = ActionDirectoryBuilder::empty();
    insert_entry(&mut builder, verification_path.clone(), entry)?;
    let existing = extract_artifact_value(&builder, &verification_path, digest_config)?
        .ok_or_else(|| {
            buck2_error!(
                ErrorTag::MaterializationError,
                "Unable to verify existing store output `{}`",
                store_path.display()
            )
        })?;
    if existing.entry() != artifact.entry() {
        return Err(buck2_error!(
            ErrorTag::MaterializationError,
            "Existing store output `{}` does not match the artifact Buck2 recorded",
            store_path.display()
        ));
    }

    Ok(())
}

fn normalize_cas_store_output_metadata<'a, D>(
    entry: DirectoryEntry<D, &ActionDirectoryMember>,
    dest: &mut AbsNormPathBuf,
) -> buck2_error::Result<()>
where
    D: ActionDirectoryRef<'a>,
{
    match entry {
        DirectoryEntry::Dir(d) => {
            for (name, entry) in d.entries() {
                dest.push(name);
                normalize_cas_store_output_metadata(entry, dest)?;
                dest.pop();
            }
            seal_cas_output_path(dest)
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::File(_)) => seal_cas_output_path(dest),
        DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(_))
        | DirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(_)) => {
            set_symlink_modified_time(dest, UNIX_EPOCH + Duration::from_secs(1))
        }
    }
}

fn seal_cas_output_path(dest: &AbsNormPath) -> buck2_error::Result<()> {
    let metadata = fs_util::symlink_metadata(dest).categorize_internal()?;
    let mut permissions = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        permissions.set_mode(permissions.mode() & !0o222);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(true);

    let file = std::fs::File::open(dest.as_maybe_relativized())?;
    file.set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1)))?;
    fs_util::set_permissions(dest, permissions).categorize_internal()?;
    Ok(())
}

fn preserve_store_output_metadata<'a, D>(
    entry: DirectoryEntry<D, &ActionDirectoryMember>,
    src: &mut AbsNormPathBuf,
    dest: &mut AbsNormPathBuf,
) -> buck2_error::Result<()>
where
    D: ActionDirectoryRef<'a>,
{
    match entry {
        DirectoryEntry::Dir(d) => {
            for (name, entry) in d.entries() {
                src.push(name);
                dest.push(name);
                preserve_store_output_metadata(entry, src, dest)?;
                src.pop();
                dest.pop();
            }
            preserve_native_output_metadata(src, dest, true)
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::File(_)) => {
            preserve_native_output_metadata(src, dest, false)
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(_))
        | DirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(_)) => {
            preserve_symlink_modified_time(src, dest)
        }
    }
}

fn preserve_native_output_metadata(
    src: &AbsNormPath,
    dest: &AbsNormPath,
    directory: bool,
) -> buck2_error::Result<()> {
    let metadata = fs_util::symlink_metadata(src).categorize_internal()?;
    let permissions = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if permissions.mode() & 0o222 != 0 {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Store output producer emitted writable path `{}`",
                src.display()
            ));
        }
    }
    #[cfg(not(unix))]
    if !permissions.readonly() {
        return Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "Store output producer emitted writable path `{}`",
            src.display()
        ));
    }

    let file = std::fs::File::open(dest.as_maybe_relativized())?;
    file.set_times(FileTimes::new().set_modified(metadata.modified()?))?;

    if directory {
        fs_util::set_permissions(dest, permissions).categorize_internal()?;
    }

    Ok(())
}

fn preserve_symlink_modified_time(
    src: &AbsNormPath,
    dest: &AbsNormPath,
) -> buck2_error::Result<()> {
    let modified = fs_util::symlink_metadata(src)
        .categorize_internal()?
        .modified()?;

    set_symlink_modified_time(dest, modified)
}

fn remove_store_temp_tree(path: &AbsNormPath) -> buck2_error::Result<()> {
    match fs_util::remove_all(path) {
        Ok(()) => Ok(()),
        #[cfg(unix)]
        Err(error) if error.io_error_kind() == Some(std::io::ErrorKind::PermissionDenied) => {
            make_directories_removable(path)?;
            fs_util::remove_all(path).categorize_internal()
        }
        Err(error) => Result::<(), fs_util::IoError>::Err(error).categorize_internal(),
    }
}

#[cfg(unix)]
fn make_directories_removable(path: &AbsNormPath) -> buck2_error::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs_util::symlink_metadata(path).categorize_internal()?;
    if !metadata.is_dir() {
        return Ok(());
    }

    let mut permissions = metadata.permissions();
    let mode = permissions.mode() | 0o700;
    if mode != permissions.mode() {
        permissions.set_mode(mode);
        fs_util::set_permissions(path, permissions).categorize_internal()?;
    }

    for entry in fs_util::read_dir(path).categorize_internal()? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            make_directories_removable(&AbsNormPathBuf::try_from(entry.path())?)?;
        }
    }

    Ok(())
}

#[cfg(unix)]
fn set_symlink_modified_time(dest: &AbsNormPath, modified: SystemTime) -> buck2_error::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::time::UNIX_EPOCH;

    let duration = modified.duration_since(UNIX_EPOCH)?;
    let timestamp = libc::timespec {
        tv_sec: duration.as_secs().try_into()?,
        tv_nsec: duration.subsec_nanos().into(),
    };
    let times = [timestamp, timestamp];
    let dest = CString::new(dest.as_maybe_relativized().as_os_str().as_bytes())?;
    let result = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            dest.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(not(unix))]
fn set_symlink_modified_time(
    _dest: &AbsNormPath,
    _modified: SystemTime,
) -> buck2_error::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::FileTimes;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(unix)]
    use std::sync::Arc;

    use buck2_common::file_ops::metadata::FileMetadata;
    #[cfg(unix)]
    use buck2_common::file_ops::metadata::Symlink;
    use buck2_core::fs::project::ProjectRootTemp;
    use buck2_execute::digest_config::DigestConfig;
    use buck2_execute::directory::ActionDirectoryBuilder;
    use buck2_execute::directory::INTERNER;
    use buck2_execute::directory::insert_file;
    use buck2_fs::fs_util;
    #[cfg(unix)]
    use buck2_fs::paths::RelativePathBuf;
    use buck2_fs::paths::forward_rel_path::ForwardRelativePath;

    use super::*;

    fn seal_for_store_test(path: &AbsNormPath) -> buck2_error::Result<()> {
        let metadata = fs_util::symlink_metadata(path).categorize_internal()?;
        let mut permissions = metadata.permissions();
        permissions.set_readonly(true);
        fs_util::set_permissions(path, permissions).categorize_internal()
    }

    #[test]
    fn materialize_store_output_copies_staged_files() -> buck2_error::Result<()> {
        let temp = ProjectRootTemp::new()?;
        let fs = temp.path().clone();
        let digest_config = DigestConfig::testing_default();
        let staged_path = ProjectRelativePathBuf::unchecked_new("buck-out/v2/pkg".to_owned());
        let staged_file = staged_path.join(ForwardRelativePath::new("bin/tool")?);
        fs.write_file(&staged_file, b"", false)?;
        let staged_bin = fs
            .root()
            .join(ForwardRelativePath::new("buck-out/v2/pkg/bin")?);
        let staged_tool = fs
            .root()
            .join(ForwardRelativePath::new("buck-out/v2/pkg/bin/tool")?);
        let modified = UNIX_EPOCH + Duration::from_secs(1);
        std::fs::File::open(staged_tool.as_maybe_relativized())?
            .set_times(FileTimes::new().set_modified(modified))?;
        std::fs::File::open(staged_bin.as_maybe_relativized())?
            .set_times(FileTimes::new().set_modified(modified))?;
        seal_for_store_test(staged_tool.as_ref())?;
        seal_for_store_test(staged_bin.as_ref())?;
        seal_for_store_test(
            fs.root()
                .join(ForwardRelativePath::new("buck-out/v2/pkg")?)
                .as_ref(),
        )?;

        let mut builder = ActionDirectoryBuilder::empty();
        insert_file(
            &mut builder,
            ProjectRelativePathBuf::unchecked_new("bin/tool".to_owned()),
            FileMetadata::empty(digest_config.cas_digest_config()),
        )?;
        let entry = ActionDirectoryEntry::Dir(
            builder
                .fingerprint(digest_config.as_directory_serializer())
                .shared(&*INTERNER),
        );
        let store_path = fs.root().join(ForwardRelativePath::new("store/pkg")?);

        Box::new(MaterializeStoreOutput {
            staged_path,
            store_path: store_path.clone(),
            entry,
            seal_cas_transport: false,
        })
        .execute(&fs)?;

        assert!(fs_util::try_exists(
            store_path.join(ForwardRelativePath::new("bin/tool")?)
        )?);
        assert!(fs_util::try_exists(AbsNormPathBuf::from(format!(
            "{}.buck2.lock",
            store_path
        ))?)?);
        assert!(!fs_util::try_exists(AbsNormPathBuf::from(format!(
            "{}.buck2.tmp",
            store_path
        ))?)?);
        assert_eq!(
            fs_util::symlink_metadata(store_path.join(ForwardRelativePath::new("bin/tool")?))
                .categorize_internal()?
                .modified()?,
            modified,
        );
        assert_eq!(
            fs_util::symlink_metadata(store_path.join(ForwardRelativePath::new("bin")?))
                .categorize_internal()?
                .modified()?,
            modified,
        );
        #[cfg(unix)]
        for path in [
            store_path.clone(),
            store_path.join(ForwardRelativePath::new("bin")?),
            store_path.join(ForwardRelativePath::new("bin/tool")?),
        ] {
            assert_eq!(
                fs_util::symlink_metadata(path)
                    .categorize_internal()?
                    .permissions()
                    .mode()
                    & 0o222,
                0,
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn materialize_store_output_preserves_symlink_modified_time() -> buck2_error::Result<()> {
        let temp = ProjectRootTemp::new()?;
        let fs = temp.path().clone();
        let digest_config = DigestConfig::testing_default();
        let staged_path = ProjectRelativePathBuf::unchecked_new("buck-out/v2/pkg".to_owned());
        let staged_target = fs
            .root()
            .join(ForwardRelativePath::new("buck-out/v2/pkg/bin/tool")?);
        let staged_link = fs
            .root()
            .join(ForwardRelativePath::new("buck-out/v2/pkg/bin/tool-link")?);
        fs_util::create_dir_all(
            staged_target
                .parent()
                .expect("tool path should have a parent"),
        )?;
        std::fs::write(staged_target.as_maybe_relativized(), b"tool")?;
        symlink("tool", staged_link.as_maybe_relativized())?;

        let modified = UNIX_EPOCH + Duration::from_secs(1);
        set_symlink_modified_time(staged_link.as_ref(), modified)?;
        seal_for_store_test(staged_target.as_ref())?;
        seal_for_store_test(
            fs.root()
                .join(ForwardRelativePath::new("buck-out/v2/pkg/bin")?)
                .as_ref(),
        )?;
        seal_for_store_test(
            fs.root()
                .join(ForwardRelativePath::new("buck-out/v2/pkg")?)
                .as_ref(),
        )?;

        let mut builder = ActionDirectoryBuilder::empty();
        insert_file(
            &mut builder,
            ProjectRelativePathBuf::unchecked_new("bin/tool".to_owned()),
            FileMetadata::empty(digest_config.cas_digest_config()),
        )?;
        builder.insert(
            ForwardRelativePath::new("bin/tool-link")?,
            ActionDirectoryEntry::Leaf(ActionDirectoryMember::Symlink(Arc::new(Symlink::new(
                RelativePathBuf::from_system_path(std::path::Path::new("tool"))?,
            )))),
        )?;
        let entry = ActionDirectoryEntry::Dir(
            builder
                .fingerprint(digest_config.as_directory_serializer())
                .shared(&*INTERNER),
        );
        let store_path = fs.root().join(ForwardRelativePath::new("store/pkg")?);

        Box::new(MaterializeStoreOutput {
            staged_path,
            store_path: store_path.clone(),
            entry,
            seal_cas_transport: false,
        })
        .execute(&fs)?;

        assert_eq!(
            fs_util::symlink_metadata(store_path.join(ForwardRelativePath::new("bin/tool-link")?))
                .categorize_internal()?
                .modified()?,
            modified,
        );
        Ok(())
    }

    #[test]
    fn materialize_store_output_rejects_writable_staged_tree() -> buck2_error::Result<()> {
        let temp = ProjectRootTemp::new()?;
        let fs = temp.path().clone();
        let digest_config = DigestConfig::testing_default();
        let staged_path = ProjectRelativePathBuf::unchecked_new("buck-out/v2/pkg".to_owned());
        fs.write_file(
            &staged_path.join(ForwardRelativePath::new("bin/tool")?),
            b"writable",
            false,
        )?;

        let mut builder = ActionDirectoryBuilder::empty();
        insert_file(
            &mut builder,
            ProjectRelativePathBuf::unchecked_new("bin/tool".to_owned()),
            FileMetadata::empty(digest_config.cas_digest_config()),
        )?;
        let entry = ActionDirectoryEntry::Dir(
            builder
                .fingerprint(digest_config.as_directory_serializer())
                .shared(&*INTERNER),
        );
        let store_path = fs.root().join(ForwardRelativePath::new("store/pkg")?);

        let result = Box::new(MaterializeStoreOutput {
            staged_path,
            store_path: store_path.clone(),
            entry,
            seal_cas_transport: false,
        })
        .execute(&fs);

        assert!(result.is_err());
        assert!(!fs_util::try_exists(&store_path)?);
        Ok(())
    }

    #[test]
    fn materialize_cas_store_output_normalizes_and_seals_transport_tree() -> buck2_error::Result<()>
    {
        let temp = ProjectRootTemp::new()?;
        let fs = temp.path().clone();
        let digest_config = DigestConfig::testing_default();
        let staged_path = ProjectRelativePathBuf::unchecked_new("buck-out/v2/pkg".to_owned());
        fs.write_file(
            &staged_path.join(ForwardRelativePath::new("bin/tool")?),
            b"cas",
            true,
        )?;

        let mut builder = ActionDirectoryBuilder::empty();
        insert_file(
            &mut builder,
            ProjectRelativePathBuf::unchecked_new("bin/tool".to_owned()),
            FileMetadata::empty(digest_config.cas_digest_config()),
        )?;
        let entry = ActionDirectoryEntry::Dir(
            builder
                .fingerprint(digest_config.as_directory_serializer())
                .shared(&*INTERNER),
        );
        let store_path = fs.root().join(ForwardRelativePath::new("store/pkg")?);

        Box::new(MaterializeStoreOutput {
            staged_path,
            store_path: store_path.clone(),
            entry,
            seal_cas_transport: true,
        })
        .execute(&fs)?;

        let tool =
            fs_util::symlink_metadata(store_path.join(ForwardRelativePath::new("bin/tool")?))
                .categorize_internal()?;
        assert_eq!(tool.modified()?, UNIX_EPOCH + Duration::from_secs(1));
        #[cfg(unix)]
        assert_eq!(tool.permissions().mode() & 0o222, 0);
        Ok(())
    }

    #[test]
    fn materialize_cas_store_output_seals_authenticated_existing_tree() -> buck2_error::Result<()> {
        let temp = ProjectRootTemp::new()?;
        let fs = temp.path().clone();
        let digest_config = DigestConfig::testing_default();
        let staged_path = ProjectRelativePathBuf::unchecked_new("buck-out/v2/pkg".to_owned());
        let store_path = fs.root().join(ForwardRelativePath::new("store/pkg")?);
        fs.write_file(
            &ProjectRelativePathBuf::unchecked_new("store/pkg/bin/tool".to_owned()),
            b"cas",
            true,
        )?;

        let mut builder = ActionDirectoryBuilder::empty();
        insert_file(
            &mut builder,
            ProjectRelativePathBuf::unchecked_new("bin/tool".to_owned()),
            FileMetadata::empty(digest_config.cas_digest_config()),
        )?;
        let entry = ActionDirectoryEntry::Dir(
            builder
                .fingerprint(digest_config.as_directory_serializer())
                .shared(&*INTERNER),
        );

        Box::new(MaterializeStoreOutput {
            staged_path,
            store_path: store_path.clone(),
            entry,
            seal_cas_transport: true,
        })
        .execute(&fs)?;

        let tool =
            fs_util::symlink_metadata(store_path.join(ForwardRelativePath::new("bin/tool")?))
                .categorize_internal()?;
        assert_eq!(tool.modified()?, UNIX_EPOCH + Duration::from_secs(1));
        #[cfg(unix)]
        assert_eq!(tool.permissions().mode() & 0o222, 0);
        Ok(())
    }

    #[test]
    fn materialize_store_output_removes_sealed_temp_subtree_after_rejection()
    -> buck2_error::Result<()> {
        let temp = ProjectRootTemp::new()?;
        let fs = temp.path().clone();
        let digest_config = DigestConfig::testing_default();
        let staged_path = ProjectRelativePathBuf::unchecked_new("buck-out/v2/pkg".to_owned());
        fs.write_file(
            &staged_path.join(ForwardRelativePath::new("aaa/tool")?),
            b"sealed",
            false,
        )?;
        fs.write_file(
            &staged_path.join(ForwardRelativePath::new("zzz/tool")?),
            b"writable",
            false,
        )?;

        let staged_root = fs.root().join(&staged_path);
        seal_for_store_test(
            staged_root
                .join(ForwardRelativePath::new("aaa/tool")?)
                .as_ref(),
        )?;
        seal_for_store_test(staged_root.join(ForwardRelativePath::new("aaa")?).as_ref())?;

        let mut builder = ActionDirectoryBuilder::empty();
        for path in ["aaa/tool", "zzz/tool"] {
            insert_file(
                &mut builder,
                ProjectRelativePathBuf::unchecked_new(path.to_owned()),
                FileMetadata::empty(digest_config.cas_digest_config()),
            )?;
        }
        let entry = ActionDirectoryEntry::Dir(
            builder
                .fingerprint(digest_config.as_directory_serializer())
                .shared(&*INTERNER),
        );
        let store_path = fs.root().join(ForwardRelativePath::new("store/pkg")?);

        let result = Box::new(MaterializeStoreOutput {
            staged_path,
            store_path: store_path.clone(),
            entry,
            seal_cas_transport: false,
        })
        .execute(&fs);

        assert!(result.is_err());
        assert!(!fs_util::try_exists(&store_path)?);
        assert!(!fs_util::try_exists(AbsNormPathBuf::from(format!(
            "{}.buck2.tmp",
            store_path
        ))?)?);
        Ok(())
    }

    #[test]
    fn materialize_store_output_reuses_existing_store_tree() -> buck2_error::Result<()> {
        let temp = ProjectRootTemp::new()?;
        let fs = temp.path().clone();
        let digest_config = DigestConfig::testing_default();
        let staged_path = ProjectRelativePathBuf::unchecked_new("buck-out/v2/pkg".to_owned());
        let staged_file = staged_path.join(ForwardRelativePath::new("bin/tool")?);
        fs.write_file(&staged_file, b"new", false)?;

        let mut builder = ActionDirectoryBuilder::empty();
        insert_file(
            &mut builder,
            ProjectRelativePathBuf::unchecked_new("bin/tool".to_owned()),
            FileMetadata::empty(digest_config.cas_digest_config()),
        )?;
        let entry = ActionDirectoryEntry::Dir(
            builder
                .fingerprint(digest_config.as_directory_serializer())
                .shared(&*INTERNER),
        );
        let store_path = fs.root().join(ForwardRelativePath::new("store/pkg")?);
        fs.write_file(
            ProjectRelativePathBuf::unchecked_new("store/pkg/bin/tool".to_owned()),
            b"existing",
            false,
        )?;
        #[cfg(unix)]
        assert_ne!(
            fs_util::symlink_metadata(store_path.join(ForwardRelativePath::new("bin/tool")?))
                .categorize_internal()?
                .permissions()
                .mode()
                & 0o222,
            0,
        );

        Box::new(MaterializeStoreOutput {
            staged_path,
            store_path: store_path.clone(),
            entry,
            seal_cas_transport: false,
        })
        .execute(&fs)?;

        assert_eq!(
            std::fs::read(store_path.join(ForwardRelativePath::new("bin/tool")?))?,
            b"existing"
        );
        #[cfg(unix)]
        assert_ne!(
            fs_util::symlink_metadata(store_path.join(ForwardRelativePath::new("bin/tool")?))
                .categorize_internal()?
                .permissions()
                .mode()
                & 0o222,
            0,
        );
        Ok(())
    }

    #[test]
    fn materialize_store_output_does_not_publish_partial_tree() -> buck2_error::Result<()> {
        let temp = ProjectRootTemp::new()?;
        let fs = temp.path().clone();
        let digest_config = DigestConfig::testing_default();
        let staged_path = ProjectRelativePathBuf::unchecked_new("buck-out/v2/pkg".to_owned());

        let mut builder = ActionDirectoryBuilder::empty();
        insert_file(
            &mut builder,
            ProjectRelativePathBuf::unchecked_new("bin/tool".to_owned()),
            FileMetadata::empty(digest_config.cas_digest_config()),
        )?;
        let entry = ActionDirectoryEntry::Dir(
            builder
                .fingerprint(digest_config.as_directory_serializer())
                .shared(&*INTERNER),
        );
        let store_path = fs.root().join(ForwardRelativePath::new("store/pkg")?);

        let result = Box::new(MaterializeStoreOutput {
            staged_path,
            store_path: store_path.clone(),
            entry,
            seal_cas_transport: false,
        })
        .execute(&fs);

        assert!(result.is_err());
        assert!(!fs_util::try_exists(&store_path)?);
        assert!(!fs_util::try_exists(AbsNormPathBuf::from(format!(
            "{}.buck2.tmp",
            store_path
        ))?)?);
        Ok(())
    }
}
