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
#[derive(Default, Clone)]
pub struct ValueInspector {
    seq: u64,
    frames: Vec<CapturedFrame>,
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
}
