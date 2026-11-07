//! Play skins with a presentation program, built by hand: callbacks
//! appended with their own constants, reads and gating, and consumers
//! compiled against the program's outputs.

#![allow(dead_code)]

use skin_model::present::{CallbackDef, Conv, Insn, OutTy, PresentDef, ReadDef, Val};
use skin_model::{
    ElementDef, Fit, LoadError, PLAY, Program, Scope, Skin, SkinDef, Ty, WidgetDefs, compile, field,
};

use crate::play_support::REGIONS;

/// An expression over the play schema and `sig`'s outputs.
pub fn expr(src: &str, sig: &[OutTy], ty: Ty) -> Program {
    compile(src, &Scope::new(&PLAY).outputs(sig), ty).unwrap()
}

/// A read of `src`, of type `ty`, under `guard`.
pub fn read(src: &str, ty: Ty, guard: Option<&str>) -> ReadDef {
    ReadDef {
        value: expr(src, &[], ty),
        guard: guard.map(|g| expr(g, &[], Ty::Bool)),
    }
}

/// Output `k`'s slot in the play's fields.
pub fn slot(k: usize) -> usize {
    field::COUNT + 2 * k
}

/// One callback for [`Builder::push`].
pub struct Cb {
    pub code: Vec<Insn>,
    pub consts: Vec<Val>,
    pub reads: Vec<ReadDef>,
    pub conv: Conv,
    pub gate: Option<ReadDef>,
    pub after: Option<u16>,
}

impl Cb {
    /// A callback returning constant `v`.
    pub fn ret(v: Val, conv: Conv) -> Self {
        Self {
            code: vec![Insn::Const { dst: 0, k: 0 }, Insn::Ret { src: 0 }],
            consts: vec![v],
            reads: Vec::new(),
            conv,
            gate: None,
            after: None,
        }
    }

    /// A callback running `code` over `consts` and `reads`.
    pub fn code(code: Vec<Insn>, consts: Vec<Val>, reads: Vec<ReadDef>, conv: Conv) -> Self {
        Self {
            code,
            consts,
            reads,
            conv,
            gate: None,
            after: None,
        }
    }

    pub fn gate(self, gate: ReadDef) -> Self {
        Self {
            gate: Some(gate),
            ..self
        }
    }

    pub fn after(self, k: u16) -> Self {
        Self {
            after: Some(k),
            ..self
        }
    }
}

/// A program built callback by callback.
#[derive(Default)]
pub struct Builder(pub PresentDef);

impl Builder {
    /// Appends `cb`, its indices shifted past what came before; its index.
    pub fn push(&mut self, cb: Cb) -> u16 {
        let d = &mut self.0;
        let (k_off, r_off) = (d.consts.len() as u16, d.reads.len() as u16);
        let start = d.code.len() as u32;
        let len = cb.code.len() as u16;
        d.code.extend(cb.code.into_iter().map(|i| match i {
            Insn::Const { dst, k } => Insn::Const { dst, k: k + k_off },
            Insn::Read { dst, read } => Insn::Read {
                dst,
                read: read + r_off,
            },
            other => other,
        }));
        d.consts.extend(cb.consts);
        d.reads.extend(cb.reads);
        let gate = cb.gate.map(|g| {
            d.reads.push(g);
            (d.reads.len() - 1) as u16
        });
        d.callbacks.push(CallbackDef {
            start,
            len,
            regs: 8,
            conv: cb.conv,
            gate,
            after: cb.after,
        });
        d.signature.push(OutTy::of(cb.conv));
        (d.callbacks.len() - 1) as u16
    }

    /// Callback `k` sharing callback `of`'s span: another object of the
    /// same closure.
    pub fn share(&mut self, of: u16) -> u16 {
        let d = &mut self.0;
        let c = d.callbacks[usize::from(of)];
        d.callbacks.push(c);
        d.signature.push(OutTy::of(c.conv));
        (d.callbacks.len() - 1) as u16
    }

    pub fn signature(&self) -> Vec<OutTy> {
        self.0.signature.clone()
    }
}

/// A play skin of `elements` and `widgets` over the widget sheet with
/// program `present`.
pub fn load(
    present: Option<PresentDef>,
    whens: Vec<Program>,
    elements: Vec<ElementDef>,
    widgets: WidgetDefs,
) -> Result<Skin, LoadError> {
    Skin::new(SkinDef {
        fit: Fit::Adapt,
        regions: REGIONS,
        whens,
        sheet: crate::widget_support::sheet(),
        lanes: None,
        elements,
        widgets,
        bomb_up_to: Some(skin_model::Grade::Bad),
        close: None,
        present,
    })
}
