use crate::inc_dec::{collect_stmt, occurring_variables_expr, JPLiveVarMap, LiveVarSet};
use crate::ir::{Call, Expr, Proc, Stmt};
use crate::layout::{Layout, UnionLayout};
use bumpalo::collections::Vec;
use bumpalo::Bump;
use roc_collections::all::MutSet;
use roc_module::symbol::{IdentIds, ModuleId, Symbol};

pub fn insert_reset_reuse<'a, 'i>(
    arena: &'a Bump,
    home: ModuleId,
    ident_ids: &'i mut IdentIds,
    mut proc: Proc<'a>,
) -> Proc<'a> {
    let mut env = Env {
        arena,
        home,
        ident_ids,
        jp_live_vars: Default::default(),
    };

    let new_body = function_r(&mut env, arena.alloc(proc.body));
    proc.body = new_body.clone();

    proc
}

struct CtorInfo<'a> {
    id: u8,
    layout: UnionLayout<'a>,
}

fn may_reuse(tag_layout: UnionLayout, tag_id: u8, other: &CtorInfo) -> bool {
    if tag_layout != other.layout {
        return false;
    }

    // if the tag id is represented as NULL, there is no memory to re-use
    match tag_layout {
        UnionLayout::NonRecursive(_)
        | UnionLayout::Recursive(_)
        | UnionLayout::NonNullableUnwrapped(_) => true,
        UnionLayout::NullableWrapped { nullable_id, .. } => tag_id as i64 != nullable_id,
        UnionLayout::NullableUnwrapped {
            nullable_id,
            other_fields,
        } => (tag_id != 0) != nullable_id,
    }
}

#[derive(Debug)]
struct Env<'a, 'i> {
    arena: &'a Bump,

    /// required for creating new `Symbol`s
    home: ModuleId,
    ident_ids: &'i mut IdentIds,

    jp_live_vars: JPLiveVarMap,
}

impl<'a, 'i> Env<'a, 'i> {
    fn unique_symbol(&mut self) -> Symbol {
        let ident_id = self.ident_ids.gen_unique();

        self.home.register_debug_idents(&self.ident_ids);

        Symbol::new(self.home, ident_id)
    }
}

fn function_s<'a, 'i>(
    env: &mut Env<'a, 'i>,
    w: Symbol,
    c: &CtorInfo<'a>,
    stmt: &'a Stmt<'a>,
) -> &'a Stmt<'a> {
    use Stmt::*;

    let arena = env.arena;

    match stmt {
        Let(symbol, expr, layout, continuation) => match expr {
            Expr::Tag {
                tag_layout,
                tag_id,
                tag_name,
                arguments,
            } if may_reuse(*tag_layout, *tag_id, c) => {
                // for now, always overwrite the tag ID just to be sure
                let update_tag_id = true;

                let new_expr = Expr::Reuse {
                    symbol: w,
                    update_tag_id,
                    tag_layout: *tag_layout,
                    tag_id: *tag_id,
                    tag_name: tag_name.clone(),
                    arguments,
                };
                let new_stmt = Let(*symbol, new_expr, *layout, continuation);

                arena.alloc(new_stmt)
            }
            _ => {
                let rest = function_s(env, w, c, continuation);
                let new_stmt = Let(*symbol, expr.clone(), *layout, rest);

                arena.alloc(new_stmt)
            }
        },
        Join {
            id,
            parameters,
            body,
            remainder,
        } => {
            let id = *id;
            let body: &Stmt = *body;
            let new_body = function_s(env, w, c, body);

            let new_join = if std::ptr::eq(body, new_body) || body == new_body {
                // the join point body will consume w
                Join {
                    id,
                    parameters,
                    body: new_body,
                    remainder,
                }
            } else {
                let new_remainder = function_s(env, w, c, remainder);

                Join {
                    id,
                    parameters,
                    body,
                    remainder: new_remainder,
                }
            };

            arena.alloc(new_join)
        }
        Invoke {
            symbol,
            call,
            layout,
            pass,
            fail,
            exception_id,
        } => {
            let new_pass = function_s(env, w, c, pass);
            let new_fail = function_s(env, w, c, fail);

            let new_invoke = Invoke {
                symbol: *symbol,
                call: call.clone(),
                layout: *layout,
                pass: new_pass,
                fail: new_fail,
                exception_id: *exception_id,
            };

            arena.alloc(new_invoke)
        }
        Switch {
            cond_symbol,
            cond_layout,
            branches,
            default_branch,
            ret_layout,
        } => {
            let mut new_branches = Vec::with_capacity_in(branches.len(), arena);
            new_branches.extend(branches.iter().map(|(tag, info, body)| {
                let new_body = function_s(env, w, c, body);

                (*tag, info.clone(), new_body.clone())
            }));

            let new_default = function_s(env, w, c, default_branch.1);

            let new_switch = Switch {
                cond_symbol: *cond_symbol,
                cond_layout: *cond_layout,
                branches: new_branches.into_bump_slice(),
                default_branch: (default_branch.0.clone(), new_default),
                ret_layout: *ret_layout,
            };

            arena.alloc(new_switch)
        }
        Refcounting(op, continuation) => {
            let continuation: &Stmt = *continuation;
            let new_continuation = function_s(env, w, c, continuation);

            if std::ptr::eq(continuation, new_continuation) || continuation == new_continuation {
                stmt
            } else {
                let new_refcounting = Refcounting(*op, new_continuation);

                arena.alloc(new_refcounting)
            }
        }
        Resume(_) | Ret(_) | Jump(_, _) | RuntimeError(_) => stmt,
    }
}

