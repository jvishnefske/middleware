//! Allocation: binding a compiled master recipe ([`Sfc`]) to units, which
//! turns it into a control recipe.
//!
//! Each unit advertises its equipment phases ([`UnitPhase`]: equipment
//! module, phase name, phase class, and the range it accepts for each class
//! parameter, which may be narrower than the class's). Allocation chooses
//! one unit per unit procedure and one equipment phase per step:
//!
//! 1. **Candidates.** A unit can host a unit procedure when it is online,
//!    has the unit class the procedure asks for (if any), and for every
//!    step of the procedure has an equipment phase of the step's class
//!    whose ranges hold the step's arguments. Among several such phases
//!    the one with the smallest (equipment module, phase name) wins, by
//!    byte order; two steps that may run at the same time
//!    ([`Sfc::concurrent`]) never share a phase, so a later concurrent step
//!    takes the next one. A unit procedure with no steps needs no unit.
//! 2. **Exclusive units.** A unit hosts at most one unit procedure of a
//!    recipe (an ISA-88 unit runs one unit procedure at a time). Units are
//!    tried in byte order of their ids; the first assignment in that order
//!    (unit procedure 0's unit first, then 1's, ...) wins, found by an
//!    iterative depth-first search bounded by [`SEARCH_BOUND`] moves.
//!
//! The result depends only on the ids and capabilities, never on the order
//! units were listed in. Failures say why: [`AllocError::NoUnit`] names the
//! first unit procedure no unit can host, and [`explain`] gives each unit's
//! first [`Reject`]ion reason for it; [`AllocError::Contention`] means
//! every procedure has candidates but not enough distinct units.

use crate::recipe::{PhaseClass, Sfc};
use crate::{MAX_PARAMS, MAX_STEPS, MAX_UNIT_PHASES, MAX_UNIT_PROCS, MAX_UNITS, bit};

/// Most moves of the assignment search: twice the nodes of a full search
/// tree of [`MAX_UNITS`] choices over [`MAX_UNIT_PROCS`] levels, rounded up.
pub const SEARCH_BOUND: usize = 10_000;

/// One equipment phase a unit offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitPhase<'a> {
    /// The equipment module's id.
    pub em: &'a str,
    /// The phase's name within the module.
    pub name: &'a str,
    /// The phase class it implements.
    pub class: &'a str,
    /// The accepted `(min, max)` per class parameter, in class order.
    pub ranges: &'a [(i32, i32)],
}

/// A unit as allocation sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unit<'a> {
    /// The unit's id (unique; the tie-break order).
    pub id: &'a str,
    /// Its unit class.
    pub class: &'a str,
    /// Whether discovery currently sees it.
    pub online: bool,
    /// Its equipment phases.
    pub phases: &'a [UnitPhase<'a>],
}

/// An equipment phase: unit index and phase index, both into the lists
/// given to [`allocate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PhaseRef {
    /// Index into the units.
    pub unit: u8,
    /// Index into that unit's phases.
    pub phase: u8,
}

/// A control recipe's binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    /// Each unit procedure's unit index, or `None` for a procedure with
    /// no steps.
    pub up_unit: [Option<u8>; MAX_UNIT_PROCS],
    /// Each step's equipment phase.
    pub step_phase: [PhaseRef; MAX_STEPS],
}

/// Why a unit cannot host a unit procedure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// Discovery does not see it.
    Offline,
    /// It is not of the unit class the procedure asks for.
    UnitClass,
    /// It has no phase of this step's class.
    MissingClass {
        /// The step.
        step: usize,
    },
    /// Its phases of the step's class do not accept this argument (the
    /// first such phase's range is reported).
    OutOfRange {
        /// The step.
        step: usize,
        /// The parameter index.
        param: usize,
        /// The step's value.
        value: i32,
        /// The phase's low end.
        min: i32,
        /// The phase's high end.
        max: i32,
    },
    /// Every suitable phase is taken by a step that may run at the same
    /// time.
    PhaseBusy {
        /// The step.
        step: usize,
    },
}

