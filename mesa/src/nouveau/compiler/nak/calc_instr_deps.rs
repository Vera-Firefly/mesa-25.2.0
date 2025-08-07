// Copyright © 2022 Collabora, Ltd.
// SPDX-License-Identifier: MIT

use crate::api::{GetDebugFlags, DEBUG};
use crate::dataflow::ForwardDataflow;
use crate::ir::*;
use crate::reg_tracker::RegTracker;

use rustc_hash::{FxHashMap, FxHashSet};
use std::cmp::max;
use std::ops::Range;
use std::{slice, u32, u8};

#[derive(Clone)]
enum RegUse<T: Clone> {
    None,
    Write(T),
    Reads(Vec<T>),
}

impl<T: Clone> RegUse<T> {
    pub fn deps(&self) -> &[T] {
        match self {
            RegUse::None => &[],
            RegUse::Write(dep) => slice::from_ref(dep),
            RegUse::Reads(deps) => &deps[..],
        }
    }

    pub fn clear(&mut self) -> Self {
        std::mem::replace(self, RegUse::None)
    }

    pub fn clear_write(&mut self) -> Self {
        if matches!(self, RegUse::Write(_)) {
            std::mem::replace(self, RegUse::None)
        } else {
            RegUse::None
        }
    }

    pub fn add_read(&mut self, dep: T) -> Self {
        match self {
            RegUse::None => {
                *self = RegUse::Reads(vec![dep]);
                RegUse::None
            }
            RegUse::Write(_) => {
                std::mem::replace(self, RegUse::Reads(vec![dep]))
            }
            RegUse::Reads(reads) => {
                reads.push(dep);
                RegUse::None
            }
        }
    }

    pub fn set_write(&mut self, dep: T) -> Self {
        std::mem::replace(self, RegUse::Write(dep))
    }
}

struct DepNode {
    read_dep: Option<usize>,
    first_wait: Option<(usize, usize)>,
}

struct DepGraph {
    deps: Vec<DepNode>,
    instr_deps: FxHashMap<(usize, usize), (usize, usize)>,
    instr_waits: FxHashMap<(usize, usize), Vec<usize>>,
    active: FxHashSet<usize>,
}

impl DepGraph {
    pub fn new() -> Self {
        Self {
            deps: Vec::new(),
            instr_deps: Default::default(),
            instr_waits: Default::default(),
            active: Default::default(),
        }
    }

    fn add_new_dep(&mut self, read_dep: Option<usize>) -> usize {
        let dep = self.deps.len();
        self.deps.push(DepNode {
            read_dep: read_dep,
            first_wait: None,
        });
        dep
    }

    pub fn add_instr(&mut self, block_idx: usize, ip: usize) -> (usize, usize) {
        let rd = self.add_new_dep(None);
        let wr = self.add_new_dep(Some(rd));
        self.instr_deps.insert((block_idx, ip), (rd, wr));
        (rd, wr)
    }

    pub fn add_signal(&mut self, dep: usize) {
        self.active.insert(dep);
    }

    pub fn add_waits(
        &mut self,
        block_idx: usize,
        ip: usize,
        mut waits: Vec<usize>,
    ) {
        for dep in &waits {
            // A wait on a write automatically waits on the read.  By removing
            // it from the active set here we ensure that we don't record any
            // duplicate write/read waits in the retain below.
            if let Some(rd) = &self.deps[*dep].read_dep {
                self.active.remove(rd);
            }
        }

        waits.retain(|dep| {
            let node = &mut self.deps[*dep];
            if let Some(wait) = node.first_wait {
                // Someone has already waited on this dep
                debug_assert!(!self.active.contains(dep));
                debug_assert!((block_idx, ip) >= wait);
                false
            } else if !self.active.contains(dep) {
                // Even if it doesn't have a use, it may still be deactivated.
                // This can happen if we depend the the destination before any
                // of its sources.
                false
            } else {
                self.deps[*dep].first_wait = Some((block_idx, ip));
                self.active.remove(dep);
                true
            }
        });

        // Sort for stability.  The list of waits may come from a HashSet (see
        // add_barrier()) and so it's not guaranteed stable across Rust
        // versions.  This also ensures that everything always waits on oldest
        // dependencies first.
        waits.sort();

        let _old = self.instr_waits.insert((block_idx, ip), waits);
        debug_assert!(_old.is_none());
    }

