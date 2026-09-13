// SPDX-License-Identifier: GPL-3.0-or-later
use crate::model::Track;

#[derive(Debug, Clone)]
pub enum SearchExpr {
    Term(String),
    Exact(String, String),
    Inequality(String, String, u32),
    Tag(String),
    Rating(u8, String),
    Duration(u32, String),
    And(Box<SearchExpr>, Box<SearchExpr>),
    Or(Box<SearchExpr>, Box<SearchExpr>),
    Not(Box<SearchExpr>),
}

impl SearchExpr {
    pub fn matches(&self, track: &Track) -> bool {
        match self {
            SearchExpr::Term(s) => {
                let s_lower = s.to_lowercase();
                track.title.to_lowercase().contains(&s_lower)
                    || track.artist.to_lowercase().contains(&s_lower)
                    || track.album.to_lowercase().contains(&s_lower)
            }
            SearchExpr::Exact(field, val) => {
                let v = val.to_lowercase();
                match field.as_str() {
                    "ar" | "artist" => track.artist.to_lowercase().contains(&v),
                    "al" | "album" => track.album.to_lowercase().contains(&v),
                    "t" | "title" => track.title.to_lowercase().contains(&v),
                    "c" | "comment" => track.comment.to_lowercase().contains(&v),
                    _ => false,
                }
            }
            SearchExpr::Tag(t) => track.genre.to_lowercase().contains(&t.to_lowercase()),
            SearchExpr::Rating(val, op) => match op.as_str() {
                ">" => track.rating > *val,
                "<" => track.rating < *val,
                ">=" => track.rating >= *val,
                "<=" => track.rating <= *val,
                _ => track.rating == *val,
            },
            SearchExpr::Inequality(field, op, val) => {
                let track_val = match field.as_str() {
                    "year" => track.year,
                    "plays" => track.play_count,
                    _ => return false,
                };
                match op.as_str() {
                    ">" => track_val > *val,
                    "<" => track_val < *val,
                    ">=" => track_val >= *val,
                    "<=" => track_val <= *val,
                    _ => track_val == *val,
                }
            }
            SearchExpr::Duration(val, op) => match op.as_str() {
                ">" => track.duration_secs > *val,
                "<" => track.duration_secs < *val,
                ">=" => track.duration_secs >= *val,
                "<=" => track.duration_secs <= *val,
                _ => track.duration_secs == *val,
            },
            SearchExpr::And(a, b) => a.matches(track) && b.matches(track),
            SearchExpr::Or(a, b) => a.matches(track) || b.matches(track),
            SearchExpr::Not(a) => !a.matches(track),
        }
    }
}

pub fn parse_query(input: &str) -> SearchExpr {
    let tokens = tokenize(input);
    let mut parser = Parser::new(tokens);
    parser.parse()
}

#[derive(Debug, PartialEq, Clone)]
enum Token {
    Text(String),
    Or,
    LParen,
    RParen,
    NotPrefix,
}

fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' => {
                chars.next();
            }
            '(' => {
                tokens.push(Token::LParen);
                chars.next();
            }
            ')' => {
                tokens.push(Token::RParen);
                chars.next();
            }
            '|' => {
                tokens.push(Token::Or);
                chars.next();
            }
            '-' => {
                chars.next();
                if let Some(&next_c) = chars.peek() {
                    if next_c.is_whitespace() || next_c == '(' || next_c == ')' || next_c == '|' {
                        tokens.push(Token::Text("-".to_string()));
                    } else {
                        tokens.push(Token::NotPrefix);
                    }
                } else {
                    tokens.push(Token::Text("-".to_string()));
                }
            }
            _ => {
                let mut term = String::new();
                let mut in_quote = false;
                let mut escaped = false;

                while let Some(&c) = chars.peek() {
                    if escaped {
                        term.push(c);
                        chars.next();
                        escaped = false;
                    } else if c == '\\' {
                        chars.next();
                        escaped = true;
                    } else if c == '"' {
                        in_quote = !in_quote;
                        chars.next();
                    } else if !in_quote && (c == ' ' || c == '(' || c == ')' || c == '|') {
                        break;
                    } else {
                        term.push(c);
                        chars.next();
                    }
                }
                if !term.is_empty() {
                    tokens.push(Token::Text(term));
                }
            }
        }
    }
    tokens
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn parse(&mut self) -> SearchExpr {
        if self.tokens.is_empty() {
            return SearchExpr::Term("".to_string());
        }
        self.parse_or()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }
    fn advance(&mut self) {
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
    }

    fn parse_or(&mut self) -> SearchExpr {
        let mut left = self.parse_and();
        while let Some(Token::Or) = self.peek() {
            self.advance();
            left = SearchExpr::Or(Box::new(left), Box::new(self.parse_and()));
        }
        left
    }

    fn parse_and(&mut self) -> SearchExpr {
        let mut left = self.parse_unary();
        while let Some(token) = self.peek() {
            if matches!(token, Token::Or | Token::RParen) {
                break;
            }
            left = SearchExpr::And(Box::new(left), Box::new(self.parse_unary()));
        }
        left
    }

    fn parse_unary(&mut self) -> SearchExpr {
        if let Some(Token::NotPrefix) = self.peek() {
            self.advance();
            return SearchExpr::Not(Box::new(self.parse_primary()));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> SearchExpr {
        match self.peek() {
            Some(Token::LParen) => {
                self.advance();
                let expr = self.parse_or();
                if let Some(Token::RParen) = self.peek() {
                    self.advance();
                }
                expr
            }
            Some(Token::Text(t)) => {
                let term = t.clone();
                self.advance();
                Self::parse_term(term)
            }
            _ => SearchExpr::Term("".to_string()),
        }
    }

    fn parse_term(term: String) -> SearchExpr {
        // Tag
        if let Some(stripped) = term.strip_prefix('#') {
            return SearchExpr::Tag(stripped.to_string());
        }
        // Rating
        if let Some(stripped) = term.strip_prefix('*') {
            let (op, val) = Self::extract_op_and_num(stripped);
            return SearchExpr::Rating(val as u8, op);
        }
        // Duration
        if let Some(stripped) = term.strip_prefix('~') {
            let (op, num_str) = Self::extract_op_and_str(stripped);
            // Quick duration parsing (e.g., "5m")
            let mut secs = 0;
            if num_str.ends_with('m') {
                secs = num_str.trim_end_matches('m').parse::<u32>().unwrap_or(0) * 60;
            } else if num_str.ends_with('s') {
                secs = num_str.trim_end_matches('s').parse::<u32>().unwrap_or(0);
            }
            return SearchExpr::Duration(secs, op);
        }
        // Prefixes (ar:, al:, t:, etc)
        if let Some((prefix, value)) = term.split_once(':') {
            let p = prefix.to_lowercase();
            if matches!(
                p.as_str(),
                "ar" | "artist" | "al" | "album" | "t" | "title" | "c" | "comment"
            ) {
                return SearchExpr::Exact(p, value.to_string());
            } else if p == "year" || p == "plays" {
                let (op, val) = Self::extract_op_and_num(value);
                return SearchExpr::Inequality(p, op, val);
            }
        }
        SearchExpr::Term(term)
    }

    fn extract_op_and_num(input: &str) -> (String, u32) {
        let (op, rem) = Self::extract_op_and_str(input);
        (op, rem.parse().unwrap_or(0))
    }

    fn extract_op_and_str(input: &str) -> (String, String) {
        if let Some(s) = input.strip_prefix(">=") {
            return (">=".to_string(), s.to_string());
        }
        if let Some(s) = input.strip_prefix("<=") {
            return ("<=".to_string(), s.to_string());
        }
        if let Some(s) = input.strip_prefix(">") {
            return (">".to_string(), s.to_string());
        }
        if let Some(s) = input.strip_prefix("<") {
            return ("<".to_string(), s.to_string());
        }
        ("=".to_string(), input.to_string())
    }
}
