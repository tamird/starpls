//! Operators represented by Starlark HIR.

use std::fmt::Write;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Arith(ArithOp),
    Bitwise(BitwiseOp),
    Cmp(CmpOp),
    Logic(LogicOp),
    MemberOp(MemberOp),
}

impl std::fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinaryOp::Arith(op) => std::fmt::Display::fmt(op, f),
            BinaryOp::Bitwise(op) => std::fmt::Display::fmt(op, f),
            BinaryOp::Cmp(op) => std::fmt::Display::fmt(op, f),
            BinaryOp::Logic(op) => std::fmt::Display::fmt(op, f),
            BinaryOp::MemberOp(op) => std::fmt::Display::fmt(op, f),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Flr,
    Mod,
}

impl std::fmt::Display for ArithOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
            ArithOp::Flr => "//",
            ArithOp::Mod => "%",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitwiseOp {
    And,
    Or,
    Xor,
    Shl,
    Shr,
}

impl std::fmt::Display for BitwiseOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BitwiseOp::And => "&",
            BitwiseOp::Or => "|",
            BitwiseOp::Xor => "^",
            BitwiseOp::Shl => "<<",
            BitwiseOp::Shr => ">>",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl std::fmt::Display for CmpOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Gt => ">",
            CmpOp::Le => "<=",
            CmpOp::Ge => ">=",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicOp {
    And,
    Or,
}

impl std::fmt::Display for LogicOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LogicOp::And => "and",
            LogicOp::Or => "or",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberOp {
    In,
    NotIn,
}

impl std::fmt::Display for MemberOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MemberOp::In => "in",
            MemberOp::NotIn => "not in",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Normal,
    Arith(ArithAssignOp),
    Bitwise(BitwiseAssignOp),
}

impl std::fmt::Display for AssignOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssignOp::Normal => f.write_char('='),
            AssignOp::Arith(op) => std::fmt::Display::fmt(op, f),
            AssignOp::Bitwise(op) => std::fmt::Display::fmt(op, f),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithAssignOp {
    Add,
    Sub,
    Mul,
    Div,
    Flr,
    Mod,
}

impl std::fmt::Display for ArithAssignOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ArithAssignOp::Add => "+=",
            ArithAssignOp::Sub => "-=",
            ArithAssignOp::Mul => "*=",
            ArithAssignOp::Div => "/=",
            ArithAssignOp::Flr => "//=",
            ArithAssignOp::Mod => "%=",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitwiseAssignOp {
    And,
    Or,
    Shl,
    Shr,
    Xor,
}

impl std::fmt::Display for BitwiseAssignOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BitwiseAssignOp::And => "&=",
            BitwiseAssignOp::Or => "|=",
            BitwiseAssignOp::Shl => "<<=",
            BitwiseAssignOp::Shr => ">>=",
            BitwiseAssignOp::Xor => "^=",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    Arith(UnaryArithOp),
    Inv,
    Not,
}

impl std::fmt::Display for UnaryOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnaryOp::Arith(op) => std::fmt::Display::fmt(op, f),
            UnaryOp::Inv => f.write_char('~'),
            UnaryOp::Not => f.write_char('!'),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnaryArithOp {
    Add,
    Sub,
}

impl std::fmt::Display for UnaryArithOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_char(match self {
            UnaryArithOp::Add => '+',
            UnaryArithOp::Sub => '-',
        })
    }
}