fn try_function_s<'a, 'i>(
    env: &mut Env<'a, 'i>,
    x: Symbol,
    c: &CtorInfo<'a>,
    stmt: &'a Stmt<'a>,
) -> &'a Stmt<'a> {
    let w = env.unique_symbol();

    let new_stmt = function_s(env, w, c, stmt);

    if std::ptr::eq(stmt, new_stmt) || stmt == new_stmt {
        stmt
    } else {
        let f00 = Stmt::Let(w, Expr::Reset(x), Layout::Union(c.layout), new_stmt);

        env.arena.alloc(f00)
    }
}

fn function_d_finalize<'a, 'i>(
    env: &mut Env<'a, 'i>,
    x: Symbol,
    c: &CtorInfo<'a>,
    output: (&'a Stmt<'a>, bool),
) -> &'a Stmt<'a> {
    let (stmt, x_live_in_stmt) = output;
    if x_live_in_stmt {
        stmt
    } else {
        try_function_s(env, x, c, stmt)
    }
}

fn function_d_main<'a, 'i>(
    env: &mut Env<'a, 'i>,
    x: Symbol,
    c: &CtorInfo<'a>,
    stmt: &'a Stmt<'a>,
) -> (&'a Stmt<'a>, bool) {
    use Stmt::*;

    let arena = env.arena;

    match stmt {
        Let(symbol, expr, layout, continuation) => match expr {
            Expr::Tag { arguments, .. } if arguments.iter().any(|s| *s == x) => {
                // If the scrutinee `x` (the one that is providing memory) is being
                // stored in a constructor, then reuse will probably not be able to reuse memory at runtime.
                // It may work only if the new cell is consumed, but we ignore this case.
                (stmt, true)
            }
            _ => {
                let (b, found) = function_d_main(env, x, c, continuation);

                let mut result = MutSet::default();
                if found || {
                    occurring_variables_expr(expr, &mut result);
                    !result.contains(&x)
                } {
                    let let_stmt = Let(*symbol, expr.clone(), *layout, b);

                    (arena.alloc(let_stmt), found)
                } else {
                    let b = try_function_s(env, x, c, b);
                    let let_stmt = Let(*symbol, expr.clone(), *layout, b);

                    (arena.alloc(let_stmt), found)
                }
            }
        },
        Invoke {
            symbol,
            call,
            layout,
            pass,
            fail,
            exception_id,
        } => todo!(),
        Switch {
            cond_symbol,
            cond_layout,
            branches,
            default_branch,
            ret_layout,
        } => {
            if has_live_var(&env.jp_live_vars, stmt, x) {
                // if `x` is live in `stmt`, we recursively process each branch
                let mut new_branches = Vec::with_capacity_in(branches.len(), arena);

                for (tag, info, body) in branches.iter() {
                    let temp = function_d_main(env, x, c, body);
                    let new_body = function_d_finalize(env, x, c, temp);

                    new_branches.push((*tag, info.clone(), new_body.clone()));
                }

                let new_default = {
                    let (info, body) = default_branch;
                    let temp = function_d_main(env, x, c, body);
                    let new_body = function_d_finalize(env, x, c, temp);

                    (info.clone(), new_body)
                };

                let new_switch = Switch {
                    cond_symbol: *cond_symbol,
                    cond_layout: *cond_layout,
                    branches: new_branches.into_bump_slice(),
                    default_branch: new_default,
                    ret_layout: *ret_layout,
                };

                (arena.alloc(new_switch), true)
            } else {
                (stmt, false)
            }
        }
        Refcounting(modify_rc, continuation) => {
            let (b, found) = function_d_main(env, x, c, continuation);

            if found || modify_rc.get_symbol() != x {
                let refcounting = Refcounting(*modify_rc, b);

                (arena.alloc(refcounting), found)
            } else {
                let b = try_function_s(env, x, c, b);
                let refcounting = Refcounting(*modify_rc, b);

                (arena.alloc(refcounting), found)
            }
        }
        Join {
            id,
            parameters,
            body,
            remainder,
        } => {
            env.jp_live_vars.insert(*id, LiveVarSet::default());

            let body_live_vars = collect_stmt(body, &env.jp_live_vars, LiveVarSet::default());

            env.jp_live_vars.insert(*id, body_live_vars);

            let (b, found) = function_d_main(env, x, c, remainder);

            let (v, _found) = function_d_main(env, x, c, body);

            env.jp_live_vars.remove(id);

            // If `found' == true`, then `Dmain b` must also have returned `(b, true)` since
            // we assume the IR does not have dead join points. So, if `x` is live in `j` (i.e., `v`),
            // then it must also live in `b` since `j` is reachable from `b` with a `jmp`.
            // On the other hand, `x` may be live in `b` but dead in `j` (i.e., `v`). -/
            let new_join = Join {
                id: *id,
                parameters,
                body: v,
                remainder: b,
            };

            (arena.alloc(new_join), found)
        }
        Ret(_) | Resume(_) | Jump(_, _) | RuntimeError(_) => {
            (stmt, has_live_var(&env.jp_live_vars, stmt, x))
        }
    }
}

