use alloy_primitives::{Address, U256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Call,
    CallCode,
    Create,
    Create2,
    SelfDestruct,
}

#[derive(Debug, Clone)]
pub struct FrameMove {
    pub from: Address,
    pub to: Address,
    pub value: U256,
    pub kind: FrameKind,
}

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub from: Address,
    pub to: Address,
    pub value: U256,
    pub kind: FrameKind,
    pub trace_path: String,
}

/// Pure capture buffer. `push_frame` is called by the revm Inspector glue
/// (Step 5) for every value-bearing sub-call; DELEGATECALL/STATICCALL never
/// reach here because they carry no transferable value.
///
/// Reverted (sub)call/create frames are *discarded* (see C1 / issue #2):
/// revm rolls back the state of a reverted (sub)call, so the committed
/// `BundleState` delta excludes that value. A frame captured on call/create
/// entry (or via the `selfdestruct` hook) that turns out to belong to a
/// reverted subtree must NOT be retained — otherwise it is summed into the
/// reconciliation totals and produces a spurious residual ⇒ a permanent
/// false stall on an entirely valid block. `call_in`/`create_in` snapshot
/// the captured-frame count on frame entry; `frame_end` pops that snapshot
/// and, if the frame reverted/halted, truncates every frame captured at or
/// below it (the whole reverted subtree).
#[derive(Default, Clone)]
pub struct ValueInspector {
    seq: u64,
    frames: Vec<CapturedFrame>,
    /// `frames.len()` snapshot at each open call/create entry (matched
    /// 1:1 with `call_in`/`create_in` ↔ `frame_end`). A reverted frame
    /// truncates `frames` back to its snapshot, dropping its whole
    /// (reverted) subtree.
    revert_marks: Vec<usize>,
}

impl ValueInspector {
    pub fn push_frame(&mut self, m: FrameMove) {
        if m.value.is_zero() {
            return;
        }
        let trace_path = self.seq.to_string();
        self.seq += 1;
        self.frames.push(CapturedFrame {
            from: m.from,
            to: m.to,
            value: m.value,
            kind: m.kind,
            trace_path,
        });
    }
    pub fn into_frames(self) -> Vec<CapturedFrame> {
        self.frames
    }

    /// Record a call/create frame entry: snapshot the current captured
    /// frame count so a later revert of this frame can drop its subtree.
    pub fn enter_frame(&mut self) {
        self.revert_marks.push(self.frames.len());
    }

    /// Record a call/create frame exit. If `reverted` is true, every
    /// frame captured at/under this frame (revm rolls back the whole
    /// subtree's state) is discarded so it is never summed into
    /// reconciliation.
    pub fn exit_frame(&mut self, reverted: bool) {
        if let Some(mark) = self.revert_marks.pop() {
            if reverted && self.frames.len() > mark {
                self.frames.truncate(mark);
            }
        }
    }

    /// Number of frames captured so far (used by the ExEx re-execution
    /// wrapper to detect whether a given `call`/`create`/`selfdestruct`
    /// hook produced a value-bearing frame).
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }
}

// ── revm 38 `Inspector` trait glue ─────────────────────────────────────────
//
// Implemented against the pinned `revm = 38.0.0` stack
// (`revm-inspector 19.0.0`, `revm-interpreter 35.0.1`,
// `revm-context-interface 17.0.1`). See `docs/reth-pin.md`
// ("revm 38 API deltas") for exactly how this differed from the plan
// candidate. Only real, transferable value is captured: `CallValue::Transfer`
// (never `CallValue::Apparent`, which is what DELEGATECALL/STATICCALL carry).
use revm::context_interface::CreateScheme;
use revm::interpreter::{
    CallInputs, CallOutcome, CallScheme, CallValue, CreateInputs, CreateOutcome,
};
use revm::Inspector;

