//! One document as a single record for a mutable write buffer.
//!
//! An insert appends one record, so it needs one WAL record and one lock,
//! regardless of how many terms the document has. Folding a buffer into an
//! immutable segment reads records back and sorts by term. A query over the
//! buffer evaluates each record with the exact evaluator, since positions and
//! the document length are both present.
//!
//! ```text
//! record := len varint, block varint, offset varint, doc_len varint,
//!           term_count varint, term*
//! term   := shared varint, suffix_len varint, suffix, n varint, position varint * n
//!           positions: first absolute, then (delta - 1); terms sorted, unique
//! ```

use crate::payload::{decode_positions, encode_positions, validate_positions};
use crate::reader::Reader;
use crate::{Error, Result, Tid, varint};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardTerm {
    pub term: String,
    pub positions: Vec<u32>,
}

/// The fixed part of a record, from [`ForwardRecord::decode_with`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordHeader {
    pub tid: Tid,
    pub doc_len: u32,
    pub term_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardRecord {
    pub tid: Tid,
    /// Document length in tokens, as the evaluator defines it.
    pub doc_len: u32,
    /// Sorted by term bytes, unique.
    pub terms: Vec<ForwardTerm>,
}

impl ForwardRecord {
    /// Groups a token stream by term. `tokens` are `(term, position)` in
    /// document order; positions must be strictly increasing.
    pub fn from_tokens<'t>(
        tid: Tid,
        tokens: impl IntoIterator<Item = (&'t str, u32)>,
    ) -> Result<Self> {
        let mut by_term = std::collections::BTreeMap::<&str, Vec<u32>>::new();
        let mut doc_len = 0u32;
        let mut last_position = None;
        for (term, position) in tokens {
            if term.is_empty() {
                return Err(Error::EmptyTerm);
            }
            if last_position.is_some_and(|last| last >= position) {
                return Err(Error::InvalidPositions);
            }
            last_position = Some(position);
            doc_len += 1;
            by_term.entry(term).or_default().push(position);
        }
        Ok(Self {
            tid,
            doc_len,
            terms: by_term
                .into_iter()
                .map(|(term, positions)| ForwardTerm {
                    term: term.to_owned(),
                    positions,
                })
                .collect(),
        })
    }

    fn validate(&self) -> Result<()> {
        Tid::new(self.tid.block, self.tid.offset)?;
        let mut previous: Option<&[u8]> = None;
        for term in &self.terms {
            if term.term.is_empty() {
                return Err(Error::EmptyTerm);
            }
            if previous.is_some_and(|p| p >= term.term.as_bytes()) {
                return Err(Error::Unordered);
            }
            validate_positions(&term.positions)?;
            previous = Some(term.term.as_bytes());
        }
        Ok(())
    }

    /// Appends the encoded record, including its length prefix.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        self.validate()?;
        let mut body = Vec::new();
        varint::put(&mut body, u64::from(self.tid.block));
        varint::put(&mut body, u64::from(self.tid.offset));
        varint::put(&mut body, u64::from(self.doc_len));
        varint::put(&mut body, self.terms.len() as u64);
        let mut previous: &[u8] = &[];
        for term in &self.terms {
            let bytes = term.term.as_bytes();
            let shared = previous
                .iter()
                .zip(bytes)
                .take_while(|(a, b)| a == b)
                .count();
            varint::put(&mut body, shared as u64);
            varint::put(&mut body, (bytes.len() - shared) as u64);
            body.extend_from_slice(&bytes[shared..]);
            encode_positions(&mut body, &term.positions);
            previous = bytes;
        }
        varint::put(out, body.len() as u64);
        out.extend_from_slice(&body);
        Ok(())
    }

    /// Byte length of the record at the start of `bytes`, so a buffer page can
    /// skip records without decoding them.
    pub fn encoded_len(bytes: &[u8]) -> Result<usize> {
        let mut reader = Reader::new(bytes);
        let len = reader.varint()? as usize;
        let total = reader
            .position()
            .checked_add(len)
            .filter(|total| *total <= bytes.len())
            .ok_or(Error::Truncated)?;
        Ok(total)
    }

    /// Decodes the record at the start of `bytes`, returning it and the bytes
    /// consumed.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize)> {
        let mut terms = Vec::new();
        let (header, total) = Self::decode_with(bytes, |term, positions| {
            terms.push(ForwardTerm {
                term: term.to_owned(),
                positions: positions.to_vec(),
            });
            Ok(())
        })?;
        Ok((
            Self {
                tid: header.tid,
                doc_len: header.doc_len,
                terms,
            },
            total,
        ))
    }

    /// Reads only the fixed part of the record at the start of `bytes`.
    pub fn peek(bytes: &[u8]) -> Result<RecordHeader> {
        let mut reader = Reader::new(bytes);
        let len = reader.varint()? as usize;
        let mut body = Reader::new(reader.take(len)?);
        let block = body.varint_u32()?;
        let offset = u16::try_from(body.varint_u32()?).map_err(|_| Error::InvalidTid)?;
        Ok(RecordHeader {
            tid: Tid::new(block, offset)?,
            doc_len: body.varint_u32()?,
            term_count: body.varint_u32()?,
        })
    }

    /// Decodes the record at the start of `bytes` without building it,
    /// handing each term and its positions to `visit` in term order. Returns
    /// the header and the bytes consumed.
    pub fn decode_with(
        bytes: &[u8],
        mut visit: impl FnMut(&str, &[u32]) -> Result<()>,
    ) -> Result<(RecordHeader, usize)> {
        let total = Self::encoded_len(bytes)?;
        let mut reader = Reader::new(bytes);
        let len = reader.varint()? as usize;
        let mut body = Reader::new(reader.take(len)?);
        let block = body.varint_u32()?;
        let offset = u16::try_from(body.varint_u32()?).map_err(|_| Error::InvalidTid)?;
        let tid = Tid::new(block, offset)?;
        let doc_len = body.varint_u32()?;
        let term_count = body.varint_u32()?;
        let mut term: Vec<u8> = Vec::new();
        let mut positions: Vec<u32> = Vec::new();
        for _ in 0..term_count {
            let shared = body.varint_u32()? as usize;
            if shared > term.len() {
                return Err(Error::Corrupt("forward term prefix"));
            }
            let suffix_len = body.varint_u32()? as usize;
            let suffix = body.take(suffix_len)?;
            // The new term is previous[..shared] + suffix; it follows the
            // previous term exactly when the suffix exceeds the rest of it.
            if !term.is_empty() && term[shared..] >= *suffix {
                return Err(Error::Corrupt("forward term order"));
            }
            term.truncate(shared);
            term.extend_from_slice(suffix);
            if term.is_empty() {
                return Err(Error::Corrupt("forward term order"));
            }
            positions.clear();
            decode_positions(&mut body, &mut positions)?;
            let text =
                std::str::from_utf8(&term).map_err(|_| Error::Corrupt("forward term UTF-8"))?;
            visit(text, &positions)?;
        }
        if body.remaining() != 0 {
            return Err(Error::Corrupt("forward record length"));
        }
        Ok((
            RecordHeader {
                tid,
                doc_len,
                term_count,
            },
            total,
        ))
    }

    /// Positions of every token in document order, the inverse of `from_tokens`.
    pub fn tokens(&self) -> Vec<(&str, u32)> {
        let mut out: Vec<(&str, u32)> = self
            .terms
            .iter()
            .flat_map(|term| term.positions.iter().map(move |p| (term.term.as_str(), *p)))
            .collect();
        out.sort_unstable_by_key(|(_, position)| *position);
        out
    }
}

