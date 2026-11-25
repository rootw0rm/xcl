//! SSA form of the console registers for the direct DXIL lowering, built while the blocks are
//! lowered (the on-the-fly construction of Braun et al., "Simple and Efficient Construction of
//! Static Single Assignment Form", CC 2013).
//!
//! Every register dword (s0–s127, v0–v255, SCC) is a variable. The lowering writes a variable
//! when an instruction defines it and reads it when one uses it; a read in a block without a
//! definition looks through the predecessors, placing a phi where they can disagree. A block
//! whose predecessors are not all lowered yet (a loop head, before its back edges) is *unsealed*:
//! its phis are left incomplete and filled in when it is sealed. A phi whose operands are all one
//! value (or itself) is trivial and is replaced by that value, through the sink's `replace`.
//!
//! The values themselves live in the DXIL module; this only decides where phis go. The sink is
//! the module builder.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// A register dword of the console program: what the SSA form tracks.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Var {
    /// s<n> as a number (`i32`)
    S(u8),
    /// the lane's bit of the mask held in s<n> (wave64: the pair from s<n>) (`i1`)
    M(u8),
    V(u16),
    Scc,
}

/// What the construction needs from the module builder.
pub trait Sink {
    type V: Copy + Eq + Hash + std::fmt::Debug;
    /// A new phi at the top of `block` for `var` (`i1` for SCC, `i32` for a register).
    fn phi(&mut self, block: usize, var: Var) -> Self::V;
    fn incoming(&mut self, phi: Self::V, value: Self::V, pred: usize);
    /// Every use of `old` becomes `new`, and `old` (a trivial phi) is dropped.
    fn replace(&mut self, old: Self::V, new: Self::V);
    /// The value of a variable read before any definition reaches it.
    fn undef(&mut self, var: Var) -> Self::V;
}

pub struct Ssa<V> {
    preds: Vec<Vec<usize>>,
    defs: Vec<HashMap<Var, V>>,
    sealed: Vec<bool>,
    /// per unsealed block: the phis placed before its predecessors were known
    incomplete: Vec<Vec<(Var, V)>>,
    /// the operands of each live phi, and the phis that use each value
    operands: HashMap<V, Vec<V>>,
    users: HashMap<V, Vec<V>>,
    /// trivial phis removed → what replaced them
    forward: HashMap<V, V>,
}

impl<V: Copy + Eq + Hash + std::fmt::Debug> Ssa<V> {
    /// `preds[b]`: the predecessors of block b, all of them, back edges included.
    pub fn new(preds: Vec<Vec<usize>>) -> Self {
        let n = preds.len();
        Ssa {
            preds,
            defs: vec![HashMap::new(); n],
            sealed: vec![false; n],
            incomplete: vec![Vec::new(); n],
            operands: HashMap::new(),
            users: HashMap::new(),
            forward: HashMap::new(),
        }
    }

    /// What a value stands for now: itself, or what replaced it as a trivial phi.
    pub fn resolve_value(&self, v: V) -> V {
        self.resolve(v)
    }

    /// The operands of a live phi (None for any other value).
    pub fn phi_operands(&self, v: V) -> Option<&[V]> {
        self.operands.get(&v).map(|o| o.as_slice())
    }

    fn resolve(&self, mut v: V) -> V {
        while let Some(&n) = self.forward.get(&v) {
            v = n;
        }
        v
    }

    pub fn write(&mut self, block: usize, var: Var, value: V) {
        self.defs[block].insert(var, value);
    }

    pub fn read<S: Sink<V = V>>(&mut self, sink: &mut S, block: usize, var: Var) -> V {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut cur = block;
        let v = loop {
            if let Some(&v) = self.defs[cur].get(&var) {
                break self.resolve(v);
            }
            if !self.sealed[cur] {
                let phi = self.new_phi(sink, cur, var);
                self.incomplete[cur].push((var, phi));
                self.write(cur, var, phi);
                break phi;
            }
            match self.preds[cur].as_slice() {
                [] => {
                    let u = sink.undef(var);
                    self.write(cur, var, u);
                    break u;
                }
                &[p] => {
                    if !seen.insert(cur) {
                        break sink.undef(var);
                    }
                    chain.push(cur);
                    cur = p;
                }
                _ => {
                    // the phi is the definition while its operands are read (a loop through here)
                    let phi = self.new_phi(sink, cur, var);
                    self.write(cur, var, phi);
                    let v = self.add_operands(sink, cur, var, phi);
                    self.write(cur, var, v);
                    break v;
                }
            }
        };
        for b in chain {
            self.write(b, var, v);
        }
        v
    }