    pub fn add_barrier(&mut self, block_idx: usize, ip: usize) {
        let waits = self.active.iter().cloned().collect();
        self.add_waits(block_idx, ip, waits);
        debug_assert!(self.active.is_empty());
    }

    pub fn dep_is_waited_after(
        &self,
        dep: usize,
        block_idx: usize,
        ip: usize,
    ) -> bool {
        if let Some(wait) = self.deps[dep].first_wait {
            wait > (block_idx, ip)
        } else {
            false
        }
    }

    pub fn get_instr_deps(
        &self,
        block_idx: usize,
        ip: usize,
    ) -> (usize, usize) {
        *self.instr_deps.get(&(block_idx, ip)).unwrap()
    }

    pub fn get_instr_waits(&self, block_idx: usize, ip: usize) -> &[usize] {
        if let Some(waits) = self.instr_waits.get(&(block_idx, ip)) {
            &waits[..]
        } else {
            &[]
        }
    }
}

struct BarAlloc {
    num_bars: u8,
    bar_dep: [usize; 6],
}

impl BarAlloc {
    pub fn new() -> BarAlloc {
        BarAlloc {
            num_bars: 6,
            bar_dep: [usize::MAX; 6],
        }
    }

    pub fn bar_is_free(&self, bar: u8) -> bool {
        debug_assert!(bar < self.num_bars);
        self.bar_dep[usize::from(bar)] == usize::MAX
    }

    pub fn set_bar_dep(&mut self, bar: u8, dep: usize) {
        debug_assert!(self.bar_is_free(bar));
        self.bar_dep[usize::from(bar)] = dep;
    }

    pub fn free_bar(&mut self, bar: u8) {
        debug_assert!(!self.bar_is_free(bar));
        self.bar_dep[usize::from(bar)] = usize::MAX;
    }

    pub fn try_find_free_bar(&self) -> Option<u8> {
        (0..self.num_bars).find(|&bar| self.bar_is_free(bar))
    }

    pub fn free_some_bar(&mut self) -> u8 {
        // Get the oldest by looking for the one with the smallest dep
        let mut bar = 0;
        for b in 1..self.num_bars {
            if self.bar_dep[usize::from(b)] < self.bar_dep[usize::from(bar)] {
                bar = b;
            }
        }
        self.free_bar(bar);
        bar
    }

