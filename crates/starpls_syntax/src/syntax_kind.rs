use self::SyntaxKind::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(non_camel_case_types)]
#[repr(u16)]
pub enum SyntaxKind {
    // Tokens.
    ERROR,
    EOF,
    COMMENT,
    NEWLINE,
    WHITESPACE,
    INDENT,
    DEDENT,
    IDENT,
    INT,
    FLOAT,
    STRING,
    BYTES,
    TRUE,
    FALSE,
    NONE,
    AND,
    BREAK,
    CONTINUE,
    DEF,
    ELIF,
    ELSE,
    FOR,
    IF,
    IN,
    LAMBDA,
    LOAD,
    NOT,
    OR,
    PASS,
    RETURN,
    AS,
    ASSERT,
    ASYNC,
    AWAIT,
    CLASS,
    DEL,
    EXCEPT,
    FINALLY,
    FROM,
    GLOBAL,
    IGNORE,
    IMPORT,
    IS,
    NONLOCAL,
    RAISE,
    TRY,
    WHILE,
    WITH,
    YIELD,
    PLUS,
    MINUS,
    STAR,
    SLASH,
    SLASH_SLASH,
    MOD,
    STAR_STAR,
    TILDE,
    AMPERSAND,
    BAR,
    CARET,
    LT_LT,
    GT_GT,
    DOT,
    COMMA,
    EQ,
    SEMI,
    COLON,
    OPEN_PAREN,
    CLOSE_PAREN,
    OPEN_BRACK,
    CLOSE_BRACK,
    OPEN_BRACE,
    CLOSE_BRACE,
    LT,
    GT,
    GE,
    LE,
    EQ_EQ,
    BANG,
    BANG_EQ,
    PLUS_EQ,
    MINUS_EQ,
    STAR_EQ,
    SLASH_EQ,
    SLASH_SLASH_EQ,
    MOD_EQ,
    AMPERSAND_EQ,
    BAR_EQ,
    CARET_EQ,
    LT_LT_EQ,
    GT_GT_EQ,
    ARROW,
    ELLIPSIS,

    // Types.
    NONE_TYPE,
    UNION_TYPE,    // int | None
    ELLIPSIS_TYPE, // ...

    PATH_TYPE,         // tuple[int, int, string], java_common.JavaRuntimeInfo
    GENERIC_ARGUMENTS, // [int, int, string] in the type above
    PATH_SEGMENT,

    IGNORE_TYPE,
    FUNCTION_TYPE,              // e.g. (int, int) -> int
    PARAMETER_TYPES,            // the (int, int) in the signature above
    SIMPLE_PARAMETER_TYPE,      // int
    ARGS_LIST_PARAMETER_TYPE,   // *int
    KWARGS_DICT_PARAMETER_TYPE, // **int

    TYPE_COMMENT,
    TYPE_COMMENT_PREFIX,
    TYPE_COMMENT_BODY,
    TYPE_LIST,
}

