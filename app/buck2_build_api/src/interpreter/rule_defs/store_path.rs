/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt;
use std::fmt::Formatter;

use allocative::Allocative;
use buck2_error::buck2_error;
use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
use starlark::any::ProvidesStaticType;
use starlark::starlark_simple_value;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::starlark_value;

#[derive(Debug, Trace, ProvidesStaticType, NoSerialize, Allocative)]
pub struct StarlarkStorePath {
    path: AbsNormPathBuf,
}

impl StarlarkStorePath {
    pub fn from_identity(store_path_key: &str, store_name: &str) -> buck2_error::Result<Self> {
        if store_path_key.len() != 32
            || !store_path_key
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "store path key must be a 32-character lowercase hex digest prefix: `{}`",
                store_path_key
            ));
        }

        if store_name.is_empty()
            || store_name == "."
            || store_name == ".."
            || store_name.contains('/')
        {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "store name must be a non-empty single path entry: `{}`",
                store_name
            ));
        }

        let path = AbsNormPathBuf::from(format!("/pkgs/store/{}-{}", store_path_key, store_name))?;

        Ok(Self { path })
    }

    pub fn path(&self) -> &AbsNormPathBuf {
        &self.path
    }
}

impl fmt::Display for StarlarkStorePath {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path)
    }
}

starlark_simple_value!(StarlarkStorePath);

#[starlark_value(type = "StorePath")]
impl<'v> StarlarkValue<'v> for StarlarkStorePath {}