    pub fn get_bar_for_dep(&self, dep: usize) -> Option<u8> {
        (0..self.num_bars).find(|&bar| self.bar_dep[usize::from(bar)] == dep)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct TexQueueSimulationEntry {
    min_pos: u8,
}

impl TexQueueSimulationEntry {
    const INVALID: Self = TexQueueSimulationEntry { min_pos: u8::MAX };

    // First element on the queue
    const FIRST: Self = TexQueueSimulationEntry { min_pos: 0 };

    fn is_valid(&self) -> bool {
        if *self == Self::INVALID {
            false
        } else {
            debug_assert!(self.min_pos <= OpTexDepBar::MAX_TEXTURES_LEFT);
            true
        }
    }

    fn push(&mut self) {
        if self.is_valid() {
            self.min_pos += 1;
        }
    }

    fn flush_after(&mut self, pos: u8) -> bool {
        if self.min_pos < pos {
            true
        } else {
            // This entry is either invalid or higher than the cull level
            *self = Self::INVALID;
            false
        }
    }

    fn merge(&mut self, other: &Self) {
        self.min_pos = self.min_pos.min(other.min_pos);
    }
}

/// Simulate the state of a register in the queue, in buckets of 4
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct TexQueueSimulationBucket {
    entries: [TexQueueSimulationEntry; 4],
}

impl TexQueueSimulationBucket {
    const EMPTY: Self = TexQueueSimulationBucket {
        entries: [TexQueueSimulationEntry::INVALID; 4],
    };

    fn min_queue_position(&self, range: Range<usize>) -> Option<u8> {
        self.entries[range]
            .iter()
            .filter(|x| x.is_valid())
            .map(|x| x.min_pos)
            .min()
    }

    fn set_as_first(&mut self, range: Range<usize>) {
        for i in range {
            debug_assert!(!self.entries[i].is_valid());
            self.entries[i] = TexQueueSimulationEntry::FIRST;
        }
    }

    fn push(&mut self) {
        for entry in &mut self.entries {
            entry.push();
        }
    }

    fn flush_after(&mut self, pos: u8) -> bool {
        debug_assert!(pos <= OpTexDepBar::MAX_TEXTURES_LEFT);

        let mut retain = false;
        for x in &mut self.entries {
            retain |= x.flush_after(pos);
        }
        retain
    }

    fn merge(&mut self, other: &Self) {
        for (x, y) in self.entries.iter_mut().zip(other.entries.iter()) {
            x.merge(y);
        }
    }
}

/// This state simulates the texture queue for each destination.
///
/// For example, at the start the queue is always empty, but if we encounter a
/// tex operation that writes in r4..r8, that is pushed on the queue at
/// position 0.  If we encounter another tex operation that only writes r5,
/// that will be pushed at position 0 and the old tex instruction will be in
/// position 1.  This data-structure keeps track of the position of the queue
/// for each destination register present in the queue, push operations
/// correspond to new texture instructions, while flush operations correspond to
/// the usage of registers which may still be on the queue.
///
/// Since all Kepler texture operations use at most 4 registers, and many
/// instruction use more than one destination at a time, we group registers in
/// buckets of 4.  With this optimization each RegRef only accesses a single
/// bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TexQueueSimulationState {
    /// Min position of the destination register in the queue,
    /// in buckets of 4 (indexed by register_index / 4).
    queue_pos: FxHashMap<u8, TexQueueSimulationBucket>,
    /// Max length of the queue, needed to check for overflows
    max_queue_len: u8,
}

impl TexQueueSimulationState {
    pub fn new() -> Self {
        TexQueueSimulationState {
            queue_pos: Default::default(),
            max_queue_len: 0,
        }
    }

    /// Translate from RegRef to bucket_index + bucket_range
    #[inline]
    fn reg_ref_to_coords(reg: RegRef) -> (u8, Range<usize>) {
        debug_assert!(reg.base_idx() <= u8::MAX.into());
        let idx = reg.base_idx() as u8 / 4;
        let sub = (reg.base_idx() % 4) as usize;

        let range = sub..(sub + reg.comps() as usize);
        assert!(range.end <= 4);
        (idx, range)
    }

    fn min_queue_position(&self, reg: RegRef) -> Option<u8> {
        let (idx, range) = Self::reg_ref_to_coords(reg);

        self.queue_pos
            .get(&idx)
            .and_then(|x| x.min_queue_position(range))
    }

    fn is_queue_full(&self) -> bool {
        // MAX_TEXTURES_LEFT describes the maximum number encodable
        // in the texdepbar, but the queue must have an element more.
        self.max_queue_len > OpTexDepBar::MAX_TEXTURES_LEFT
    }

    /// Flush every element whose position >= pos
    ///
    /// Effectively simulates the execution of a `texdepbar pos`
    fn flush_after(&mut self, pos: u8) {
        self.max_queue_len = self.max_queue_len.min(pos);
        self.queue_pos.retain(|_, v| v.flush_after(pos));
    }

