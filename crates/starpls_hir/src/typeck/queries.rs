//! Inference query boundaries. Each query owns its working state and follows
//! only the imported definitions needed for its result.

use starpls_common::File;
use starpls_common::InFile;

use crate::def::ExprId;
use crate::def::LoadItemId;
use crate::def::LoadStmt;
use crate::def::ParamId;
use crate::def::StmtId;
use crate::typeck::resolve_type_ref;
use crate::typeck::Ty;
use crate::typeck::TyContext;
use crate::typeck::TypeRef;
use crate::Db;

#[salsa::tracked(returns(clone))]
pub(crate) fn infer_expr(db: &dyn Db, file: File, expr: ExprId) -> Ty {
    TyContext::new(db).infer_expr(file, expr)
}

#[salsa::tracked(returns(clone))]
pub(crate) fn infer_param(db: &dyn Db, file: File, param: ParamId) -> Ty {
    TyContext::new(db).infer_param(file, param)
}

#[salsa::tracked(returns(clone))]
pub(crate) fn infer_load_item(db: &dyn Db, file: File, item: LoadItemId) -> Ty {
    TyContext::new(db).infer_load_item(file, item)
}

#[salsa::tracked(returns(clone))]
pub(crate) fn resolve_load_stmt(db: &dyn Db, file: File, load: LoadStmt) -> Option<File> {
    TyContext::new(db).resolve_load_stmt(file, load)
}

pub(crate) fn resolve_type(db: &dyn Db, type_ref: TypeRef, usage: Option<InFile<StmtId>>) -> Ty {
    resolve_type_query(db, db.environment(), type_ref, usage)
}

#[salsa::tracked(returns(clone))]
pub(crate) fn resolve_type_query(
    db: &dyn Db,
    _environment: crate::Environment,
    type_ref: TypeRef,
    usage: Option<InFile<StmtId>>,
) -> Ty {
    resolve_type_ref(&mut TyContext::new(db), &type_ref, usage).0
}
