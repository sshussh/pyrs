//! Crate-wide prelude so split modules can share helpers without
//! threading explicit imports through every call site.

pub(crate) use crate::analyze::*;
pub(crate) use crate::builtins::*;
pub(crate) use crate::class_env::*;
pub(crate) use crate::classes::*;
pub(crate) use crate::closures::*;
pub(crate) use crate::ctx::*;
pub(crate) use crate::error::*;
pub(crate) use crate::flow::*;
pub(crate) use crate::imports::*;
pub(crate) use crate::infer::*;
pub(crate) use crate::lower_expr::*;
pub(crate) use crate::lower_fn::*;
pub(crate) use crate::lower_stmt::*;
pub(crate) use crate::module::*;
pub(crate) use crate::ops::*;
pub(crate) use crate::types::*;
