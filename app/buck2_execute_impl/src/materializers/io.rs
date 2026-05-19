/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use buck2_core::fs::project::ProjectRoot;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_directory::directory::directory::Directory;
use buck2_directory::directory::entry::DirectoryEntry;
use buck2_execute::directory::ActionDirectory;
use buck2_execute::directory::ActionDirectoryEntry;
use buck2_execute::directory::ActionDirectoryMember;
use buck2_execute::directory::ActionDirectoryRef;
use buck2_execute::directory::ActionSharedDirectory;
use buck2_execute::execute::blocking::IoRequest;
use buck2_fs::error::IoResultExt;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_norm_path::AbsNormPath;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use buck2_hash::StdBuckHashMap;

pub struct MaterializeTreeStructure {
    pub path: ProjectRelativePathBuf,
    pub entry: ActionDirectoryEntry<ActionSharedDirectory>,
}

impl IoRequest for MaterializeTreeStructure {
    fn execute(self: Box<Self>, project_fs: &ProjectRoot) -> buck2_error::Result<()> {
        materialize_dirs_and_syms(self.entry.as_ref(), project_fs.root().join(&self.path))?;

        Ok(())
    }
}

pub struct MaterializeStoreOutput {
    pub staged_path: ProjectRelativePathBuf,
    pub store_path: AbsNormPathBuf,
    pub entry: ActionDirectoryEntry<ActionSharedDirectory>,
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
            return Ok(());
        }

        let staged_path = project_fs.root().join(&self.staged_path);
        materialize_dirs_and_syms(self.entry.as_ref(), &self.store_path)?;
        materialize_files(self.entry.as_ref(), &staged_path, &self.store_path, None)?;

        Ok(())
    }
}

/// Materializes the entry at `dest`.
///
/// - `materialize_dirs_and_syms`: if `true`, materializes directories and
///   symlinks.
/// - `file_src`: takes the destination path of a file, and returns its
///   source path (where it should be copied from). If it returns [`None`],
///   the file is not materialized.
fn materialize<F, D>(
    entry: DirectoryEntry<&D, &ActionDirectoryMember>,
    dest: &AbsNormPath,
    materialize_dirs_and_syms: bool,
    mut file_src: F,
    executable_bit_override: Option<bool>,
) -> buck2_error::Result<()>
where
    F: FnMut(&AbsNormPath) -> Option<AbsNormPathBuf>,
    D: ActionDirectory,
{
    let mut dest = dest.to_owned();
    if materialize_dirs_and_syms {
        // create the directory where we'll materialize the entry
        if let Some(parent) = dest.parent() {
            fs_util::create_dir_all(parent)?;
        }
    }
    materialize_recursively(
        entry.map_dir(|d| Directory::as_ref(d)),
        &mut dest,
        materialize_dirs_and_syms,
        &mut file_src,
        executable_bit_override,
    )
}

/// Materializes the directories and symlinks of an entry at `dest`. Files
/// are not materialized.
pub(crate) fn materialize_dirs_and_syms<P, D>(
    entry: DirectoryEntry<&D, &ActionDirectoryMember>,
    dest: P,
) -> buck2_error::Result<()>
where
    P: AsRef<AbsNormPath>,
    D: ActionDirectory,
{
    materialize(entry, dest.as_ref(), true, |_: &AbsNormPath| None, None)
}

/// Materializes the files of an the entry rooted at `dest`.
///
/// Files are copied from `src`. In other words, if a file would be
/// materialized at `dest/p`, then it's copied from `src/p`.
pub(crate) fn materialize_files<P, D>(
    entry: DirectoryEntry<&D, &ActionDirectoryMember>,
    src: P,
    dest: P,
    executable_bit_override: Option<bool>,
) -> buck2_error::Result<()>
where
    P: AsRef<AbsNormPath>,
    D: ActionDirectory,
{
    let src = src.as_ref();
    let dest = dest.as_ref();
    let file_src = |d: &AbsNormPath| {
        // It's safe to unwrap because `materialize_impl` always gives us a
        // path inside `dest`.
        let subpath = d.strip_prefix(dest).unwrap();
        if subpath.as_str().is_empty() {
            // `dest` itself is a file
            Some(src.to_buf())
        } else {
            Some(src.join(subpath))
        }
    };
    materialize(entry, dest, false, file_src, executable_bit_override)
}

