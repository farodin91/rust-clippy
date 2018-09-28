use crate::rustc::lint::{EarlyContext, EarlyLintPass, LintArray, LintPass, in_external_macro, LintContext};
use crate::rustc::{declare_tool_lint, lint_array};
use if_chain::if_chain;
use crate::syntax::ast;
use crate::syntax::source_map::Span;
use crate::syntax::visit::FnKind;
use crate::syntax_pos::BytePos;
use crate::rustc_errors::Applicability;
use crate::utils::{in_macro, match_path_ast, snippet_opt, span_lint_and_then, span_note_and_lint};

/// **What it does:** Checks for return statements at the end of a block.
///
/// **Why is this bad?** Removing the `return` and semicolon will make the code
/// more rusty.
///
/// **Known problems:** If the computation returning the value borrows a local
/// variable, removing the `return` may run afoul of the borrow checker.
///
/// **Example:**
/// ```rust
/// fn foo(x: usize) { return x; }
/// ```
/// simplify to
/// ```rust
/// fn foo(x: usize) { x }
/// ```
declare_clippy_lint! {
    pub NEEDLESS_RETURN,
    style,
    "using a return statement like `return expr;` where an expression would suffice"
}

/// **What it does:** Checks for `let`-bindings, which are subsequently
/// returned.
///
/// **Why is this bad?** It is just extraneous code. Remove it to make your code
/// more rusty.
///
/// **Known problems:** None.
///
/// **Example:**
/// ```rust
/// fn foo() -> String {
///    let x = String::new();
///    x
///}
/// ```
/// instead, use
/// ```
/// fn foo() -> String {
///    String::new()
///}
/// ```
declare_clippy_lint! {
    pub LET_AND_RETURN,
    style,
    "creating a let-binding and then immediately returning it like `let x = expr; x` at \
     the end of a block"
}

/// **What it does:** Checks for unit (`()`) expressions that can be removed
///
/// **Why is this bad?** Such expressions add no value, but can make the code
/// less readable. Depending on formatting they can make a `break` or `return`
/// statement look like a function call.
///
/// **Known problems:** None.
///
/// **Example:**
/// ```rust
/// fn return_unit() -> () { () }
/// ```
declare_clippy_lint! {
    pub UNUSED_UNIT,
    style,
    "needless unit expression"
}

#[derive(Copy, Clone)]
pub struct ReturnPass;

impl ReturnPass {
    // Check the final stmt or expr in a block for unnecessary return.
    fn check_block_return(&mut self, cx: &EarlyContext<'_>, block: &ast::Block) {
        if let Some(stmt) = block.stmts.last() {
            match stmt.node {
                ast::StmtKind::Expr(ref expr) | ast::StmtKind::Semi(ref expr) => {
                    self.check_final_expr(cx, expr, Some(stmt.span));
                },
                _ => (),
            }
        }
    }

    // Check a the final expression in a block if it's a return.
    fn check_final_expr(&mut self, cx: &EarlyContext<'_>, expr: &ast::Expr, span: Option<Span>) {
        match expr.node {
            // simple return is always "bad"
            ast::ExprKind::Ret(Some(ref inner)) => {
                // allow `#[cfg(a)] return a; #[cfg(b)] return b;`
                if !expr.attrs.iter().any(attr_is_cfg) {
                    self.emit_return_lint(cx, span.expect("`else return` is not possible"), inner.span);
                }
            },
            // a whole block? check it!
            ast::ExprKind::Block(ref block, _) => {
                self.check_block_return(cx, block);
            },
            // an if/if let expr, check both exprs
            // note, if without else is going to be a type checking error anyways
            // (except for unit type functions) so we don't match it
            ast::ExprKind::If(_, ref ifblock, Some(ref elsexpr)) => {
                self.check_block_return(cx, ifblock);
                self.check_final_expr(cx, elsexpr, None);
            },
            // a match expr, check all arms
            ast::ExprKind::Match(_, ref arms) => for arm in arms {
                self.check_final_expr(cx, &arm.body, Some(arm.body.span));
            },
            _ => (),
        }
    }

    fn emit_return_lint(&mut self, cx: &EarlyContext<'_>, ret_span: Span, inner_span: Span) {
        if in_external_macro(cx.sess(), inner_span) || in_macro(inner_span) {
            return;
        }
        span_lint_and_then(cx, NEEDLESS_RETURN, ret_span, "unneeded return statement", |db| {
            if let Some(snippet) = snippet_opt(cx, inner_span) {
                db.span_suggestion_with_applicability(
                    ret_span,
                    "remove `return` as shown",
                    snippet,
                    Applicability::MachineApplicable,
                );
            }
        });
    }

    // Check for "let x = EXPR; x"
    fn check_let_return(&mut self, cx: &EarlyContext<'_>, block: &ast::Block) {
        let mut it = block.stmts.iter();

        // we need both a let-binding stmt and an expr
        if_chain! {
            if let Some(retexpr) = it.next_back();
            if let ast::StmtKind::Expr(ref retexpr) = retexpr.node;
            if let Some(stmt) = it.next_back();
            if let ast::StmtKind::Local(ref local) = stmt.node;
            // don't lint in the presence of type inference
            if local.ty.is_none();
            if !local.attrs.iter().any(attr_is_cfg);
            if let Some(ref initexpr) = local.init;
            if let ast::PatKind::Ident(_, ident, _) = local.pat.node;
            if let ast::ExprKind::Path(_, ref path) = retexpr.node;
            if match_path_ast(path, &[&ident.as_str()]);
            if !in_external_macro(cx.sess(), initexpr.span);
            then {
                    span_note_and_lint(cx,
                                       LET_AND_RETURN,
                                       retexpr.span,
                                       "returning the result of a let binding from a block. \
                                       Consider returning the expression directly.",
                                       initexpr.span,
                                       "this expression can be directly returned");
            }
        }
    }

