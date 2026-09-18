use std::mem;

use ruff_python_ast::token::Token;
use ruff_python_ast::token::TokenKind;
use ruff_python_parser::lexer::lex;
use ruff_python_parser::Mode;

use super::step::Step;
use super::Input;
use super::Output;
use crate::SyntaxKind;

pub enum StrStep<'a> {
    Start { kind: SyntaxKind },
    Finish,
    Token { kind: SyntaxKind, text: &'a str },
    Error { message: String, pos: usize },
}

pub struct StrWithTokens<'a> {
    input: &'a str,
    token_kinds: Vec<SyntaxKind>,
    token_start: Vec<u32>,
}

impl<'a> StrWithTokens<'a> {
    pub fn new(input: &'a str) -> Self {
        let mut lexer = lex(input, Mode::Expression);
        let mut token_kinds = Vec::new();
        let mut token_start = Vec::new();
        let mut end = 0;
        loop {
            let kind = lexer.next_token();
            if kind == TokenKind::EndOfFile {
                break;
            }
            let range = lexer.current_range();
            if range.is_empty() {
                continue;
            }
            let start = u32::from(range.start());
            if end < start {
                token_kinds.push(SyntaxKind::WHITESPACE);
                token_start.push(end);
            }
            let text = &input[range];
            let token = Token::new(kind, range, lexer.current_flags());
            token_kinds.push(if text == "ignore" {
                SyntaxKind::IGNORE
            } else {
                token_kind(token, text)
            });
            token_start.push(start);
            end = u32::from(range.end());
        }
        if end < input.len() as u32 {
            token_kinds.push(SyntaxKind::WHITESPACE);
            token_start.push(end);
        }
        token_start.push(input.len() as u32);
        Self {
            input,
            token_kinds,
            token_start,
        }
    }

    pub fn to_input(&self) -> Input {
        Input {
            tokens: self
                .token_kinds
                .iter()
                .filter(|kind| !kind.is_trivia_token())
                .cloned()
                .collect(),
        }
    }

    pub fn build_with_trivia(&self, output: Output, sink: &mut dyn FnMut(StrStep<'_>)) {
        let mut builder = Builder {
            state: BuilderState::Init,
            str_with_tokens: self,
            sink,
            pos: 0,
        };

        // Defer to the builder to handle each step type.
        for step in output.steps {
            match step {
                Step::Start { kind } => builder.start(kind),
                Step::Finish => builder.finish(),
                Step::Token { kind } => builder.token(kind),
                Step::Error { message } => builder.error(message),
            }
        }

        // The builder defers its last finish step to be manually handled by us here.
        // Consume any remaining trivia tokens, then finally emit the "finish" step.
        match builder.state {
            BuilderState::PendingFinish => {
                builder.eat_trivia_tokens();
                (builder.sink)(StrStep::Finish)
            }
            BuilderState::Init | BuilderState::Normal => unreachable!(),
        }
    }

    pub fn kind(&self, pos: usize) -> SyntaxKind {
        self.token_kinds[pos]
    }

    pub fn token_text(&self, pos: usize) -> &str {
        &self.input[self.token_start[pos] as usize..self.token_start[pos + 1] as usize]
    }

    pub fn token_pos(&self, pos: usize) -> u32 {
        self.token_start[pos]
    }

    pub fn len(&self) -> usize {
        self.token_kinds.len()
    }
}

enum BuilderState {
    Init,
    Normal,
    PendingFinish,
}

struct Builder<'a, 'b> {
    state: BuilderState,
    str_with_tokens: &'a StrWithTokens<'a>,
    sink: &'b mut dyn FnMut(StrStep<'_>),
    pos: usize,
}

impl Builder<'_, '_> {
    fn start(&mut self, kind: SyntaxKind) {
        match mem::replace(&mut self.state, BuilderState::Normal) {
            BuilderState::Init => {
                (self.sink)(StrStep::Start { kind });
                return;
            }
            BuilderState::Normal => (),
            BuilderState::PendingFinish => (self.sink)(StrStep::Finish),
        }
        self.eat_trivia_tokens();
        (self.sink)(StrStep::Start { kind })
    }

    fn finish(&mut self) {
        match mem::replace(&mut self.state, BuilderState::PendingFinish) {
            BuilderState::Init => unreachable!(),
            BuilderState::Normal => (),
            BuilderState::PendingFinish => (self.sink)(StrStep::Finish),
        }
    }