fn function_d<'a, 'i>(
    env: &mut Env<'a, 'i>,
    x: Symbol,
    c: &CtorInfo<'a>,
    stmt: &'a Stmt<'a>,
) -> &'a Stmt<'a> {
    let temp = function_d_main(env, x, c, stmt);

    function_d_finalize(env, x, c, temp)
}

fn function_r<'a, 'i>(env: &mut Env<'a, 'i>, stmt: &'a Stmt<'a>) -> &'a Stmt<'a> {
    use Stmt::*;

    let arena = env.arena;

    match stmt {
        Switch {
            cond_symbol,
            cond_layout,
            branches,
            default_branch,
            ret_layout,
        } => {
            let mut new_branches = Vec::with_capacity_in(branches.len(), arena);

            // TODO for non-recursive unions there is no benefit
            let benefits_from_reuse = match cond_layout {
                Layout::Union(union_layout) => Some(union_layout),
                _ => None,
            };

            for (tag, info, body) in branches.iter() {
                let temp = function_r(env, body);

                // TODO the branch info stores the tag ID
                let new_body = match benefits_from_reuse {
                    Some(union_layout) => {
                        let x = *cond_symbol;
                        let ctor_info = CtorInfo {
                            layout: *union_layout,
                            id: 0,
                        };
                        function_d(env, x, &ctor_info, temp)
                    }
                    None => temp,
                };

                new_branches.push((*tag, info.clone(), new_body.clone()));
            }

            let new_default = {
                let (info, body) = default_branch;
                let temp = function_r(env, body);

                let new_body = match benefits_from_reuse {
                    Some(union_layout) => {
                        let x = *cond_symbol;
                        let ctor_info = CtorInfo {
                            layout: *union_layout,
                            id: 0,
                        };
                        function_d(env, x, &ctor_info, temp)
                    }
                    None => temp,
                };

                (info.clone(), new_body)
            };

            let new_switch = Switch {
                cond_symbol: *cond_symbol,
                cond_layout: *cond_layout,
                branches: new_branches.into_bump_slice(),
                default_branch: new_default,
                ret_layout: *ret_layout,
            };

            arena.alloc(new_switch)
        }

        Join {
            id,
            parameters,
            body,
            remainder,
        } => {
            env.jp_live_vars.insert(*id, LiveVarSet::default());

            let body_live_vars = collect_stmt(body, &env.jp_live_vars, LiveVarSet::default());

            env.jp_live_vars.insert(*id, body_live_vars);

            let b = function_r(env, remainder);

            let v = function_r(env, body);

            env.jp_live_vars.remove(id);

            let join = Join {
                id: *id,
                parameters,
                body: v,
                remainder: b,
            };

            arena.alloc(join)
        }

        Let(symbol, expr, layout, continuation) => {
            let b = function_r(env, continuation);

            arena.alloc(Let(*symbol, expr.clone(), *layout, b))
        }
        Invoke {
            symbol,
            call,
            layout,
            pass,
            fail,
            exception_id,
        } => todo!(),
        Refcounting(modify_rc, continuation) => {
            let b = function_r(env, continuation);

            arena.alloc(Refcounting(*modify_rc, b))
        }

        Resume(_) | Ret(_) | Jump(_, _) | RuntimeError(_) => {
            // terminals
            stmt
        }
    }
}

