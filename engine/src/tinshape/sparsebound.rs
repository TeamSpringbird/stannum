// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Lab only (lane sparsebound, never in the extension): per-(sparse term,
//! group) bounds, the census of what they would prune in a disjunction
//! (`sbcensus`) and a timed walk that uses them (`sbuse`).
//!
//! A sparse record bounds a group by its 256-posting footer blocks there.
//! TIN keeps one footer entry per (term, group) for every term. This module
//! derives, per sparse record a walk opens, each group's postings (bucket,
//! length rounded to six significant bits, length class, footer block) and
//! the group's frontier encoded as a grouped record's directory frontier
//! is ([`postings::put_group_frontier`]), cached across passes (built in the
//! replay's untimed answer pass), so a timed walk pays only what reading
//! such an entry from the record would cost: a step to the group's entry
//! and the frontier's fold.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use segment::tinshape::postings::{self, Form, GroupFrontiers};
use segment::tinshape::segment::Segment;

use super::TermSet;

/// One posting of a sparse record, as a per-(term, group) entry would see it.
#[derive(Clone, Copy, Debug)]
pub struct Post {
    pub bucket: u8,
    pub length: u32,
    pub class: u8,
    pub block: u32,
}

/// A sparse record's groups, their postings and frontiers.
#[derive(Debug, Default)]
pub struct Gb {
    pub groups: Vec<u32>,
    /// Group `k`'s postings are `posts[posts_at[k]..posts_at[k + 1]]`.
    pub posts_at: Vec<u32>,
    pub posts: Vec<Post>,
    /// Group `k`'s frontier starts at `fbytes[foff[k]]` (16 bytes of slack
    /// after the last, for the fold's window).
    pub foff: Vec<u32>,
    pub fbytes: Vec<u8>,
    /// Whether the record carries its lengths inline (no format change
    /// needed to bound its groups exactly).
    pub inline: bool,
}

impl Gb {
    fn build(segment: &Segment<'_>, set: &TermSet<'_>) -> Gb {
        let geometry = &segment.docs.geometry;
        let p = &set.postings;
        if matches!(p.form, Form::Grouped(_)) {
            return Gb::default();
        }
        let single = matches!(p.form, Form::Single(_));
        let footer = (!single).then(|| {
            p.footer(geometry, segment.block_size, set.max_bucket, segment.adaptive_tf)
                .expect("a sparse footer")
        });
        let mut gb = Gb {
            inline: p.lengths.is_some(),
            ..Gb::default()
        };
        let slots = p.slots(geometry).expect("a sparse record's slots");
        let mut index = 0u32;
        let mut pairs: Vec<(u8, u32)> = Vec::new();
        let mut i = 0;
        while i < slots.len() {
            let g = geometry.group_of_slot(slots[i]);
            let base = geometry.groups[g].slot_base;
            let end = base + geometry.groups[g].slots();
            gb.groups.push(g as u32);
            gb.posts_at.push(gb.posts.len() as u32);
            pairs.clear();
            while i < slots.len() && slots[i] < end {
                let local = slots[i] - base;
                let rank = segment.docs.rank_in(g, local).expect("a document");
                let length = match &p.lengths {
                    Some(l) => l.get(index).expect("an inline length"),
                    None => segment.lengths.get(rank).expect("a length"),
                };
                let class = if segment.lengths.has_classes() {
                    segment.lengths.class(rank).expect("a class") % 4
                } else {
                    0
                };
                let bucket = match &footer {
                    Some(f) => f.bucket(p.tf, index).expect("a bucket"),
                    None => set.max_bucket,
                };
                gb.posts.push(Post {
                    bucket,
                    length,
                    class,
                    block: index / segment.block_size,
                });
                pairs.push((bucket, length));
                index += 1;
                i += 1;
            }
            let f = postings::group_frontier(pairs.iter().copied(), GroupFrontiers::Rounded);
            gb.foff.push(gb.fbytes.len() as u32);
            // A frontier holds at most 16 pairs (one per bucket).
            postings::put_group_frontier(&mut gb.fbytes, &f);
        }
        gb.posts_at.push(gb.posts.len() as u32);
        gb.fbytes.extend_from_slice(&[0u8; 16]);
        gb
    }
}

thread_local! {
    static CACHE: RefCell<HashMap<(u32, usize), Rc<Gb>>> = RefCell::new(HashMap::new());
}

/// The record's groups and frontiers, built once per (segment, record).
pub fn get(segment: &Segment<'_>, set: &TermSet<'_>) -> Rc<Gb> {
    let key = (segment.documents, set.at);
    if let Some(gb) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return gb;
    }
    let gb = Rc::new(Gb::build(segment, set));
    CACHE.with(|c| c.borrow_mut().insert(key, gb.clone()));
    gb
}

/// Census counters, summed over the walks since the last [`take`].
pub const NAMES: [&str; 24] = [
    "groups",          // 0 groups reached
    "sparse_groups",   // 1 groups reached holding a present sparse term
    "sparse_pairs",    // 2 (sparse term, group) pairs present
    "pruned_A",        // 3 pruned by the group bound today
    "pruned_B",        // 4 ... sparse terms bounded per posting by bucket + block floor (engine only)
    "pruned_I",        // 5 ... exact rounded where inline lengths, else B (engine only)
    "pruned_K",        // 6 ... per posting bucket + length class floor (2 bits a posting)
    "pruned_C",        // 7 ... per-(term, group) rounded frontier (TIN's entry)
    "pruned_Z",        // 8 ... sparse terms at zero (ceiling)
    "maskpruned_A",    // 9 pruned by the empty mask today
    "maskpruned_newC", // 10 of those, groups C prunes by bound
    "newC_postings_loaded", // 11 postings the rows of newly pruned groups (C) loaded today
    "sparse_posts",    // 12 postings of present sparse terms in reached groups
    "inline_pairs",    // 13 pairs whose record has inline lengths
    "ratio_C_sum_ppm", // 14 sum of C/A per pair, in millionths
    "ratio_B_sum_ppm", // 15 sum of B/A per pair
    "pairs_C_lt_A",    // 16 pairs where C < A
    "sparse_terms_opened", // 17
    "pruned_A_sparse", // 18 pruned today among sparse groups
    "pruned_C_sparse", // 19 pruned by C among sparse groups
    "lab_group_miss", // 20 must be 0
    "lab_c_below_score", // 21 must be 0
    "unused22",
    "unused23",
];
pub static C: [AtomicU64; 24] = [const { AtomicU64::new(0) }; 24];

#[inline(always)]
pub fn add(i: usize, v: u64) {
    C[i].fetch_add(v, Ordering::Relaxed);
}

/// The counters since the last call, reset, as `name=value` pairs.
pub fn take() -> String {
    let v: Vec<u64> = C.iter().map(|c| c.swap(0, Ordering::Relaxed)).collect();
    NAMES
        .iter()
        .zip(&v)
        .map(|(n, x)| format!("{n}={x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