    /// Every predecessor of `block` has been lowered: complete its phis.
    pub fn seal<S: Sink<V = V>>(&mut self, sink: &mut S, block: usize) {
        for (var, phi) in std::mem::take(&mut self.incomplete[block]) {
            self.add_operands(sink, block, var, phi);
        }
        self.sealed[block] = true;
    }

    fn new_phi<S: Sink<V = V>>(&mut self, sink: &mut S, block: usize, var: Var) -> V {
        let phi = sink.phi(block, var);
        self.operands.insert(phi, Vec::new());
        phi
    }

    fn add_operands<S: Sink<V = V>>(&mut self, sink: &mut S, block: usize, var: Var, phi: V) -> V {
        let preds = self.preds[block].clone();
        if preds.is_empty() {
            let undef = sink.undef(var);
            return self.replace_phi(sink, phi, undef);
        }
        // Collect all incoming values before registering operands/users so a half-populated
        // phi is never exposed to a recursive remove_trivial cascade.
        let incoming: Vec<(usize, V)> = preds.into_iter().map(|p| (p, self.read(sink, p, var))).collect();
        let mut ops = Vec::with_capacity(incoming.len());
        for (p, v) in incoming {
            let v = self.resolve(v);
            sink.incoming(phi, v, p);
            ops.push(v);
            if v != phi {
                let list = self.users.entry(v).or_default();
                if !list.contains(&phi) {
                    list.push(phi);
                }
            }
        }
        *self.operands.get_mut(&phi).expect("live phi") = ops;
        self.remove_trivial(sink, phi)
    }

    fn remove_trivial<S: Sink<V = V>>(&mut self, sink: &mut S, phi: V) -> V {
        let mut same = None;
        for &op in &self.operands[&phi] {
            let op = self.resolve(op);
            if Some(op) == same || op == phi {
                continue;
            }
            if same.is_some() {
                return phi;
            }
            same = Some(op);
        }
        let Some(same) = same else { return phi }; // only reachable through itself: kept
        self.replace_phi(sink, phi, same)
    }