/// Why allocation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    /// More than [`MAX_UNITS`] units, or more than [`MAX_UNIT_PHASES`]
    /// phases on one.
    TooMany,
    /// No unit can host this unit procedure ([`explain`] says why).
    NoUnit {
        /// The unit procedure.
        up: usize,
    },
    /// Every procedure has candidates but they cannot all get distinct
    /// units; the mask has a bit per contending procedure.
    Contention {
        /// The unit procedures with steps.
        ups: u8,
    },
}

/// A phase's ranges hold the step's arguments, or the first violation.
fn fits(
    sfc: &Sfc,
    class: &PhaseClass<'_>,
    step: usize,
    phase: &UnitPhase<'_>,
) -> Result<(), Reject> {
    for pi in 0..class.params.len().min(MAX_PARAMS) {
        let value = sfc.args[step][pi];
        let (min, max) = phase.ranges.get(pi).copied().unwrap_or((1, 0));
        if value < min || value > max {
            return Err(Reject::OutOfRange {
                step,
                param: pi,
                value,
                min,
                max,
            });
        }
    }
    Ok(())
}

/// For each step of unit procedure `up`, the phase unit `unit` would run it
/// on, or the first reason it cannot.
fn place(
    sfc: &Sfc,
    classes: &[PhaseClass<'_>],
    up: usize,
    required_class: Option<&str>,
    unit: &Unit<'_>,
) -> Result<[u8; MAX_STEPS], Reject> {
    if !unit.online {
        return Err(Reject::Offline);
    }
    if required_class.is_some_and(|c| c != unit.class) {
        return Err(Reject::UnitClass);
    }
    let mut chosen = [u8::MAX; MAX_STEPS];
    for step in 0..sfc.steps {
        if usize::from(sfc.step_up[step]) != up {
            continue;
        }
        let class = classes
            .get(usize::from(sfc.step_class[step]))
            .ok_or(Reject::MissingClass { step })?;
        let mut first_misfit = None;
        let mut any_class = false;
        let mut best: Option<usize> = None;
        for (pi, p) in unit.phases.iter().enumerate() {
            if p.class != class.name {
                continue;
            }
            any_class = true;
            if let Err(r) = fits(sfc, class, step, p) {
                first_misfit = first_misfit.or(Some(r));
                continue;
            }
            let busy =
                (0..step).any(|other| chosen[other] == pi as u8 && sfc.concurrent(step, other));
            let better = best.is_none_or(|b| {
                let q = &unit.phases[b];
                (p.em.as_bytes(), p.name.as_bytes()) < (q.em.as_bytes(), q.name.as_bytes())
            });
            if !busy && better {
                best = Some(pi);
            }
        }
        match best {
            Some(pi) => chosen[step] = pi as u8,
            None if !any_class => return Err(Reject::MissingClass { step }),
            None => return Err(first_misfit.unwrap_or(Reject::PhaseBusy { step })),
        }
    }
    Ok(chosen)
}

/// Units' indices sorted by id (insertion sort over at most [`MAX_UNITS`]).
fn by_id(units: &[Unit<'_>]) -> [u8; MAX_UNITS] {
    let mut order = [0u8; MAX_UNITS];
    for (i, slot) in order.iter_mut().enumerate() {
        *slot = i as u8;
    }
    let n = units.len().min(MAX_UNITS);
    for i in 1..n {
        let mut j = i;
        while j > 0
            && units[usize::from(order[j])].id.as_bytes()
                < units[usize::from(order[j - 1])].id.as_bytes()
        {
            order.swap(j, j - 1);
            j -= 1;
        }
    }
    order
}

/// Binds `sfc` (compiled from a recipe whose unit procedures' unit-class
/// constraints are `up_class`) to `units`; see the module docs.
pub fn allocate(
    sfc: &Sfc,
    classes: &[PhaseClass<'_>],
    up_class: &[Option<&str>],
    units: &[Unit<'_>],
) -> Result<Allocation, AllocError> {
    if units.len() > MAX_UNITS || units.iter().any(|u| u.phases.len() > MAX_UNIT_PHASES) {
        return Err(AllocError::TooMany);
    }
    let order = by_id(units);
    let n_up = sfc.unit_procs.min(MAX_UNIT_PROCS);
    // candidates[up]: bit k = the k-th unit in id order can host it.
    let mut candidates = [0u32; MAX_UNIT_PROCS];
    let mut placed = [[u8::MAX; MAX_STEPS]; MAX_UNIT_PROCS * MAX_UNITS];
    let mut needs = 0u8;
    for up in 0..n_up {
        if (0..sfc.steps).all(|s| usize::from(sfc.step_up[s]) != up) {
            continue;
        }
        needs |= 1 << up;
        for (k, &ui) in order.iter().take(units.len()).enumerate() {
            let class = up_class.get(up).copied().flatten();
            if let Ok(p) = place(sfc, classes, up, class, &units[usize::from(ui)]) {
                candidates[up] |= bit(k);
                placed[up * MAX_UNITS + k] = p;
            }
        }
        if candidates[up] == 0 {
            return Err(AllocError::NoUnit { up });
        }
    }
    // Exclusive assignment: iterative depth-first search in id order.
    let mut pick = [usize::MAX; MAX_UNIT_PROCS];
    let mut next = [0usize; MAX_UNIT_PROCS];
    let mut used = 0u32;
    let mut level = 0usize;
    let mut found = false;
    for _ in 0..SEARCH_BOUND {
        if level == n_up {
            found = true;
            break;
        }
        if needs & (1 << level) == 0 {
            level += 1;
            continue;
        }
        let free = candidates[level] & !used;
        match (next[level]..units.len()).find(|&k| free & bit(k) != 0) {
            Some(k) => {
                pick[level] = k;
                used |= bit(k);
                next[level] = k + 1;
                level += 1;
            }
            None => {
                // Back up to the previous procedure that holds a unit.
                let mut back = None;
                for l in (0..level).rev() {
                    if needs & (1 << l) != 0 {
                        back = Some(l);
                        break;
                    }
                }
                match back {
                    None => return Err(AllocError::Contention { ups: needs }),
                    Some(l) => {
                        used &= !bit(pick[l]);
                        pick[l] = usize::MAX;
                        for m in next.iter_mut().skip(l + 1) {
                            *m = 0;
                        }
                        level = l;
                    }
                }
            }
        }
    }
    if !found {
        return Err(AllocError::Contention { ups: needs });
    }
    let mut a = Allocation {
        up_unit: [None; MAX_UNIT_PROCS],
        step_phase: [PhaseRef::default(); MAX_STEPS],
    };
    for up in 0..n_up {
        if needs & (1 << up) == 0 {
            continue;
        }
        let k = pick[up];
        let unit = order[k];
        a.up_unit[up] = Some(unit);
        let p = &placed[up * MAX_UNITS + k];
        for (s, slot) in a.step_phase.iter_mut().enumerate().take(sfc.steps) {
            if usize::from(sfc.step_up[s]) == up {
                *slot = PhaseRef { unit, phase: p[s] };
            }
        }
    }
    Ok(a)
}

/// Why each unit (in the order given) cannot host unit procedure `up`, or
/// `None` where it can: the reasons behind [`AllocError::NoUnit`].
pub fn explain(
    sfc: &Sfc,
    classes: &[PhaseClass<'_>],
    up_class: &[Option<&str>],
    units: &[Unit<'_>],
    up: usize,
) -> [Option<Reject>; MAX_UNITS] {
    let mut reasons = [None; MAX_UNITS];
    let class = up_class.get(up).copied().flatten();
    for (slot, unit) in reasons.iter_mut().zip(units) {
        *slot = place(sfc, classes, up, class, unit).err();
    }
    reasons
}

#[cfg(test)]
mod tests;