/// Materializes the files of an entry rooted at `dest`.
///
/// For a file at path `file_dest` in the entry, if `file_dest` exists in
/// `srcs` with value `file_src`, the file is copied from `file_src` to
/// `file_dest`. It's then removed from `srcs`.
fn _materialize_files_from_map<P, D>(
    entry: DirectoryEntry<&D, &ActionDirectoryMember>,
    srcs: &mut StdBuckHashMap<AbsNormPathBuf, AbsNormPathBuf>,
    dest: P,
) -> buck2_error::Result<()>
where
    P: AsRef<AbsNormPath>,
    D: ActionDirectory,
{
    let file_src = |d: &AbsNormPath| srcs.remove(d);
    materialize(entry, dest.as_ref(), false, file_src, None)
}

#[cfg(test)]
mod tests {
    use buck2_common::file_ops::metadata::FileMetadata;
    use buck2_core::fs::project::ProjectRootTemp;
    use buck2_execute::digest_config::DigestConfig;
    use buck2_execute::directory::ActionDirectoryBuilder;
    use buck2_execute::directory::INTERNER;
    use buck2_execute::directory::insert_file;
    use buck2_fs::fs_util;
    use buck2_fs::paths::forward_rel_path::ForwardRelativePath;

    use super::*;

    #[test]
    fn materialize_store_output_copies_staged_files() -> buck2_error::Result<()> {
        let temp = ProjectRootTemp::new()?;
        let fs = temp.path().clone();
        let digest_config = DigestConfig::testing_default();
        let staged_path = ProjectRelativePathBuf::unchecked_new("buck-out/v2/pkg".to_owned());
        let staged_file = staged_path.join(ForwardRelativePath::new("bin/tool")?);
        fs.write_file(&staged_file, b"", false)?;

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
        })
        .execute(&fs)?;

        assert!(fs_util::try_exists(
            store_path.join(ForwardRelativePath::new("bin/tool")?)
        )?);
        assert!(fs_util::try_exists(AbsNormPathBuf::from(format!(
            "{}.buck2.lock",
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

        Box::new(MaterializeStoreOutput {
            staged_path,
            store_path: store_path.clone(),
            entry,
        })
        .execute(&fs)?;

        assert_eq!(
            std::fs::read(store_path.join(ForwardRelativePath::new("bin/tool")?))?,
            b"existing"
        );
        Ok(())
    }
}

fn materialize_recursively<'a, F, D>(
    entry: DirectoryEntry<D, &ActionDirectoryMember>,
    dest: &mut AbsNormPathBuf,
    materialize_dirs_and_syms: bool,
    file_src: &mut F,
    executable_bit_override: Option<bool>,
) -> buck2_error::Result<()>
where
    F: FnMut(&AbsNormPath) -> Option<AbsNormPathBuf>,
    D: ActionDirectoryRef<'a>,
{
    match entry {
        DirectoryEntry::Dir(d) => {
            if materialize_dirs_and_syms {
                fs_util::create_dir_all(&dest)?;
            }
            for (name, entry) in d.entries() {
                dest.push(name);
                materialize_recursively(
                    entry,
                    dest,
                    materialize_dirs_and_syms,
                    file_src,
                    executable_bit_override,
                )?;
                dest.pop();
            }
            Ok(())
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::File(_)) => {
            if let Some(src) = file_src(dest) {
                fs_util::copy(src, &dest).categorize_internal()?;
                if let Some(executable_bit_override) = executable_bit_override {
                    fs_util::set_executable(&dest, executable_bit_override)
                        .categorize_internal()?;
                }
            }
            Ok(())
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::Symlink(s)) => {
            if materialize_dirs_and_syms
                && fs_util::symlink_metadata(&dest)
                    .categorize_internal()
                    .is_err()
            {
                fs_util::symlink(s.target().as_str(), dest).categorize_internal()?;
            }
            Ok(())
        }
        DirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(s)) => {
            if materialize_dirs_and_syms
                && fs_util::symlink_metadata(&dest)
                    .categorize_internal()
                    .is_err()
            {
                fs_util::symlink(s.target(), dest).categorize_internal()?;
            }
            Ok(())
        }
    }
}