/// Iterates records packed back to back.
pub fn records(mut bytes: &[u8]) -> impl Iterator<Item = Result<ForwardRecord>> + '_ {
    std::iter::from_fn(move || {
        if bytes.is_empty() {
            return None;
        }
        match ForwardRecord::decode(bytes) {
            Ok((record, consumed)) => {
                bytes = &bytes[consumed..];
                Some(Ok(record))
            }
            Err(error) => {
                bytes = &[];
                Some(Err(error))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_tokens_and_packs_records() {
        let tid = Tid::new(12, 3).unwrap();
        let tokens = [("craft", 1), ("beer", 2), ("craft", 4), ("ale", 9)];
        let record = ForwardRecord::from_tokens(tid, tokens).unwrap();
        assert_eq!(record.doc_len, 4);
        assert_eq!(
            record
                .terms
                .iter()
                .map(|t| t.term.as_str())
                .collect::<Vec<_>>(),
            ["ale", "beer", "craft"]
        );
        assert_eq!(record.terms[2].positions, [1, 4]);
        assert_eq!(record.tokens(), tokens);
        let mut bytes = Vec::new();
        record.encode(&mut bytes).unwrap();
        let empty = ForwardRecord::from_tokens(Tid::new(13, 1).unwrap(), []).unwrap();
        empty.encode(&mut bytes).unwrap();
        let decoded: Vec<ForwardRecord> = records(&bytes).map(Result::unwrap).collect();
        assert_eq!(decoded, [record.clone(), empty]);
        let (first, consumed) = ForwardRecord::decode(&bytes).unwrap();
        assert_eq!(first, record);
        assert_eq!(consumed, ForwardRecord::encoded_len(&bytes).unwrap());
    }

    #[test]
    fn validation_and_corruption() {
        assert_eq!(
            ForwardRecord::from_tokens(Tid::new(1, 1).unwrap(), [("a", 2), ("b", 2)]),
            Err(Error::InvalidPositions)
        );
        assert_eq!(
            ForwardRecord::from_tokens(Tid::new(1, 1).unwrap(), [("", 1)]),
            Err(Error::EmptyTerm)
        );
        let unordered = ForwardRecord {
            tid: Tid::new(1, 1).unwrap(),
            doc_len: 2,
            terms: vec![
                ForwardTerm {
                    term: "b".into(),
                    positions: vec![1],
                },
                ForwardTerm {
                    term: "a".into(),
                    positions: vec![2],
                },
            ],
        };
        assert_eq!(unordered.encode(&mut Vec::new()), Err(Error::Unordered));
        let record =
            ForwardRecord::from_tokens(Tid::new(1, 1).unwrap(), [("x", 1), ("y", 2)]).unwrap();
        let mut bytes = Vec::new();
        record.encode(&mut bytes).unwrap();
        assert!(ForwardRecord::decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(ForwardRecord::decode(&[]).is_err());
        let mut tampered = bytes.clone();
        tampered[0] += 1; // Length prefix now exceeds the input.
        assert!(ForwardRecord::decode(&tampered).is_err());
        let errors: Vec<_> = records(&bytes[..bytes.len() - 1]).collect();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].is_err());
    }
}