#[macro_export]
macro_rules! T {
    [ident] => { $ crate :: SyntaxKind :: IDENT };
    ['('] => { $ crate :: SyntaxKind :: OPEN_PAREN };
    [')'] => { $ crate :: SyntaxKind :: CLOSE_PAREN };
    ['['] => { $ crate :: SyntaxKind :: OPEN_BRACK };
    [']'] => { $ crate :: SyntaxKind :: CLOSE_BRACK };
    ['{'] => { $ crate :: SyntaxKind :: OPEN_BRACE };
    ['}'] => { $ crate :: SyntaxKind :: CLOSE_BRACE };
    [if] => { $ crate :: SyntaxKind :: IF };
    [elif] => { $ crate :: SyntaxKind :: ELIF };
    [else] => { $ crate :: SyntaxKind :: ELSE };
    [+] => { $ crate :: SyntaxKind :: PLUS };
    [-] => { $ crate :: SyntaxKind :: MINUS };
    [~] => { $ crate :: SyntaxKind :: TILDE };
    [not] => { $ crate :: SyntaxKind :: NOT };
    [lambda] => { $ crate :: SyntaxKind :: LAMBDA };
    [return] => { $ crate :: SyntaxKind :: RETURN };
    [break] => { $ crate :: SyntaxKind :: BREAK };
    [continue] => { $ crate :: SyntaxKind :: CONTINUE };
    [pass] => { $ crate :: SyntaxKind :: PASS };
    [load] => { $ crate :: SyntaxKind :: LOAD };
    [def] => { $ crate :: SyntaxKind :: DEF };
    [for] => { $ crate :: SyntaxKind :: FOR };
    ['\n'] => { $ crate :: SyntaxKind :: NEWLINE };
    [;] => { $ crate :: SyntaxKind :: SEMI };
    [or] => { $ crate :: SyntaxKind :: OR };
    [and] => { $ crate :: SyntaxKind :: AND };
    [==] => { $ crate :: SyntaxKind :: EQ_EQ };
    [!=] => { $ crate :: SyntaxKind :: BANG_EQ };
    [<] => { $ crate :: SyntaxKind :: LT };
    [>] => { $ crate :: SyntaxKind :: GT };
    [<=] => { $ crate :: SyntaxKind :: LE };
    [>=] => { $ crate :: SyntaxKind :: GE };
    [in] => { $ crate :: SyntaxKind :: IN };
    [|] => { $ crate :: SyntaxKind :: BAR };
    [^] => { $ crate :: SyntaxKind :: CARET };
    [&] => { $ crate :: SyntaxKind :: AMPERSAND };
    [<<] => { $ crate :: SyntaxKind :: LT_LT };
    [>>] => { $ crate :: SyntaxKind :: GT_GT };
    [*] => { $ crate :: SyntaxKind :: STAR };
    [**] => { $ crate :: SyntaxKind :: STAR_STAR };
    [/] => { $ crate :: SyntaxKind :: SLASH };
    ["//"] => { $ crate :: SyntaxKind :: SLASH_SLASH };
    [%] => { $ crate :: SyntaxKind :: MOD };
    [True] => { $ crate :: SyntaxKind :: TRUE };
    [False] => { $ crate :: SyntaxKind :: FALSE };
    [None] => { $ crate :: SyntaxKind :: NONE };
    [.] => { $ crate :: SyntaxKind :: DOT };
    [,] => { $ crate :: SyntaxKind :: COMMA };
    [=] => { $ crate :: SyntaxKind :: EQ };
    [+=] => { $ crate :: SyntaxKind :: PLUS_EQ };
    [-=] => { $ crate :: SyntaxKind :: MINUS_EQ };
    [*=] => { $ crate :: SyntaxKind :: STAR_EQ };
    [/=] => { $ crate :: SyntaxKind :: SLASH_EQ };
    ["//="] => { $ crate :: SyntaxKind :: SLASH_SLASH_EQ };
    [%=] => { $ crate :: SyntaxKind :: MOD_EQ };
    [&=] => { $ crate :: SyntaxKind :: AMPERSAND_EQ };
    [|=] => { $ crate :: SyntaxKind :: BAR_EQ };
    [^=] => { $ crate :: SyntaxKind :: CARET_EQ };
    [<<=] => { $ crate :: SyntaxKind :: LT_LT_EQ };
    [>>=] => { $ crate :: SyntaxKind :: GT_GT_EQ };
    [:] => { $ crate :: SyntaxKind :: COLON };
    [ignore] => { $ crate :: SyntaxKind :: IGNORE };
}

impl SyntaxKind {
    pub fn is_trivia_token(&self) -> bool {
        matches!(*self, WHITESPACE | COMMENT)
    }

    pub fn is_keyword(&self) -> bool {
        matches!(
            *self,
            AND | BREAK
                | CONTINUE
                | DEF
                | ELIF
                | ELSE
                | FOR
                | IF
                | IN
                | LAMBDA
                | LOAD
                | NOT
                | OR
                | PASS
                | RETURN
        )
    }
}

impl From<u16> for SyntaxKind {
    #[inline]
    fn from(value: u16) -> Self {
        assert!(value <= TYPE_LIST as u16);
        unsafe { std::mem::transmute(value) }
    }
}

impl From<SyntaxKind> for u16 {
    #[inline]
    fn from(kind: SyntaxKind) -> Self {
        kind as u16
    }
}

/// A bitset of `SyntaxKind`s. Only `SyntaxKind`s corresponding to lexer tokens should be added to a `SyntaxKindSet`.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SyntaxKindSet(u128);

impl SyntaxKindSet {
    pub const fn new(kinds: &[SyntaxKind]) -> SyntaxKindSet {
        let mut inner = 0;
        let mut i = 0;
        while i < kinds.len() {
            inner |= 1 << kinds[i] as u16;
            i += 1;
        }
        SyntaxKindSet(inner)
    }

    pub const fn contains(&self, kind: SyntaxKind) -> bool {
        self.0 & 1 << kind as usize > 0
    }

    pub const fn union(&self, other: SyntaxKindSet) -> SyntaxKindSet {
        SyntaxKindSet(self.0 | other.0)
    }
}