    // get the def site
    fn get_def(&self, span: Span) -> Option<Span> {
        span.ctxt().outer().expn_info().and_then(|info| info.def_site)
    }

    // is this expr a `()` unit?
    fn is_unit_expr(&self, expr: &ast::Expr) -> bool {
        if let ast::ExprKind::Tup(ref vals) = expr.node {
            vals.is_empty()
        } else {
            false
        }
    }

    fn check_fn_ty(&mut self, cx: &EarlyContext<'_>, ty: &ast::Ty, span: Span) {
        if_chain! {
            if let ast::TyKind::Tup(ref vals) = ty.node;
            if vals.is_empty() && !in_macro(ty.span) &&
                    self.get_def(span) == self.get_def(ty.span);
            then {
                let (rspan, appl) = if let Ok(fn_source) =
                        cx.sess().source_map()
                                 .span_to_snippet(span.with_hi(ty.span.hi())) {
                    if let Some(rpos) = fn_source.rfind("->") {
                        (ty.span.with_lo(BytePos(span.lo().0 + rpos as u32)),
                            Applicability::MachineApplicable)
                    } else {
                        (ty.span, Applicability::MaybeIncorrect)
                    }
                } else {
                    (ty.span, Applicability::MaybeIncorrect)
                };
                span_lint_and_then(cx, UNUSED_UNIT, rspan, "unneeded unit return type", |db| {
                    db.span_suggestion_with_applicability(
                        rspan,
                        "remove the `-> ()`",
                        String::new(),
                        appl,
                    );
                });
            }
        }
    }

    fn check_decl(&mut self, cx: &EarlyContext<'_>, decl: &ast::FnDecl, span: Span) {
        if let ast::FunctionRetTy::Ty(ref ty) = decl.output {
            self.check_fn_ty(cx, ty, span);
        }
    }
}

impl LintPass for ReturnPass {
    fn get_lints(&self) -> LintArray {
        lint_array!(NEEDLESS_RETURN, LET_AND_RETURN)
    }
}

impl EarlyLintPass for ReturnPass {
    fn check_fn(&mut self, cx: &EarlyContext<'_>, kind: FnKind<'_>, decl: &ast::FnDecl, span: Span, _: ast::NodeId) {
        match kind {
            FnKind::ItemFn(.., block) | FnKind::Method(.., block) => self.check_block_return(cx, block),
            FnKind::Closure(body) => self.check_final_expr(cx, body, Some(body.span)),
        }
        self.check_decl(cx, decl, span);
    }

    fn check_block(&mut self, cx: &EarlyContext<'_>, block: &ast::Block) {
        self.check_let_return(cx, block);
        if_chain! {
            if let Some(ref stmt) = block.stmts.last();
            if let ast::StmtKind::Expr(ref expr) = stmt.node;
            if self.is_unit_expr(expr);
            if !in_macro(expr.span);
            then {
                let sp = expr.span;
                span_lint_and_then(cx, UNUSED_UNIT, sp, "unneeded unit expression", |db| {
                    db.span_suggestion_with_applicability(
                        sp,
                        "remove the final `()`",
                        String::new(),
                        Applicability::MachineApplicable,
                    );
                });
            }
        }
    }

    fn check_expr(&mut self, cx: &EarlyContext<'_>, e: &ast::Expr) {
        match e.node {
            ast::ExprKind::Ret(Some(ref expr)) | ast::ExprKind::Break(_, Some(ref expr)) => {
                if self.is_unit_expr(expr) && !in_macro(expr.span) {
                    span_lint_and_then(cx, UNUSED_UNIT, expr.span, "unneeded `()`", |db| {
                        db.span_suggestion_with_applicability(
                            expr.span,
                            "remove the `()`",
                            String::new(),
                            Applicability::MachineApplicable,
                        );
                    });
                }
            }
            _ => ()
        }
    }

    fn check_ty(&mut self, cx: &EarlyContext<'_>, ty: &ast::Ty) {
        match ty.node {
            ast::TyKind::BareFn(ref bare_fn_ty) => {
                self.check_decl(cx, &bare_fn_ty.decl, ty.span);
            }
            _ => ()
        }
    }

    fn check_path(&mut self, cx: &EarlyContext<'_>, path: &ast::Path, _: ast::NodeId) {
        if_chain!{
            if path.segments.len() == 1;
            if let Some(ref segment) = path.segments.first();
            if ["Fn", "FnMut", "FnOnce"].contains(&&*segment.ident.as_str());
            if let Some(ref args) = segment.args;
            if let ast::GenericArgs::Parenthesized(ref par_args) = &**args;
            if let Some(ref ty) = par_args.output;
            then {
                self.check_fn_ty(cx, ty, par_args.span);
            }
        }
    }
}

fn attr_is_cfg(attr: &ast::Attribute) -> bool {
    attr.meta_item_list().is_some() && attr.name() == "cfg"
}