impl<CTX> Inspector<CTX> for ValueInspector {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        // Snapshot BEFORE recording this frame's own value so a revert of
        // this very call drops its value too (only gas was charged).
        self.enter_frame();
        if let CallValue::Transfer(v) = inputs.value {
            if !v.is_zero() {
                let kind = match inputs.scheme {
                    CallScheme::CallCode => FrameKind::CallCode,
                    _ => FrameKind::Call,
                };
                self.push_frame(FrameMove {
                    from: inputs.caller,
                    to: inputs.target_address,
                    value: v,
                    kind,
                });
            }
        }
        None
    }

    fn call_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        // A non-`is_ok` instruction result == revert/halt; revm rolls back
        // the whole subtree's state, so its captured frames never
        // committed and must be dropped from reconciliation (issue #2).
        let reverted = !outcome.instruction_result().is_ok();
        self.exit_frame(reverted);
    }

    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.enter_frame();
        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        let value = inputs.value();
        if !value.is_zero() {
            if let Some(addr) = outcome.address {
                let kind = match inputs.scheme() {
                    CreateScheme::Create2 { .. } => FrameKind::Create2,
                    _ => FrameKind::Create,
                };
                self.push_frame(FrameMove {
                    from: inputs.caller(),
                    to: addr,
                    value,
                    kind,
                });
            }
        }
        // `outcome.address == None` already guards the value push above on
        // a reverted CREATE, but nested frames captured inside a reverted
        // CREATE subtree must still be dropped — mirror call_end.
        let reverted = !outcome.instruction_result().is_ok();
        self.exit_frame(reverted);
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        if !value.is_zero() {
            self.push_frame(FrameMove {
                from: contract,
                to: target,
                value,
                kind: FrameKind::SelfDestruct,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};

    #[test]
    fn records_value_call_and_assigns_trace_path() {
        let mut insp = ValueInspector::default();
        insp.push_frame(FrameMove {
            from: Address::repeat_byte(1),
            to: Address::repeat_byte(2),
            value: U256::from(10),
            kind: FrameKind::Call,
        });
        insp.push_frame(FrameMove {
            from: Address::repeat_byte(1),
            to: Address::repeat_byte(3),
            value: U256::ZERO,
            kind: FrameKind::Call,
        }); // zero -> dropped
        let frames = insp.into_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].trace_path, "0");
        assert_eq!(frames[0].value, U256::from(10));
    }

    fn mv(to: u8, v: u64) -> FrameMove {
        FrameMove {
            from: Address::repeat_byte(1),
            to: Address::repeat_byte(to),
            value: U256::from(v),
            kind: FrameKind::Call,
        }
    }

    #[test]
    fn reverted_frame_is_dropped_with_its_subtree() {
        // Outer call captures a frame, makes a nested call that captures
        // another frame, then the outer call REVERTS — both must vanish
        // (revm rolls back the whole subtree). Issue #2.
        let mut insp = ValueInspector::default();
        insp.enter_frame(); // outer call entry
        insp.push_frame(mv(2, 10)); // outer's own value transfer
        insp.enter_frame(); // nested call entry
        insp.push_frame(mv(3, 5)); // nested value transfer
        insp.exit_frame(false); // nested succeeds (kept for now)
        insp.exit_frame(true); // OUTER reverts ⇒ drop subtree
        assert!(
            insp.into_frames().is_empty(),
            "all frames under a reverted call must be dropped"
        );
    }

    #[test]
    fn only_reverted_inner_frame_dropped_outer_kept() {
        // Successful outer tx with a single reverted internal call: only
        // the inner reverted frame is dropped; the outer survives.
        let mut insp = ValueInspector::default();
        insp.enter_frame(); // top-level (successful tx)
        insp.push_frame(mv(2, 100)); // genuine top-level transfer
        insp.enter_frame(); // internal call
        insp.push_frame(mv(3, 7)); // internal transfer (will revert)
        insp.exit_frame(true); // internal call REVERTS ⇒ drop frame to 0x3
        insp.exit_frame(false); // top-level succeeds
        let frames = insp.into_frames();
        assert_eq!(frames.len(), 1, "only the surviving top-level frame");
        assert_eq!(frames[0].to, Address::repeat_byte(2));
        assert_eq!(frames[0].value, U256::from(100));
    }

    #[test]
    fn successful_frames_all_retained() {
        let mut insp = ValueInspector::default();
        insp.enter_frame();
        insp.push_frame(mv(2, 1));
        insp.enter_frame();
        insp.push_frame(mv(3, 2));
        insp.exit_frame(false);
        insp.exit_frame(false);
        assert_eq!(insp.into_frames().len(), 2);
    }
}
