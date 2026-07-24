use crate::ast::*;
use crate::codegen::translator::{Ctx, Translator};
use crate::codegen::yul::*;

impl<'a> Translator<'a> {
    pub(crate) fn binary_op_meta(
        &self,
        left: &Expr,
        right: &Expr,
        ctx: &Ctx,
    ) -> Option<(usize, bool)> {
        let order: [&Expr; 2] =
            if matches!(left, Expr::Number(_)) && !matches!(right, Expr::Number(_)) {
                [right, left]
            } else {
                [left, right]
            };
        for e in order {
            if let Type::Primitive(name) = self.static_type(e, ctx) {
                if let Some(m) = numeric_meta(&name) {
                    return Some(m);
                }
            }
        }
        None
    }

    pub(crate) fn max_value_hex(bits: usize) -> String {
        if bits == 256 {
            "not(0)".to_string()
        } else {
            mask_hex(bits / 8)
        }
    }

    pub(crate) fn max_signed_hex(bits: usize) -> String {
        let n = bits / 8;
        format!("0x{}{}{}", "00".repeat(32 - n), "7f", "ff".repeat(n - 1))
    }

    pub(crate) fn min_signed_hex(bits: usize) -> String {
        let n = bits / 8;
        format!("0x{}{}{}", "ff".repeat(32 - n), "80", "00".repeat(n - 1))
    }

    pub(crate) fn const_fold(
        &self,
        left: &Expr,
        right: &Expr,
        op: &str,
        meta: Option<(usize, bool)>,
    ) -> Option<String> {
        let (bits, signed) = meta?;
        if signed {
            return None;
        }
        let a = literal_u128(left)?;
        let b = literal_u128(right)?;

        let result: u128 = match op {
            "+" => a.checked_add(b)?,

            "-" => a.checked_sub(b)?,
            "*" => a.checked_mul(b)?,
            _ => return None,
        };

        let fits = if bits >= 128 {
            true
        } else {
            result <= ((1u128 << bits) - 1)
        };
        if !fits {
            return None;
        }

        Some(result.to_string())
    }

    pub(crate) fn translate_binary_op(
        &self,
        left: &Expr,
        operator: &str,
        right: &Expr,
        ctx: &Ctx,
    ) -> String {
        let l = self.translate_expr(left, ctx);
        let r = self.translate_expr(right, ctx);

        if matches!(operator, "==" | "!=")
            && is_str_type(&self.static_type(left, ctx))
            && is_str_type(&self.static_type(right, ctx))
        {
            self.ensure_helper("gum_str_len", gum_str_len_helper_src);
            self.ensure_helper("gum_str_eq", gum_str_eq_helper_src);
            let eq = format!("gum_str_eq({}, {})", l, r);
            return if operator == "==" {
                eq
            } else {
                format!("iszero({})", eq)
            };
        }

        let meta = self.binary_op_meta(left, right, ctx);
        let signed = meta.map(|(_, s)| s).unwrap_or(false);

        if matches!(operator, "+" | "-" | "*") {
            if let Some(folded) = self.const_fold(left, right, operator, meta) {
                return folded;
            }
        }

        let rich = self.rich_reverts;

        if matches!(operator, "*" | "/")
            && is_fixed_point(&self.static_type(left, ctx))
            && is_fixed_point(&self.static_type(right, ctx))
        {
            self.ensure_helper("gum_muldiv", || gum_muldiv_helper_src(rich));
            let (lo, hi) = (Self::min_signed_hex(256), Self::max_signed_hex(256));
            if operator == "*" {
                self.ensure_helper("gum_wad_mul", || gum_wad_mul_helper_src(rich));
                return format!("gum_wad_mul({}, {}, {}, {})", l, r, lo, hi);
            }
            self.ensure_helper("gum_wad_div", || gum_wad_div_helper_src(rich));
            return format!("gum_wad_div({}, {}, {}, {})", l, r, lo, hi);
        }
        match operator {
            "+" => match meta {
                Some((bits, true)) => {
                    self.ensure_helper("checked_sadd", || checked_sadd_helper_src(rich));
                    format!(
                        "checked_sadd({}, {}, {}, {})",
                        l,
                        r,
                        Self::min_signed_hex(bits),
                        Self::max_signed_hex(bits)
                    )
                }
                Some((bits, false)) => {
                    self.ensure_helper("checked_add", || checked_add_helper_src(rich));
                    format!("checked_add({}, {}, {})", l, r, Self::max_value_hex(bits))
                }
                None => format!("add({}, {})", l, r),
            },
            "-" => match meta {
                Some((256, true)) => {
                    self.ensure_helper("checked_ssub", || checked_ssub_helper_src(rich));
                    format!("checked_ssub({}, {})", l, r)
                }
                Some((bits, true)) => {
                    self.ensure_helper("checked_ssub", || checked_ssub_helper_src(rich));
                    self.ensure_helper("checked_ssub_n", || checked_ssub_n_helper_src(rich));
                    format!(
                        "checked_ssub_n({}, {}, {}, {})",
                        l,
                        r,
                        Self::min_signed_hex(bits),
                        Self::max_signed_hex(bits)
                    )
                }
                _ => {
                    self.ensure_helper("checked_sub", || checked_sub_helper_src(rich));
                    format!("checked_sub({}, {})", l, r)
                }
            },
            "*" => match meta {
                Some((bits, true)) => {
                    self.ensure_helper("checked_smul", || checked_smul_helper_src(rich));
                    format!(
                        "checked_smul({}, {}, {}, {})",
                        l,
                        r,
                        Self::min_signed_hex(bits),
                        Self::max_signed_hex(bits)
                    )
                }
                Some((bits, false)) => {
                    self.ensure_helper("checked_mul", || checked_mul_helper_src(rich));
                    format!("checked_mul({}, {}, {})", l, r, Self::max_value_hex(bits))
                }
                None => format!("mul({}, {})", l, r),
            },
            "/" => {
                if signed {
                    self.ensure_helper("checked_sdiv", || checked_sdiv_helper_src(rich));
                    format!("checked_sdiv({}, {})", l, r)
                } else {
                    self.ensure_helper("checked_div", || checked_div_helper_src(rich));
                    format!("checked_div({}, {})", l, r)
                }
            }
            "%" => {
                if signed {
                    self.ensure_helper("checked_smod", || checked_smod_helper_src(rich));
                    format!("checked_smod({}, {})", l, r)
                } else {
                    self.ensure_helper("checked_mod", || checked_mod_helper_src(rich));
                    format!("checked_mod({}, {})", l, r)
                }
            }
            "**" => format!("exp({}, {})", l, r),
            "==" => format!("eq({}, {})", l, r),
            "!=" => format!("iszero(eq({}, {}))", l, r),
            "<" => {
                if signed {
                    format!("slt({}, {})", l, r)
                } else {
                    format!("lt({}, {})", l, r)
                }
            }
            ">" => {
                if signed {
                    format!("sgt({}, {})", l, r)
                } else {
                    format!("gt({}, {})", l, r)
                }
            }
            "<=" => {
                if signed {
                    format!("iszero(sgt({}, {}))", l, r)
                } else {
                    format!("iszero(gt({}, {}))", l, r)
                }
            }
            ">=" => {
                if signed {
                    format!("iszero(slt({}, {}))", l, r)
                } else {
                    format!("iszero(lt({}, {}))", l, r)
                }
            }
            "&&" => format!("and({}, {})", l, r),
            "||" => format!("or({}, {})", l, r),
            _ => format!("/* unsupported op {} */", operator),
        }
    }
}