    pub fn push(&mut self, reg: RegRef) -> Option<u8> {
        // Assert we are not on the queue
        debug_assert!(self.min_queue_position(reg).is_none());

        // Check that the push operation does not overflow the queue,
        // if it does, we must insert a barrier
        let mut tex_bar = None;
        if self.is_queue_full() {
            // The queue is full, there are 64 in-flight tex-ops.
            // make space by making removing 1 texture.
            tex_bar = Some(OpTexDepBar::MAX_TEXTURES_LEFT);
            self.flush_after(OpTexDepBar::MAX_TEXTURES_LEFT);
            // Now the queue is not full anymore
            debug_assert!(!self.is_queue_full());
        }

        self.max_queue_len += 1;
        // Every entry is pushed by 1
        for x in self.queue_pos.values_mut() {
            x.push();
        }

        // Put us on the queue as first
        let (idx, range) = Self::reg_ref_to_coords(reg);
        self.queue_pos
            .entry(idx)
            .or_insert(TexQueueSimulationBucket::EMPTY)
            .set_as_first(range);

        tex_bar
    }

    pub fn flush(&mut self, reg: RegRef) -> Option<u8> {
        let queue_pos = self.min_queue_position(reg);

        let Some(queue_pos) = queue_pos else {
            return None; // Not in queue
        };

        // Cut the queue
        self.flush_after(queue_pos);
        debug_assert!(self.min_queue_position(reg).is_none());

        Some(queue_pos)
    }

    pub fn merge(&mut self, other: &Self) {
        self.max_queue_len = self.max_queue_len.max(other.max_queue_len);
        for (key, y) in other.queue_pos.iter() {
            let x = self
                .queue_pos
                .entry(*key)
                .or_insert(TexQueueSimulationBucket::EMPTY);
            x.merge(y);
        }
    }

