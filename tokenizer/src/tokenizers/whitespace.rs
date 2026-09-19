// Copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use crate::{Token, Tokenizer};

pub struct Whitespace;

pub struct WhitespaceIter<'a> {
    inner: std::str::SplitWhitespace<'a>,
    pos: u32,
}

impl<'a> WhitespaceIter<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            inner: text.split_whitespace(),
            pos: 0,
        }
    }
}

impl Tokenizer for Whitespace {
    type Iter<'tokenizer, 'text>
        = WhitespaceIter<'text>
    where
        Self: 'tokenizer;

    fn tokenize<'tokenizer, 'text>(
        &'tokenizer self,
        text: &'text str,
    ) -> Self::Iter<'tokenizer, 'text> {
        WhitespaceIter::new(text)
    }
}

impl<'a> Iterator for WhitespaceIter<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let word = self.inner.next()?;
        let pos = self.pos;
        self.pos += 1;
        Some(Token::new(word, pos))
    }
}

#[cfg(test)]
mod tests {
    use crate::tokenizers::Whitespace;
    use crate::{Classification, Token, Tokenizer};

    #[test]
    fn test_whitespace() {
        let whitespace = Whitespace;
        let mut tokenizer = whitespace.tokenize("Hello, world!");
        assert_eq!(
            tokenizer.next().unwrap(),
            Token::with_classification("Hello,", 0, Classification::Unknown)
        );
        assert_eq!(
            tokenizer.next().unwrap(),
            Token::with_classification("world!", 1, Classification::Unknown)
        );
        assert_eq!(tokenizer.next(), None);
    }
}