    fn replace_phi<S: Sink<V = V>>(&mut self, sink: &mut S, phi: V, replacement: V) -> V {
        let replacement = self.resolve(replacement);
        sink.replace(phi, replacement);
        self.forward.insert(phi, replacement);
        self.operands.remove(&phi);
        // Transfer live users to `replacement` before cascading so any user that stays live
        // now is revisited if `replacement` itself collapses later (or during this cascade).
        let users = self.users.remove(&phi).unwrap_or_default();
        for &user in &users {
            if user != phi && user != replacement && self.operands.contains_key(&user) {
                let list = self.users.entry(replacement).or_default();
                if !list.contains(&user) {
                    list.push(user);
                }
            }
        }
        for user in users {
            if user != phi && self.operands.contains_key(&user) {
                self.remove_trivial(sink, user);
            }
        }
        self.resolve(replacement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what the construction asks of a module builder.
    #[derive(Default)]
    struct Log {
        next: u32,
        phis: Vec<(u32, usize, Var)>,
        incoming: Vec<(u32, u32, usize)>,
        replaced: Vec<(u32, u32)>,
    }

    impl Log {
        fn value(&mut self) -> u32 {
            self.next += 1;
            self.next
        }
        fn live_phis(&self) -> Vec<u32> {
            self.phis.iter().map(|p| p.0).filter(|p| !self.replaced.iter().any(|r| r.0 == *p)).collect()
        }
    }

    impl Sink for Log {
        type V = u32;
        fn phi(&mut self, block: usize, var: Var) -> u32 {
            let v = self.value();
            self.phis.push((v, block, var));
            v
        }
        fn incoming(&mut self, phi: u32, value: u32, pred: usize) {
            self.incoming.push((phi, value, pred));
        }
        fn replace(&mut self, old: u32, new: u32) {
            self.replaced.push((old, new));
        }
        fn undef(&mut self, _: Var) -> u32 {
            0
        }
    }

    const R: Var = Var::V(3);

    #[test]
    fn a_straight_line_needs_no_phi() {
        // 0 → 1 → 2
        let (mut ssa, mut log) = (Ssa::new(vec![vec![], vec![0], vec![1]]), Log::default());
        let a = log.value();
        ssa.write(0, R, a);
        for b in 0..3 {
            ssa.seal(&mut log, b);
        }
        assert_eq!(ssa.read(&mut log, 2, R), a);
        assert!(log.phis.is_empty());
    }

    #[test]
    fn a_diamond_joins_two_definitions() {
        // 0 → {1, 2} → 3, each arm defines R
        let (mut ssa, mut log) = (Ssa::new(vec![vec![], vec![0], vec![0], vec![1, 2]]), Log::default());
        let (a, b) = (log.value(), log.value());
        for blk in 0..4 {
            ssa.seal(&mut log, blk);
        }
        ssa.write(1, R, a);
        ssa.write(2, R, b);
        let j = ssa.read(&mut log, 3, R);
        assert_eq!(log.live_phis(), vec![j]);
        assert_eq!(log.incoming, vec![(j, a, 1), (j, b, 2)]);
    }

    #[test]
    fn one_arm_defining_still_joins_with_the_value_from_before() {
        let (mut ssa, mut log) = (Ssa::new(vec![vec![], vec![0], vec![0], vec![1, 2]]), Log::default());
        let (a, b) = (log.value(), log.value());
        for blk in 0..4 {
            ssa.seal(&mut log, blk);
        }
        ssa.write(0, R, a);
        ssa.write(1, R, b);
        let j = ssa.read(&mut log, 3, R);
        assert_eq!(log.incoming, vec![(j, b, 1), (j, a, 2)]);
    }

    #[test]
    fn a_loop_that_changes_the_register_keeps_its_phi() {
        // 0 → 1 (head) → 2 (body) → 1, 1 → 3 (exit); the body increments R
        let (mut ssa, mut log) = (Ssa::new(vec![vec![], vec![0, 2], vec![1], vec![1]]), Log::default());
        let a = log.value();
        ssa.seal(&mut log, 0);
        ssa.write(0, R, a);
        let head = ssa.read(&mut log, 1, R); // head not sealed yet: incomplete phi
        ssa.seal(&mut log, 2);
        let body_in = ssa.read(&mut log, 2, R);
        assert_eq!(body_in, head);
        let inc = log.value();
        ssa.write(2, R, inc);
        ssa.seal(&mut log, 1); // back edge known
        ssa.seal(&mut log, 3);
        assert_eq!(ssa.read(&mut log, 3, R), head);
        assert_eq!(log.live_phis(), vec![head]);
        assert_eq!(log.incoming, vec![(head, a, 0), (head, inc, 2)]);
    }

    #[test]
    fn a_loop_that_leaves_the_register_alone_drops_its_phi() {
        let (mut ssa, mut log) = (Ssa::new(vec![vec![], vec![0, 2], vec![1], vec![1]]), Log::default());
        let a = log.value();
        ssa.seal(&mut log, 0);
        ssa.write(0, R, a);
        let head = ssa.read(&mut log, 1, R);
        ssa.seal(&mut log, 2);
        let _ = ssa.read(&mut log, 2, R);
        ssa.seal(&mut log, 1);
        ssa.seal(&mut log, 3);
        assert_eq!(log.replaced, vec![(head, a)]);
        assert!(log.live_phis().is_empty());
        assert_eq!(ssa.read(&mut log, 3, R), a, "reads after the removal see the replacement");
    }

    #[test]
    fn removing_a_phi_cascades_to_the_phis_built_on_it() {
        // nested loops that never change R: inner head 2's phi uses outer head 1's phi
        // 0 → 1 → 2 → 3 → 2, 3 → 4 → 1, 4 → 5
        let preds = vec![vec![], vec![0, 4], vec![1, 3], vec![2], vec![3], vec![4]];
        let (mut ssa, mut log) = (Ssa::new(preds), Log::default());
        let a = log.value();
        ssa.seal(&mut log, 0);
        ssa.write(0, R, a);
        let outer = ssa.read(&mut log, 1, R);
        let inner = ssa.read(&mut log, 2, R);
        ssa.seal(&mut log, 3);
        let _ = ssa.read(&mut log, 3, R);
        ssa.seal(&mut log, 2);
        ssa.seal(&mut log, 4);
        let _ = ssa.read(&mut log, 4, R);
        ssa.seal(&mut log, 1);
        ssa.seal(&mut log, 5);
        assert!(log.live_phis().is_empty(), "both heads' phis go: {:?} {:?}", outer, inner);
        assert_eq!(ssa.read(&mut log, 5, R), a);
    }

    #[test]
    fn delayed_trivial_phi_collapse_transfers_users() {
        // 0 → 1 (outer, [0, 4]) → 2 (inner, [1, 2]), 3 ([2, 0]), 4 ([1]).
        // Sealing 2 collapses phi2 → phi1 while phi3(phi2, a) is not yet trivial;
        // sealing 1 later collapses phi1 → a and must revisit phi3.
        let preds = vec![vec![], vec![0, 4], vec![1, 2], vec![2, 0], vec![1]];
        let (mut ssa, mut log) = (Ssa::new(preds), Log::default());
        let a = log.value();
        ssa.seal(&mut log, 0);
        ssa.write(0, R, a);
        let _phi1 = ssa.read(&mut log, 1, R);
        let _phi2 = ssa.read(&mut log, 2, R);
        ssa.seal(&mut log, 3);
        let phi3 = ssa.read(&mut log, 3, R);
        ssa.seal(&mut log, 2);
        ssa.seal(&mut log, 4);
        let _ = ssa.read(&mut log, 4, R);
        ssa.seal(&mut log, 1);
        assert!(log.live_phis().is_empty(), "live phis left: {:?}", log.live_phis());
        assert_eq!(ssa.resolve_value(phi3), a);
        assert_eq!(ssa.read(&mut log, 3, R), a);

        // Same cascade when users[phi2] lists [phi3, phi1] (same == phi1 collapses second).
        let preds = vec![vec![], vec![0, 2], vec![1, 2], vec![2, 0]];
        let (mut ssa, mut log) = (Ssa::new(preds), Log::default());
        let a = log.value();
        ssa.seal(&mut log, 0);
        ssa.write(0, R, a);
        let _phi1 = ssa.read(&mut log, 1, R);
        let _phi2 = ssa.read(&mut log, 2, R);
        ssa.seal(&mut log, 3);
        let phi3 = ssa.read(&mut log, 3, R); // users[phi2] = [phi3]
        ssa.seal(&mut log, 1); // phi1 = phi(a, phi2), users[phi2] = [phi3, phi1]
        ssa.seal(&mut log, 2); // phi2 → phi1 cascades phi1 → a, which must collapse phi3 → a
        assert!(log.live_phis().is_empty(), "live phis left after [phi3, phi1] cascade: {:?}", log.live_phis());
        assert_eq!(ssa.resolve_value(phi3), a);
    }

    #[test]
    fn mutual_phi_cycle_collapse_returns_resolved_value() {
        // 0 → 1 ↔ 2 (all sealed): reading 2 first creates phi2, recurses into phi1(a, phi2),
        // then collapses phi2 → phi1 which cascades phi1 → a; read(2) must return a, not stale phi1.
        let preds = vec![vec![], vec![0, 2], vec![1, 2]];
        let (mut ssa, mut log) = (Ssa::new(preds), Log::default());
        let a = log.value();
        for b in 0..3 {
            ssa.seal(&mut log, b);
        }
        ssa.write(0, R, a);
        assert_eq!(ssa.read(&mut log, 2, R), a);
        assert!(log.live_phis().is_empty(), "live phis left: {:?}", log.live_phis());
    }

    #[test]
    fn unsealed_zero_pred_block_replaces_incomplete_phi_with_undef() {
        // Block 0 has 0 preds and is read before sealing; block 2 joins block 0 with sealed 0-pred block 1.
        let preds = vec![vec![], vec![], vec![0, 1]];
        let (mut ssa, mut log) = (Ssa::new(preds), Log::default());
        let p0 = ssa.read(&mut log, 0, R);
        ssa.seal(&mut log, 1);
        ssa.seal(&mut log, 2);
        let p2 = ssa.read(&mut log, 2, R);
        ssa.seal(&mut log, 0);
        assert!(log.live_phis().is_empty(), "0-pred phi stayed live: {:?}", log.live_phis());
        assert_eq!(ssa.resolve_value(p0), 0);
        assert_eq!(ssa.resolve_value(p2), 0);
        assert_eq!(ssa.read(&mut log, 0, R), 0);
    }

    #[test]
    fn sealed_single_pred_cycle_and_deep_chain_do_not_overflow() {
        // Unreachable 1-pred self-loop
        let (mut ssa, mut log) = (Ssa::new(vec![vec![0]]), Log::default());
        ssa.seal(&mut log, 0);
        assert_eq!(ssa.read(&mut log, 0, R), 0);
        assert!(log.phis.is_empty());

        // Unreachable 1-pred 2-cycle with stem
        let (mut ssa, mut log) = (Ssa::new(vec![vec![1], vec![0], vec![0]]), Log::default());
        for b in 0..3 {
            ssa.seal(&mut log, b);
        }
        assert_eq!(ssa.read(&mut log, 2, R), 0);
        assert!(log.phis.is_empty());

        // Deep sealed linear chain (20,000 blocks)
        let n = 20_000usize;
        let mut preds = Vec::with_capacity(n);
        preds.push(vec![]);
        for b in 1..n {
            preds.push(vec![b - 1]);
        }
        let (mut ssa, mut log) = (Ssa::new(preds), Log::default());
        let a = log.value();
        for b in 0..n {
            ssa.seal(&mut log, b);
        }
        ssa.write(0, R, a);
        assert_eq!(ssa.read(&mut log, n - 1, R), a);
        assert!(log.phis.is_empty());
    }
}