    /// Simulates the execution of an instruction and returns the
    /// barrier level needed.
    pub fn visit_instr(&mut self, instr: &Instr) -> Option<u8> {
        // Flush register reads and writes
        // (avoid write-after-write and read-after-write hazards)
        // Compute the minimum required flush level (for barriers)
        let flush_level = if !self.queue_pos.is_empty() {
            let src_refs =
                instr.srcs().iter().filter_map(|x| x.src_ref.as_reg());
            let dst_refs = instr.dsts().iter().filter_map(|x| x.as_reg());

            src_refs
                .chain(dst_refs)
                .filter_map(|reg_ref| self.flush(*reg_ref))
                .reduce(|a, b| a.min(b))
        } else {
            // The queue is empty, no need to check the instruction
            None
        };

        // Push registers (if we are a tex instruction)
        // We might need to insert a barrier if the queue is full
        let push_level = if instr_needs_texbar(&instr) {
            let dst = instr.dsts()[0].as_reg().unwrap();
            self.push(*dst)
        } else {
            None
        };

        // If the flush needs a barrier, the queue will not be full,
        // therefore the push will not need a barrier.
        debug_assert!(!flush_level.is_some() || !push_level.is_some());
        flush_level.or(push_level)
    }
}

fn instr_needs_texbar(instr: &Instr) -> bool {
    matches!(
        instr.op,
        Op::Tex(_)
            | Op::Tld(_)
            | Op::Tmml(_)
            | Op::Tld4(_)
            | Op::Txd(_)
            | Op::Txq(_)
    )
}

/// Hardware has a FIFO queue of texture that are still fetching,
/// when the oldest tex finishes executing, it's written to the reg,
/// removed from the queue and it begins executing the new one.
/// The problem arises when a texture is read while it is still being fetched
/// to avoid it, we have a `texdepbar {i}` instruction that stalls until
/// the texture fetch queue has at most {i} elements.
/// e.g. the most simple solution is to have texdepbar 0 after each texture
/// instruction, but this would stall the pipeline until the texture fetch
/// finishes executing.
/// This algorithm inserts `texdepbar` at each use of the texture results,
/// simulating the texture queue execution.
///
/// Note that the texture queue has for each entry (texture data, register output)
/// and each register can be on the queue only once (we don't want to have multiple texture
/// operations in-flight that write to the same registers).
/// This can lead to a neat algorithm:
/// instead of tracking the queue directly, which can exponentially explode in complexity,
/// track the position of each register, which needs at most 255/63 positions.
/// For branches the state is duplicated in each basic block,
/// for joins instead we want to keep both the minimum position of each
/// entry and the maximum length og the queue to avoid overflows.
///
/// TODO: IF this pass is too slow, there are still optimizations left:
/// - Our data-flow computes barrier levels and discards them,
///   but since most CFG blocks do not need recomputation, we could save
///   the barrier levels in a vec and save a pass later.
/// - Instead of pushing by 1 each element in the queue on a `push` op,
///   we could keep track of an in-flight range and use a wrapping timestamp
///   this improves performance but needs careful implementation to avoid bugs
fn insert_texture_barriers(f: &mut Function, sm: &dyn ShaderModel) {
    assert!(sm.is_kepler()); // Only kepler has texture barriers!

    let mut state_in: Vec<_> = (0..f.blocks.len())
        .map(|_| TexQueueSimulationState::new())
        .collect();
    let mut state_out: Vec<_> = (0..f.blocks.len())
        .map(|_| TexQueueSimulationState::new())
        .collect();
    ForwardDataflow {
        cfg: &f.blocks,
        block_in: &mut state_in[..],
        block_out: &mut state_out[..],
        transfer: |_block_idx, block, sim_out, sim_in| {
            let mut sim = sim_in.clone();

            for instr in block.instrs.iter() {
                // Ignore the barrier, we will recompute this later
                let _bar = sim.visit_instr(&instr);
            }

            if *sim_out == sim {
                false
            } else {
                *sim_out = sim;
                true
            }
        },
        join: |sim_out, pred_sim_in| {
            sim_out.merge(pred_sim_in);
        },
    }
    .solve();

    for (block, mut sim) in f.blocks.iter_mut().zip(state_in.into_iter()) {
        block.map_instrs(|instr| {
            if let Some(textures_left) = sim.visit_instr(&instr) {
                let bar = Instr::new_boxed(OpTexDepBar { textures_left });
                MappedInstrs::Many(vec![bar, instr])
            } else {
                MappedInstrs::One(instr)
            }
        });
    }
}

fn assign_barriers(f: &mut Function, sm: &dyn ShaderModel) {
    let mut uses = Box::new(RegTracker::new_with(&|| RegUse::None));
    let mut deps = DepGraph::new();

    for (bi, b) in f.blocks.iter().enumerate() {
        for (ip, instr) in b.instrs.iter().enumerate() {
            if instr.is_branch() {
                deps.add_barrier(bi, ip);
            } else {
                // Execution predicates are handled immediately and we don't
                // need barriers for them, regardless of whether or not it's a
                // fixed-latency instruction.
                let mut waits = Vec::new();
                uses.for_each_instr_pred_mut(instr, |u| {
                    let u = u.clear_write();
                    waits.extend_from_slice(u.deps());
                });

                if sm.op_needs_scoreboard(&instr.op) {
                    let (rd, wr) = deps.add_instr(bi, ip);
                    uses.for_each_instr_src_mut(instr, |_, u| {
                        // Only mark a dep as signaled if we actually have
                        // something that shows up in the register file as
                        // needing scoreboarding
                        deps.add_signal(rd);
                        let u = u.add_read(rd);
                        waits.extend_from_slice(u.deps());
                    });
                    uses.for_each_instr_dst_mut(instr, |_, u| {
                        // Only mark a dep as signaled if we actually have
                        // something that shows up in the register file as
                        // needing scoreboarding
                        deps.add_signal(wr);
                        let u = u.set_write(wr);
                        for dep in u.deps() {
                            // Don't wait on ourselves
                            if *dep != rd {
                                waits.push(*dep);
                            }
                        }
                    });
                } else {
                    // Delays will cover us here.  We just need to make sure
                    // that we wait on any uses that we consume.
                    uses.for_each_instr_src_mut(instr, |_, u| {
                        let u = u.clear_write();
                        waits.extend_from_slice(u.deps());
                    });
                    uses.for_each_instr_dst_mut(instr, |_, u| {
                        let u = u.clear();
                        waits.extend_from_slice(u.deps());
                    });
                }
                deps.add_waits(bi, ip, waits);
            }
        }
    }

    let mut bars = BarAlloc::new();

    for (bi, b) in f.blocks.iter_mut().enumerate() {
        for (ip, instr) in b.instrs.iter_mut().enumerate() {
            let mut wait_mask = 0_u8;
            for dep in deps.get_instr_waits(bi, ip) {
                if let Some(bar) = bars.get_bar_for_dep(*dep) {
                    wait_mask |= 1 << bar;
                    bars.free_bar(bar);
                }
            }
            instr.deps.add_wt_bar_mask(wait_mask);

            if instr.needs_yield() {
                instr.deps.set_yield(true);
            }

            if !sm.op_needs_scoreboard(&instr.op) {
                continue;
            }

            let (rd_dep, wr_dep) = deps.get_instr_deps(bi, ip);
            if deps.dep_is_waited_after(rd_dep, bi, ip) {
                let rd_bar = bars.try_find_free_bar().unwrap_or_else(|| {
                    let bar = bars.free_some_bar();
                    instr.deps.add_wt_bar(bar);
                    bar
                });
                bars.set_bar_dep(rd_bar, rd_dep);
                instr.deps.set_rd_bar(rd_bar);
            }
            if deps.dep_is_waited_after(wr_dep, bi, ip) {
                let wr_bar = bars.try_find_free_bar().unwrap_or_else(|| {
                    let bar = bars.free_some_bar();
                    instr.deps.add_wt_bar(bar);
                    bar
                });
                bars.set_bar_dep(wr_bar, wr_dep);
                instr.deps.set_wr_bar(wr_bar);
            }
        }
    }
}

fn calc_delays(f: &mut Function, sm: &dyn ShaderModel) -> u32 {
    let mut min_num_static_cycles = 0;
    for b in f.blocks.iter_mut().rev() {
        let mut cycle = 0_u32;

        // Vector mapping IP to start cycle
        let mut instr_cycle = vec![0; b.instrs.len()];

        // Maps registers to RegUse<ip, src_dst_idx>.  Predicates are
        // represented by  src_idx = usize::MAX.
        let mut uses: Box<RegTracker<RegUse<(usize, usize)>>> =
            Box::new(RegTracker::new_with(&|| RegUse::None));

        // Map from barrier to last waited cycle
        let mut bars = [0_u32; 6];

        for ip in (0..b.instrs.len()).rev() {
            let instr = &b.instrs[ip];
            let mut min_start = cycle + sm.exec_latency(&instr.op);
            if let Some(bar) = instr.deps.rd_bar() {
                min_start = max(min_start, bars[usize::from(bar)] + 2);
            }
            if let Some(bar) = instr.deps.wr_bar() {
                min_start = max(min_start, bars[usize::from(bar)] + 2);
            }
            uses.for_each_instr_dst_mut(instr, |i, u| match u {
                RegUse::None => {
                    // We don't know how it will be used but it may be used in
                    // the next block so we need at least assume the maximum
                    // destination latency from the end of the block.
                    let s = sm.worst_latency(&instr.op, i);
                    min_start = max(min_start, s);
                }
                RegUse::Write((w_ip, w_dst_idx)) => {
                    let s = instr_cycle[*w_ip]
                        + sm.waw_latency(
                            &instr.op,
                            i,
                            !instr.pred.pred_ref.is_none(),
                            &b.instrs[*w_ip].op,
                            *w_dst_idx,
                        );
                    min_start = max(min_start, s);
                }
                RegUse::Reads(reads) => {
                    for (r_ip, r_src_idx) in reads {
                        let c = instr_cycle[*r_ip];
                        let s = if *r_src_idx == usize::MAX {
                            c + sm.paw_latency(&instr.op, i)
                        } else {
                            c + sm.raw_latency(
                                &instr.op,
                                i,
                                &b.instrs[*r_ip].op,
                                *r_src_idx,
                            )
                        };
                        min_start = max(min_start, s);
                    }
                }
            });
            uses.for_each_instr_src_mut(instr, |i, u| match u {
                RegUse::None => (),
                RegUse::Write((w_ip, w_dst_idx)) => {
                    let s = instr_cycle[*w_ip]
                        + sm.war_latency(
                            &instr.op,
                            i,
                            &b.instrs[*w_ip].op,
                            *w_dst_idx,
                        );
                    min_start = max(min_start, s);
                }
                RegUse::Reads(_) => (),
            });

            let instr = &mut b.instrs[ip];

            let delay = min_start - cycle;
            let delay = delay.max(MIN_INSTR_DELAY.into()).try_into().unwrap();
            instr.deps.set_delay(delay);

            instr_cycle[ip] = min_start;

            // Set the writes before adding the reads
            // as we are iterating backwards through instructions.
            uses.for_each_instr_dst_mut(instr, |i, c| {
                c.set_write((ip, i));
            });
            uses.for_each_instr_pred_mut(instr, |c| {
                c.add_read((ip, usize::MAX));
            });
            uses.for_each_instr_src_mut(instr, |i, c| {
                c.add_read((ip, i));
            });
            for (bar, c) in bars.iter_mut().enumerate() {
                if instr.deps.wt_bar_mask & (1 << bar) != 0 {
                    *c = min_start;
                }
            }

            cycle = min_start;
        }
        min_num_static_cycles += cycle;
    }

    let max_instr_delay = sm.max_instr_delay();
    f.map_instrs(|mut instr, _| {
        if instr.deps.delay > max_instr_delay {
            let mut delay = instr.deps.delay - max_instr_delay;
            instr.deps.set_delay(max_instr_delay);
            let mut instrs = vec![instr];
            while delay > 0 {
                let mut nop = Instr::new_boxed(OpNop { label: None });
                nop.deps.set_delay(delay.min(max_instr_delay));
                delay -= nop.deps.delay;
                instrs.push(nop);
            }
            MappedInstrs::Many(instrs)
        } else if matches!(instr.op, Op::SrcBar(_)) {
            instr.op = Op::Nop(OpNop { label: None });
            MappedInstrs::One(instr)
        } else if sm.exec_latency(&instr.op) > 1 {
            // It's unclear exactly why but the blob inserts a Nop with a delay
            // of 2 after every instruction which has an exec latency.  Perhaps
            // it has something to do with .yld?  In any case, the extra 2
            // cycles aren't worth the chance of weird bugs.
            let mut nop = Instr::new_boxed(OpNop { label: None });
            nop.deps.set_delay(2);
            MappedInstrs::Many(vec![instr, nop])
        } else {
            MappedInstrs::One(instr)
        }
    });

    min_num_static_cycles
}

impl Shader<'_> {
    pub fn assign_deps_serial(&mut self) {
        for f in &mut self.functions {
            for b in &mut f.blocks.iter_mut().rev() {
                let mut wt = 0_u8;
                for instr in &mut b.instrs {
                    if matches!(&instr.op, Op::Bar(_))
                        || matches!(&instr.op, Op::BClear(_))
                        || matches!(&instr.op, Op::BSSy(_))
                        || matches!(&instr.op, Op::BSync(_))
                    {
                        instr.deps.set_yield(true);
                    } else if instr.is_branch() {
                        instr.deps.add_wt_bar_mask(0x3f);
                    } else {
                        instr.deps.add_wt_bar_mask(wt);
                        if instr.dsts().len() > 0 {
                            instr.deps.set_wr_bar(0);
                            wt |= 1 << 0;
                        }
                        if !instr.pred.pred_ref.is_none()
                            || instr.srcs().len() > 0
                        {
                            instr.deps.set_rd_bar(1);
                            wt |= 1 << 1;
                        }
                    }
                }
            }
        }
    }