    fn token(&mut self, kind: SyntaxKind) {
        match mem::replace(&mut self.state, BuilderState::Normal) {
            BuilderState::Init => unreachable!(),
            BuilderState::Normal => (),
            BuilderState::PendingFinish => (self.sink)(StrStep::Finish),
        }
        self.eat_trivia_tokens();
        self.do_token(kind)
    }

    fn error(&mut self, message: String) {
        (self.sink)(StrStep::Error {
            message,
            pos: self.pos,
        })
    }

    fn eat_trivia_tokens(&mut self) {
        while self.pos < self.str_with_tokens.len() {
            let kind = self.str_with_tokens.kind(self.pos);
            if !kind.is_trivia_token() {
                break;
            }
            self.do_token(kind);
        }
    }

    fn do_token(&mut self, kind: SyntaxKind) {
        let text = self.str_with_tokens.token_text(self.pos);
        (self.sink)(StrStep::Token { kind, text });
        self.pos += 1;
    }
}

fn token_kind(token: Token, text: &str) -> SyntaxKind {
    use TokenKind as T;

    use crate::SyntaxKind::*;
    match token.kind() {
        T::Identifier => {
            if text == "load" {
                LOAD
            } else {
                IDENT
            }
        }
        T::Int => INT,
        T::Float => FLOAT,
        T::String => {
            if token.unwrap_string_flags().is_byte_string() {
                BYTES
            } else {
                STRING
            }
        }
        T::Comment => COMMENT,
        T::Newline => NEWLINE,
        T::NonLogicalNewline => WHITESPACE,
        T::Indent => WHITESPACE,
        T::Dedent => WHITESPACE,
        T::Lpar => OPEN_PAREN,
        T::Rpar => CLOSE_PAREN,
        T::Lsqb => OPEN_BRACK,
        T::Rsqb => CLOSE_BRACK,
        T::Lbrace => OPEN_BRACE,
        T::Rbrace => CLOSE_BRACE,
        T::Colon => COLON,
        T::Comma => COMMA,
        T::Semi => SEMI,
        T::Plus => PLUS,
        T::Minus => MINUS,
        T::Star => STAR,
        T::Slash => SLASH,
        T::Vbar => BAR,
        T::Amper => AMPERSAND,
        T::Less => LT,
        T::Greater => GT,
        T::Equal => EQ,
        T::Dot => DOT,
        T::Percent => MOD,
        T::EqEqual => EQ_EQ,
        T::NotEqual => BANG_EQ,
        T::LessEqual => LE,
        T::GreaterEqual => GE,
        T::Tilde => TILDE,
        T::CircumFlex => CARET,
        T::LeftShift => LT_LT,
        T::RightShift => GT_GT,
        T::DoubleStar => STAR_STAR,
        T::PlusEqual => PLUS_EQ,
        T::MinusEqual => MINUS_EQ,
        T::StarEqual => STAR_EQ,
        T::SlashEqual => SLASH_EQ,
        T::PercentEqual => MOD_EQ,
        T::AmperEqual => AMPERSAND_EQ,
        T::VbarEqual => BAR_EQ,
        T::CircumflexEqual => CARET_EQ,
        T::LeftShiftEqual => LT_LT_EQ,
        T::RightShiftEqual => GT_GT_EQ,
        T::DoubleSlash => SLASH_SLASH,
        T::DoubleSlashEqual => SLASH_SLASH_EQ,
        T::Rarrow => ARROW,
        T::Ellipsis => ELLIPSIS,
        T::And => AND,
        T::As => AS,
        T::Assert => ASSERT,
        T::Async => ASYNC,
        T::Await => AWAIT,
        T::Break => BREAK,
        T::Class => CLASS,
        T::Continue => CONTINUE,
        T::Def => DEF,
        T::Del => DEL,
        T::Elif => ELIF,
        T::Else => ELSE,
        T::Except => EXCEPT,
        T::False => FALSE,
        T::Finally => FINALLY,
        T::For => FOR,
        T::From => FROM,
        T::Global => GLOBAL,
        T::If => IF,
        T::Import => IMPORT,
        T::In => IN,
        T::Is => IS,
        T::Lambda => LAMBDA,
        T::None => NONE,
        T::Nonlocal => NONLOCAL,
        T::Not => NOT,
        T::Or => OR,
        T::Pass => PASS,
        T::Raise => RAISE,
        T::Return => RETURN,
        T::True => TRUE,
        T::Try => TRY,
        T::While => WHILE,
        T::With => WITH,
        T::Yield => YIELD,
        T::Case => IDENT,
        T::Lazy => IDENT,
        T::Match => IDENT,
        T::Type => IDENT,
        _ => ERROR,
    }
}
