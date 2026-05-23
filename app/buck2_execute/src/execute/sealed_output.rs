/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use buck2_fs::error::IoResultExt;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_norm_path::AbsNormPath;
#[cfg(unix)]
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;

pub(crate) fn remove_output_tree(path: &AbsNormPath) -> buck2_error::Result<()> {
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

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use buck2_core::fs::project::ProjectRootTemp;
    use buck2_core::fs::project_rel_path::ProjectRelativePath;
    use buck2_fs::error::IoResultExt;
    use buck2_fs::fs_util;
    use buck2_fs::paths::forward_rel_path::ForwardRelativePath;

    use crate::execute::clean_output_paths::cleanup_path;

    #[test]
    fn cleanup_removes_read_only_directory_tree() -> buck2_error::Result<()> {
        let temp = ProjectRootTemp::new()?;
        let fs = temp.path().clone();
        let output = fs.root().join(ForwardRelativePath::new("buck-out/v2/pkg")?);
        let child = output.join(ForwardRelativePath::new("lib")?);
        fs_util::create_dir_all(&child)?;
        std::fs::write(
            child
                .join(ForwardRelativePath::new("payload")?)
                .as_maybe_relativized(),
            b"payload",
        )?;

        for path in [&child, &output] {
            let mut permissions = fs_util::symlink_metadata(path)
                .categorize_internal()?
                .permissions();
            permissions.set_mode(0o555);
            fs_util::set_permissions(path, permissions).categorize_internal()?;
        }

        cleanup_path(&fs, ProjectRelativePath::new("buck-out/v2/pkg")?)?;
        assert!(!fs_util::try_exists(&output)?);
        Ok(())
    }
}