    pub fn calc_instr_deps(&mut self) {
        if self.sm.is_kepler() {
            for f in &mut self.functions {
                insert_texture_barriers(f, self.sm);
            }
        }

        if DEBUG.serial() {
            self.assign_deps_serial();
        } else {
            let mut min_num_static_cycles = 0;
            for f in &mut self.functions {
                assign_barriers(f, self.sm);
                min_num_static_cycles += calc_delays(f, self.sm);
            }

            if DEBUG.cycles() {
                // This is useful for debugging differences in the scheduler
                // cycle count model and the calc_delays() model.  However, it
                // isn't totally valid since assign_barriers() can add extra
                // dependencies for barrier re-use and those may add cycles.
                // The chances of it doing this are low, thanks to our LRU
                // allocation strategy, but it's still not an assert we want
                // running in production.
                assert!(self.info.num_static_cycles >= min_num_static_cycles);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg_gpr(range: Range<usize>) -> RegRef {
        RegRef::new(
            RegFile::GPR,
            range.start as u32,
            (range.end - range.start) as u8,
        )
    }

    #[test]
    fn test_texdepbar_basic() {
        let mut sim = TexQueueSimulationState::new();

        // RaW
        assert_eq!(sim.push(reg_gpr(0..4)), None);
        assert_eq!(sim.flush(reg_gpr(2..3)), Some(0));

        // 2 entries in the queue
        assert_eq!(sim.push(reg_gpr(0..2)), None); // [A]
        assert_eq!(sim.push(reg_gpr(2..4)), None); // [B, A]
        assert_eq!(sim.flush(reg_gpr(0..1)), Some(1)); // [B]
        assert_eq!(sim.flush(reg_gpr(3..4)), Some(0)); // []

        // Test bucket conflicts
        assert_eq!(sim.push(reg_gpr(0..1)), None);
        assert_eq!(sim.flush(reg_gpr(1..3)), None);
        assert_eq!(sim.flush(reg_gpr(0..3)), Some(0));

        // Bucket conflict part 2: Electric Boogaloo
        assert_eq!(sim.push(reg_gpr(1..2)), None);
        assert_eq!(sim.push(reg_gpr(0..1)), None);
        assert_eq!(sim.flush(reg_gpr(1..2)), Some(1));
        assert_eq!(sim.flush(reg_gpr(0..1)), Some(0));

        // Interesting CFG case that the old pass got wrong.
        // CFG: A -> [B, C] -> D
        // A pushes
        assert_eq!(sim.push(reg_gpr(0..4)), None);
        // B: pushes a tex then flushes it
        let mut b_sim = sim.clone();
        assert_eq!(b_sim.push(reg_gpr(4..8)), None);
        assert_eq!(b_sim.flush(reg_gpr(4..8)), Some(0));
        // C: pushes 3 tex and never flishes them
        let mut c_sim = sim.clone();
        assert_eq!(c_sim.push(reg_gpr(4..5)), None);
        assert_eq!(c_sim.push(reg_gpr(5..6)), None);
        assert_eq!(c_sim.push(reg_gpr(6..7)), None);
        // D: flushes the tex pushed by A
        let mut d_sim = b_sim;
        d_sim.merge(&mut c_sim);
        assert_eq!(c_sim.flush(reg_gpr(0..4)), Some(3));
        // the "shortest push path" would pass by B but in fact
        // by passing in B our texture is flushed off the queue.
        // (old algorithm would insert a texdepbar 1)
    }

    #[test]
    fn test_texdepbar_overflow() {
        let mut sim = TexQueueSimulationState::new();

        // Fill the texture queue
        for i in 0..(usize::from(OpTexDepBar::MAX_TEXTURES_LEFT) + 1) {
            assert_eq!(sim.push(reg_gpr(i..(i + 1))), None);
        }
        // The new push would overflow the queue, we NEED a barrier
        assert_eq!(
            sim.push(reg_gpr(64..65)),
            Some(OpTexDepBar::MAX_TEXTURES_LEFT)
        );
        assert_eq!(
            sim.push(reg_gpr(65..66)),
            Some(OpTexDepBar::MAX_TEXTURES_LEFT)
        );
    }
}
