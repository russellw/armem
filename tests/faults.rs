//! Tests that the models report a fault only in an execution that can
//! happen.

use armem::{Model, Options, litmus, run};

/// P1 takes a lock with a compare-and-swap, then writes x. P0 reads x and,
/// if it is 1, takes the lock too and faults on a null pointer. Under every
/// model P0's compare-and-swap then fails, since P1 holds the lock. The ARM
/// model may still let P1 promise x=1 and P0 read it, take the lock and
/// fault; but then P1 cannot take the lock to fulfil its promise, so no
/// execution faults.
const LOCK_THEN_FAULT: &str = "AArch64 lock-then-fault
{ 0:X1=x; 0:X3=l; 1:X1=x; 1:X3=l; }
 P0              | P1              ;
 LDR W0,[X1]     | MOV W4,#0       ;
 CBZ W0,E0       | MOV W5,#1       ;
 MOV W4,#0       | CAS W4,W5,[X3]  ;
 MOV W5,#2       | CBNZ W4,E1      ;
 CAS W4,W5,[X3]  | MOV W2,#1       ;
 CBNZ W4,E0      | STR W2,[X1]     ;
 MOV X7,#0       | E1:             ;
 LDR W6,[X7]     |                 ;
 E0:             |                 ;
exists (0:X0=1)
";

#[test]
fn no_fault_from_an_unfulfillable_promise() {
    let t = litmus::parse(LOCK_THEN_FAULT).unwrap();
    for m in [Model::Arm, Model::Tso, Model::Sc] {
        let r = run(&t, m, &Options::default());
        assert!(r.faults.is_empty(), "{}: {:?}", m.name(), r.faults);
        assert!(r.observed(&t).unwrap(), "{}", m.name());
    }
}

/// Without P1's compare-and-swap, P0 can take the lock after reading x=1
/// and fault, under every model.
#[test]
fn fault_after_a_fulfilled_promise() {
    let src = LOCK_THEN_FAULT.replace(" CAS W4,W5,[X3]  ;", " NOP             ;").replace(" CBNZ W4,E1      ;", " NOP             ;");
    let t = litmus::parse(&src).unwrap();
    for m in [Model::Arm, Model::Tso, Model::Sc] {
        let r = run(&t, m, &Options::default());
        assert_eq!(r.faults.len(), 1, "{}: {:?}", m.name(), r.faults);
    }
}