fn has_live_var<'a>(jp_live_vars: &JPLiveVarMap, stmt: &'a Stmt<'a>, needle: Symbol) -> bool {
    use Stmt::*;

    match stmt {
        Let(s, e, _, c) => {
            debug_assert_ne!(*s, needle);
            has_live_var_expr(e, needle) || has_live_var(jp_live_vars, c, needle)
        }
        Invoke {
            symbol,
            call,
            pass,
            fail,
            ..
        } => {
            debug_assert_ne!(*symbol, needle);

            has_live_var_call(call, needle)
                || has_live_var(jp_live_vars, pass, needle)
                || has_live_var(jp_live_vars, fail, needle)
        }
        Switch { cond_symbol, .. } if *cond_symbol == needle => true,
        Switch {
            branches,
            default_branch,
            ..
        } => {
            has_live_var(jp_live_vars, default_branch.1, needle)
                || branches
                    .iter()
                    .any(|(_, _, body)| has_live_var(jp_live_vars, body, needle))
        }
        Ret(s) => *s == needle,
        Refcounting(modify_rc, cont) => {
            modify_rc.get_symbol() == needle || has_live_var(jp_live_vars, cont, needle)
        }
        Join {
            id,
            parameters,
            body,
            remainder,
        } => {
            debug_assert!(parameters.iter().all(|p| p.symbol != needle));

            let mut jp_live_vars = jp_live_vars.clone();

            jp_live_vars.insert(*id, LiveVarSet::default());

            let body_live_vars = collect_stmt(body, &jp_live_vars, LiveVarSet::default());

            if body_live_vars.contains(&needle) {
                return true;
            }

            jp_live_vars.insert(*id, body_live_vars);

            has_live_var(&jp_live_vars, remainder, needle)
        }
        Jump(id, arguments) => {
            arguments.iter().any(|s| *s == needle) || jp_live_vars[id].contains(&needle)
        }
        Resume(_) | RuntimeError(_) => false,
    }
}

fn has_live_var_expr<'a>(expr: &'a Expr<'a>, needle: Symbol) -> bool {
    match expr {
        Expr::Literal(_) => false,
        Expr::Call(_) => todo!(),
        Expr::Array { elems: fields, .. }
        | Expr::Tag {
            arguments: fields, ..
        }
        | Expr::Struct(fields) => fields.iter().any(|s| *s == needle),
        Expr::StructAtIndex { structure, .. }
        | Expr::GetTagId { structure, .. }
        | Expr::UnionAtIndex { structure, .. } => *structure == needle,
        Expr::EmptyArray => false,
        Expr::Reuse { .. } => unreachable!("not introduced"),
        Expr::Reset(_) => unreachable!("not introduced"),
        Expr::RuntimeErrorFunction(_) => false,
    }
}

fn has_live_var_call<'a>(call: &'a Call<'a>, needle: Symbol) -> bool {
    call.arguments.iter().any(|s| *s == needle)
}
