// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Document lengths quantized to one byte, for bounding.
//!
//! A ranked walk bounds a candidate's score before it reads anything; the
//! document's length is the one input it lacks, and the length table is
//! four bytes per document, so reading it is a page per scattered candidate.
//! The class table is one byte per document: every class names the shortest
//! length it holds, so a score bounded at that length covers the document,
//! and the exact length is read only for candidates the class bound admits.
//! Classes 0 to 63 are the lengths 1 to 64 exactly; above that each class is
//! about a tenth longer than the last, reaching `u32::MAX`.

/// The shortest length of each class.
const MIN_LENGTH: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut c = 0;
    let mut length: u64 = 1;
    while c < 256 {
        table[c] = if length > u32::MAX as u64 {
            u32::MAX
        } else {
            length as u32
        };
        length = if c < 63 {
            length + 1
        } else {
            length * 1098 / 1000 + 1
        };
        c += 1;
    }
    table
};

/// The class of a document of `length` tokens: the last class whose
/// shortest length is at most `length`.
pub fn class_of(length: u32) -> u8 {
    (MIN_LENGTH
        .partition_point(|min| *min <= length)
        .saturating_sub(1)) as u8
}

/// The shortest length a document of `class` can have.
pub const fn min_length(class: u8) -> u32 {
    MIN_LENGTH[class as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_cover_every_length_with_a_lower_bound_within_a_tenth() {
        assert_eq!(min_length(0), 1);
        assert_eq!(min_length(63), 64);
        assert!(MIN_LENGTH.windows(2).all(|pair| pair[0] < pair[1]));
        let mut length = 1u64;
        while length <= u32::MAX as u64 {
            let class = class_of(length as u32);
            let min = min_length(class) as u64;
            assert!(min <= length, "{length}: class {class} starts at {min}");
            assert!(
                (class as usize + 1 == 256) || min_length(class + 1) as u64 > length,
                "{length}: class {class} ends before it"
            );
            assert!(
                length <= min + min / 9 + 1,
                "{length}: class {class} starts at {min}"
            );
            length = length * 3 / 2 + 1;
        }
        assert_eq!(class_of(u32::MAX), 255);
        assert_eq!(class_of(0), 0);
    }
}
